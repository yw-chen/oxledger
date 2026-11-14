// SPDX-License-Identifier: GPL-2.0-or-later
//! G20 (docs/impl/p1-backend-gaps.md): a request dropped while sqlx's
//! `pool.begin()` waits for the server's answer to `BEGIN` left its
//! connection inside a transaction in the pool. The next borrower then got
//! "there is already a transaction in progress" and `read_tx` failed with
//! "SET TRANSACTION ISOLATION LEVEL must be called before any query".
//!
//! `raw_pool_reproduces_the_leak` shows the mechanism on a pool without the
//! fix (sqlx defaults), cancelling `begin()` exactly inside the window
//! (`cancel_after_server_began`: the server shows the session inside a
//! transaction, the client has not read the answer).
//! `pool_options_never_lend_a_connection_in_a_transaction` sweeps
//! cancellation points over the first 2 ms against
//! [`oxledger::db::pool_options`] (nothing may be lent inside a
//! transaction), then cancels inside the window three times and checks the
//! release check caught each one. Before every cancellation the pool's one
//! connection is back and idle, so the cancellation falls inside `begin()`
//! itself, not inside `acquire()` waiting for the previous release.
//! Needs PostgreSQL 18 (OXLEDGER_TEST_DATABASE_URL).

mod common;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool, Row};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::{Duration, Instant};

const ATTEMPTS_PER_BUDGET: usize = 2;
const BUDGET_STEPS: u64 = 200;

fn url() -> String {
    common::test_db().url()
}

/// Waits until every connection of `pool` is back and idle (sqlx returns
/// them, and runs the release check, in a spawned task).
async fn idle(pool: &PgPool) {
    let start = Instant::now();
    while pool.size() > 0 && pool.num_idle() < pool.size() as usize {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the pool did not get its connection back"
        );
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
}

/// Polls `pool.begin()` for about `budget`, letting the runtime drive the
/// socket in between, and drops it unfinished (a cancelled request).
async fn cancel_begin(pool: &PgPool, budget: Duration) {
    idle(pool).await;
    let start = Instant::now();
    let mut fut = Box::pin(pool.begin());
    loop {
        // A finished begin drops its transaction here (the normal path).
        let done = std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx).is_ready())).await;
        if done || start.elapsed() >= budget {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// How long [`cancel_after_server_began`] waits, between two polls of
/// `begin()`, for the server to show the session inside a transaction. A
/// poll that has not sent `BEGIN` yet (still pinging the connection) costs
/// this much; one that has, only the server's time to run it (about one
/// round trip, ~30 ms on a loaded machine).
const BEGAN_WITHIN: Duration = Duration::from_secs(1);
/// Attempts of [`cancel_after_server_began`] before the test gives up.
const WINDOW_ATTEMPTS: usize = 10;

/// The server pid of the pool's (one) connection.
async fn backend_pid(pool: &PgPool) -> i32 {
    idle(pool).await;
    let mut c = pool.acquire().await.unwrap();
    let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    drop(c);
    idle(pool).await;
    pid
}

/// Whether the server shows session `pid` inside a transaction block
/// within [`BEGAN_WITHIN`]. Read from another connection; each lookup is
/// its own transaction, so `pg_stat_activity` is current.
async fn began(observer: &PgPool, pid: i32) -> bool {
    let start = Instant::now();
    loop {
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM pg_stat_activity WHERE pid = $1")
                .bind(pid)
                .fetch_optional(observer)
                .await
                .unwrap()
                .flatten();
        if state.as_deref() == Some("idle in transaction") {
            return true;
        }
        if start.elapsed() >= BEGAN_WITHIN {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Drops a `pool.begin()` exactly inside the G20 window: after the server
/// ran its `BEGIN`, before the client read the answer. Between two polls
/// the future is left alone while `observer` watches the server, so an
/// answer that arrived meanwhile stays unread; once the session shows
/// `idle in transaction` the future is dropped. No clock decides where the
/// cancellation falls, so a slow machine only makes it slower (the timed
/// sweep in [`budgets`] stopped reaching `BEGIN` at all under load: the
/// ping before it took longer than the 2 ms the sweep covers).
///
/// `false`: `begin()` finished first (the server took longer than
/// [`BEGAN_WITHIN`] to run `BEGIN`, so the next poll read the answer).
async fn cancel_after_server_began(pool: &PgPool, observer: &PgPool) -> bool {
    let pid = backend_pid(pool).await;
    let mut fut = Box::pin(pool.begin());
    loop {
        let done = std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx).is_ready())).await;
        if done {
            return false;
        }
        if began(observer, pid).await {
            return true;
        }
    }
}

/// [`cancel_after_server_began`] until it lands in the window, at most
/// [`WINDOW_ATTEMPTS`] times.
async fn cancel_in_window(pool: &PgPool, observer: &PgPool) {
    for _ in 0..WINDOW_ATTEMPTS {
        if cancel_after_server_began(pool, observer).await {
            return;
        }
    }
    panic!("begin() always finished before its cancellation ({WINDOW_ATTEMPTS} attempts)");
}

/// A separate pool that watches the tested pool's session.
async fn observer() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .unwrap()
}

