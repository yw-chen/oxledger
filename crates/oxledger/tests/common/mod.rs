// SPDX-License-Identifier: GPL-2.0-or-later
//! Shared helpers for the PostgreSQL integration tests.
//!
//! Which database a test uses (docs/impl/testing-playbook.md):
//!
//! - server: the one in `OXLEDGER_TEST_DATABASE_URL` if set, otherwise the
//!   test-only container `oxledger-pg-test` on 127.0.0.1:55433
//!   (`scripts/test-pg.ps1`), never the development `oxledger-pg`;
//! - name: the database in `OXLEDGER_TEST_DATABASE_URL` if set, otherwise
//!   `OXLEDGER_TEST_DB_PREFIX` (default `oxledger_test`);
//! - under nextest, `_<NEXTEST_TEST_GLOBAL_SLOT>` is appended: the slot is
//!   unique among the tests running at the same moment, so each runs on its
//!   own slot database and the usual TRUNCATE reset stays (no DROP DATABASE
//!   per test, which holds an xid across a checkpoint and stalls every
//!   `sync_all` on the cluster, G21).
//!
//! Scratch databases (upgrade tests and the like) are named after the slot
//! database, so they follow the prefix too.
#![allow(dead_code)]

pub mod snap;

use oxledger::db;
use sqlx::PgPool;

/// The test container from `scripts/test-pg.ps1`.
const DEFAULT_SERVER: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55433";
const DEFAULT_PREFIX: &str = "oxledger_test";
/// Connections per test pool unless `OXLEDGER_DB_MAX_CONNECTIONS` says
/// otherwise. Times the nextest slots this must stay well below the test
/// container's `max_connections` (D10).
pub const TEST_MAX_CONNECTIONS: u32 = 4;

/// Where this test's database lives. Build URLs with the methods, not from
/// `base`: they keep the `?query` of `OXLEDGER_TEST_DATABASE_URL`.
#[derive(Clone, Debug)]
pub struct TestDb {
    /// `postgres://user:pw@host:port`, no database, no query.
    pub base: String,
    /// The slot database's name.
    pub name: String,
    /// `?...` from `OXLEDGER_TEST_DATABASE_URL`, or empty.
    query: String,
}

impl TestDb {
    /// The slot database.
    pub fn url(&self) -> String {
        self.url_for(&self.name)
    }

    /// The `postgres` maintenance database on the same server.
    pub fn admin_url(&self) -> String {
        self.url_for("postgres")
    }

    /// Another database (a scratch one) on the same server, same query.
    pub fn url_for(&self, name: &str) -> String {
        format!("{}/{name}{}", self.base, self.query)
    }

    /// `<name>_<suffix>`: a scratch database next to the slot database.
    pub fn scratch(&self, suffix: &str) -> String {
        let n = format!("{}_{suffix}", self.name);
        check_ident(&n);
        n
    }
}

fn check_ident(name: &str) {
    assert!(
        !name.is_empty()
            && name.len() <= 63
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "unexpected test database name {name:?}"
    );
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The database this test process uses (see the module docs).
pub fn test_db() -> TestDb {
    let (base, mut name, query) = match env("OXLEDGER_TEST_DATABASE_URL") {
        Some(url) => {
            let (base, rest) = url.rsplit_once('/').expect("database URL has a path");
            let (name, query) = match rest.split_once('?') {
                Some((n, q)) => (n.to_string(), format!("?{q}")),
                None => (rest.to_string(), String::new()),
            };
            (base.to_string(), name, query)
        }
        None => (
            DEFAULT_SERVER.to_string(),
            env("OXLEDGER_TEST_DB_PREFIX").unwrap_or_else(|| DEFAULT_PREFIX.into()),
            String::new(),
        ),
    };
    if let Some(slot) = env("NEXTEST_TEST_GLOBAL_SLOT") {
        let slot: u32 = slot.parse().expect("NEXTEST_TEST_GLOBAL_SLOT is a number");
        name = format!("{name}_{slot}");
    }
    check_ident(&name);
    TestDb { base, name, query }
}

/// Creates `name` on `db`'s server if it does not exist yet.
pub async fn ensure_database(db: &TestDb, name: &str) {
    check_ident(name);
    let admin = PgPool::connect(&db.admin_url()).await.unwrap_or_else(|e| {
        panic!("PostgreSQL 18 not reachable ({e}); start the test container: scripts/test-pg.ps1")
    });
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(name)
            .fetch_one(&admin)
            .await
            .unwrap();
    if !exists {
        // Checked above to be a plain identifier.
        let create = format!("CREATE DATABASE {name}");
        if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(create))
            .execute(&admin)
            .await
        {
            // Another test process may have created it meanwhile.
            eprintln!("CREATE DATABASE {name}: {e}");
        }
    }
    admin.close().await;
}

