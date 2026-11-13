// SPDX-License-Identifier: GPL-2.0-or-later
//! G20 (docs/impl/p1-backend-gaps.md): a request dropped while sqlx's
//! `pool.begin()` waits for the server's answer to `BEGIN` left its
//! connection inside a transaction in the pool. The next borrower then got
//! "there is already a transaction in progress" and `read_tx` failed with
//! "SET TRANSACTION ISOLATION LEVEL must be called before any query".
//!
//! `raw_pool_reproduces_the_leak` shows the mechanism on a pool without the
//! fix (sqlx defaults); `pool_options_never_lend_a_connection_in_a_transaction`
//! runs the same cancellations against [`oxledger::db::pool_options`] and
//! counts what its release check caught. Before every cancellation the
//! pool's one connection is back and idle, so the cancellation falls inside
//! `begin()` itself, not inside `acquire()` waiting for the previous release.
//! Needs PostgreSQL 18 (OXLEDGER_TEST_DATABASE_URL).

mod common;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool, Row};
use std::future::Future;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::{Duration, Instant};

const DEFAULT_ADMIN_URL: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger";
const ATTEMPTS_PER_BUDGET: usize = 2;
const BUDGET_STEPS: u64 = 200;

fn url() -> String {
    match std::env::var("OXLEDGER_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            let admin = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_ADMIN_URL.into());
            format!("{}/oxledger_test", admin.rsplit_once('/').unwrap().0)
        }
    }
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
    let mut leaks = 0;
    let mut reproduced = false;
    for budget in budgets() {
        for _ in 0..ATTEMPTS_PER_BUDGET {
            cancel_begin(&pool, budget).await;
            idle(&pool).await;
            let mut c = pool.acquire().await.unwrap();
            if !in_block(&mut c).await {
                continue;
            }
            leaks += 1;
            if !reproduced {
                // The check above was "a query on the leaked connection"
                // (as the auth lookup of the next request); now the read.
                drop(c);
                let e = read_tx_like(&pool).await.expect_err("read_tx must fail");
                assert!(
                    e.to_string().contains(
                        "SET TRANSACTION ISOLATION LEVEL must be called before any query"
                    ),
                    "{e}"
                );
                reproduced = true;
                // The failed read rolled the leaked transaction back.
                idle(&pool).await;
                let mut c = pool.acquire().await.unwrap();
                assert!(!in_block(&mut c).await);
            } else {
                c.close_on_drop();
            }
        }
    }
    eprintln!("raw pool: {leaks} leaked connections");
    assert!(
        reproduced,
        "no cancellation hit the window between BEGIN and its answer"
    );
    pool.close().await;
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
    // The cancellations did hit the window: the release check is what kept
    // those connections out of the pool.
    assert!(caught > 0, "no cancellation hit the window");
    pool.close().await;
}