/// Cancellation points from 0 to 2 ms after the call, in 10 µs steps.
fn budgets() -> impl Iterator<Item = Duration> {
    (0..BUDGET_STEPS).map(|i| Duration::from_micros(10 * i))
}

/// Whether `c` is inside a transaction block. A simple-protocol query (one
/// message), so the two timestamps are equal outside a block.
async fn in_block(c: &mut PgConnection) -> bool {
    sqlx::raw_sql("SELECT statement_timestamp() <> transaction_timestamp()")
        .fetch_one(&mut *c)
        .await
        .unwrap()
        .get(0)
}

/// What `service::read_tx` does.
async fn read_tx_like(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(())
}

#[tokio::test]
async fn raw_pool_reproduces_the_leak() {
    // The schema is not needed, only a reachable database.
    common::test_pool().await.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .unwrap();
    let observer = observer().await;
    cancel_in_window(&pool, &observer).await;
    idle(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    // "A query on the leaked connection" (as the auth lookup of the next
    // request) runs inside the cancelled request's transaction.
    assert!(
        in_block(&mut c).await,
        "the pool lent the connection outside a transaction: sqlx's begin() no longer leaks?"
    );
    // Now the read.
    drop(c);
    let e = read_tx_like(&pool).await.expect_err("read_tx must fail");
    assert!(
        e.to_string()
            .contains("SET TRANSACTION ISOLATION LEVEL must be called before any query"),
        "{e}"
    );
    // The failed read rolled the leaked transaction back.
    idle(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    assert!(!in_block(&mut c).await);
    drop(c);
    pool.close().await;
    observer.close().await;
}

#[tokio::test]
async fn pool_options_never_lend_a_connection_in_a_transaction() {
    common::test_pool().await.close().await;
    let pool = oxledger::db::pool_options()
        .max_connections(1)
        .connect(&url())
        .await
        .unwrap();
    let caught_before = oxledger::db::RELEASED_IN_TRANSACTION.load(Ordering::SeqCst);
    let mut lent = 0;
    for budget in budgets() {
        for _ in 0..ATTEMPTS_PER_BUDGET {
            cancel_begin(&pool, budget).await;
            idle(&pool).await;
            let mut c = pool.acquire().await.unwrap();
            if in_block(&mut c).await {
                lent += 1;
                c.close_on_drop();
                continue;
            }
            drop(c);
            read_tx_like(&pool).await.unwrap();
        }
    }
    let caught = oxledger::db::RELEASED_IN_TRANSACTION.load(Ordering::SeqCst) - caught_before;
    eprintln!("fixed pool: release check caught {caught}, lent in a transaction {lent}");
    assert_eq!(lent, 0, "connections lent inside a transaction");
    // Cancellations that surely hit the window (the timed sweep above may
    // miss it on a loaded machine): each one is caught by the release
    // check, and the next borrower gets a clean connection.
    let observer = observer().await;
    for n in 1..=3 {
        let before = oxledger::db::RELEASED_IN_TRANSACTION.load(Ordering::SeqCst);
        cancel_in_window(&pool, &observer).await;
        idle(&pool).await;
        let mut c = pool.acquire().await.unwrap();
        assert!(
            !in_block(&mut c).await,
            "cancellation {n}: lent in a transaction"
        );
        drop(c);
        read_tx_like(&pool).await.unwrap();
        assert_eq!(
            oxledger::db::RELEASED_IN_TRANSACTION.load(Ordering::SeqCst) - before,
            1,
            "cancellation {n}: the release check did not catch it"
        );
    }
    pool.close().await;
    observer.close().await;
}