/// Connects to this test's slot database (created if missing) and runs
/// migrations. The pool has [`TEST_MAX_CONNECTIONS`] connections unless
/// `OXLEDGER_DB_MAX_CONNECTIONS` is set.
pub async fn test_pool() -> PgPool {
    test_pool_sized(TEST_MAX_CONNECTIONS).await
}

/// [`test_pool`] with `max` connections, for the few tests that hold more
/// than [`TEST_MAX_CONNECTIONS`] at once. `OXLEDGER_DB_MAX_CONNECTIONS`
/// still wins when set.
pub async fn test_pool_sized(max: u32) -> PgPool {
    let db = test_db();
    ensure_database(&db, &db.name).await;
    let opts = if env(db::MAX_CONNECTIONS_ENV).is_some() {
        db::pool_options()
    } else {
        db::pool_options().max_connections(max)
    };
    let pool = db::connect_with(opts, &db.url())
        .await
        .expect("connect test database");
    // A wait that times out reports this database's outbox and the xid holders.
    oxledger::test_support::set_report_pool(&pool);
    pool
}

// Bounded waits (T1b, testing-playbook "等待一律有上限"): every wait in a test
// goes through these or a fault gate (`Gate::reached.notified()`, bounded
// the same way). A timeout panics with what was awaited, the outbox backlog
// and `metrics::describe_xid_holders`.
#[allow(unused_imports)]
pub use oxledger::test_support::{
    POLL_EVERY, WAIT_TIMEOUT, join, panic_text, poll_until, state_report, state_report_for,
    wait_for,
};
#[allow(unused_imports)]
pub use std::ops::ControlFlow::{Break, Continue};

/// Waits until the cluster-wide snapshot horizon H is above `xid`, so the
/// next search or notify round is sure to see the outbox rows of `xid`
/// (03 §4.7: rows at or above H wait for a later round). Tests that stop a
/// single round at a fault gate call this first, or the round may skip
/// their row and never reach the gate (G21).
#[track_caller]
pub fn wait_horizon_past(pool: &PgPool, xid: u64) -> impl std::future::Future<Output = ()> {
    wait_horizon_past_within(pool, xid, WAIT_TIMEOUT)
}

/// [`wait_horizon_past`] with its own bound (tests of the helper).
#[track_caller]
pub fn wait_horizon_past_within(
    pool: &PgPool,
    xid: u64,
    dur: std::time::Duration,
) -> impl std::future::Future<Output = ()> {
    poll_until(
        &format!("the snapshot horizon to pass xid {xid}"),
        dur,
        async move || {
            let h: String =
                sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
                    .fetch_one(pool)
                    .await
                    .unwrap();
            let h: u64 = h.parse().unwrap();
            if h > xid {
                Break(())
            } else {
                Continue(format!("H={h}"))
            }
        },
    )
}

/// Stack for [`run_with_big_stack`]: what `RUST_MIN_STACK` used to give
/// every test thread.
pub const BIG_STACK: usize = 16 * 1024 * 1024;

/// Runs an async test body like `#[tokio::test]` (current-thread runtime,
/// all drivers on), but on its own thread with a [`BIG_STACK`] stack: for
/// the few tests whose debug-build future overflows the default 2 MiB test
/// thread (D13). Panics in `body` fail the calling test.
pub fn run_with_big_stack<F, Fut>(body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let name = std::thread::current()
        .name()
        .unwrap_or("big-stack test")
        .to_string();
    let handle = std::thread::Builder::new()
        .name(name)
        .stack_size(BIG_STACK)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(body())
        })
        .expect("spawn big-stack test thread");
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}
