// SPDX-License-Identifier: GPL-2.0-or-later
//! Helpers for tests and `oxledger seed-demo` only (contract 02 §2 P5).
//!
//! Compiled only with the `test-support` feature, so nothing in a release
//! build can reach them. They write rows directly: no permission check, no
//! workflow, no issue events. Real writes go through the command engine.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Fields of an issue created by [`create_issue`].
pub struct NewIssue<'a> {
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    pub status_id: Uuid,
    pub subject: &'a str,
    pub description: &'a str,
    pub author_id: Uuid,
    pub assigned_to_id: Option<Uuid>,
    pub is_private: bool,
    pub parent_id: Option<Uuid>,
}

/// Inserts an issue with the next project key (D9) and returns (id, key).
pub async fn create_issue(pool: &PgPool, n: NewIssue<'_>) -> Result<(Uuid, String)> {
    let mut tx = pool.begin().await?;
    let (pkey, number): (String, i32) = sqlx::query_as(
        "UPDATE projects SET issue_counter = issue_counter + 1 WHERE id = $1 RETURNING key, issue_counter",
    )
    .bind(n.project_id)
    .fetch_one(&mut *tx)
    .await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id,
                             assigned_to_id, is_private, parent_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
    )
    .bind(n.project_id)
    .bind(n.tracker_id)
    .bind(n.status_id)
    .bind(n.subject)
    .bind(n.description)
    .bind(n.author_id)
    .bind(n.assigned_to_id)
    .bind(n.is_private)
    .bind(n.parent_id)
    .fetch_one(&mut *tx)
    .await?;
    let key = format!("{pkey}-{number}");
    sqlx::query("INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ($1,$2,$3,$4)")
        .bind(&key)
        .bind(id)
        .bind(n.project_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((id, key))
}

// ---------------------------------------------------------------- bounded waits (T1b, G11)

/// How long a test waits for something before it fails. What tests wait
/// for (a fault gate, a run ending, a lock wait, the frontier) normally
/// takes well under a second, and up to about 5 s under a full parallel
/// run (a file of m1_10_ai_run went from 50 s to 350 s); `sync_all` and
/// `dispatch_all` themselves wait up to 20 s for the horizon. 60 s is more
/// than ten times what a wait normally takes and above the 30 s floor
/// (decisions T1b, testing-playbook "等待一律有上限").
pub const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Pause between two checks of [`poll_until`].
pub const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// Bound of opening the report's own connection. Shorter than
/// [`REPORT_TIMEOUT`], so a server that accepts but never answers is named
/// as such, and a refused connection keeps its error.
pub const REPORT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Bound of each part of the report (outbox, xid holders) on its own: a
/// part stuck behind a lock (`LOCK TABLE outbox`) does not hide the other.
/// The server side gets `lock_timeout` 2 s and `statement_timeout` 4 s on
/// the report's connection as well.
pub const REPORT_SECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Backstop of the whole report (connect + both parts).
const REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

static REPORT_POOL: std::sync::Mutex<Option<PgPool>> = std::sync::Mutex::new(None);

/// The database [`state_report`] reads: `tests/common::test_pool` registers
/// each pool it opens (one test per process under nextest).
pub fn set_report_pool(pool: &PgPool) {
    if let Ok(mut p) = REPORT_POOL.lock() {
        *p = Some(pool.clone());
    }
}

/// What a timed-out wait prints: the outbox backlog of every consumer and
/// who holds the cluster-wide snapshot horizon back
/// ([`crate::metrics::describe_xid_holders`]), for the database
/// [`set_report_pool`] registered.
pub async fn state_report() -> String {
    let Some(pool) = REPORT_POOL.lock().ok().and_then(|p| p.clone()) else {
        return "state: no test database registered (common::test_pool not used)".into();
    };
    state_report_for(&pool).await
}

/// [`state_report`] for `pool`'s database. Reads on a connection of its own
/// (a pool exhausted by the stuck work does not stall it); every step is
/// bounded ([`REPORT_CONNECT_TIMEOUT`], [`REPORT_SECTION_TIMEOUT`]) and a
/// failed step says why instead of hiding the others.
pub async fn state_report_for(pool: &PgPool) -> String {
    use sqlx::Connection as _;
    let opts = (*pool.connect_options())
        .clone()
        .options([("lock_timeout", "2s"), ("statement_timeout", "4s")]);
    let report = async {
        // One plain connection first: a pool retries a refused connection
        // until its timeout and then only says `PoolTimedOut`, losing why.
        let probe = sqlx::postgres::PgConnection::connect_with(&opts);
        match tokio::time::timeout(REPORT_CONNECT_TIMEOUT, probe).await {
            Ok(Ok(c)) => {
                let _ = tokio::time::timeout(REPORT_CONNECT_TIMEOUT, c.close()).await;
            }
            Ok(Err(e)) => return format!("state: cannot connect: {e}"),
            Err(_) => {
                return format!(
                    "state: cannot connect: no answer within {REPORT_CONNECT_TIMEOUT:?}"
                );
            }
        }
        let connect = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(REPORT_CONNECT_TIMEOUT)
            .connect_with(opts);
        let own = match tokio::time::timeout(REPORT_CONNECT_TIMEOUT, connect).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return format!("state: cannot connect: {e}"),
            Err(_) => {
                return format!(
                    "state: cannot connect: no answer within {REPORT_CONNECT_TIMEOUT:?}"
                );
            }
        };
        let outbox = section("outbox", outbox_backlog(&own)).await;
        let holders = section("xid holders", crate::metrics::describe_xid_holders(&own)).await;
        let _ = tokio::time::timeout(REPORT_CONNECT_TIMEOUT, own.close()).await;
        format!("{outbox}\n{holders}")
    };
    tokio::time::timeout(REPORT_TIMEOUT, report)
        .await
        .unwrap_or_else(|_| format!("state: lookup did not finish within {REPORT_TIMEOUT:?}"))
}

async fn section(name: &str, part: impl std::future::Future<Output = String>) -> String {
    tokio::time::timeout(REPORT_SECTION_TIMEOUT, part)
        .await
        .unwrap_or_else(|_| {
            format!("{name}: lookup did not finish within {REPORT_SECTION_TIMEOUT:?}")
        })
}

/// The snapshot horizon H, the newest outbox row, and per consumer cursor
/// how many `issue` rows are still after it.
async fn outbox_backlog(pool: &PgPool) -> String {
    let head: Result<(String, Option<String>, i64), _> = sqlx::query_as(
        "SELECT pg_snapshot_xmin(pg_current_snapshot())::text,
                (SELECT max(xid)::text FROM outbox), (SELECT count(*) FROM outbox)",
    )
    .fetch_one(pool)
    .await;
    let (h, newest, rows) = match head {
        Ok(v) => v,
        Err(e) => return format!("outbox: lookup failed: {e}"),
    };
    let mut s = format!(
        "outbox: H={h} newest xid={} rows={rows}",
        newest.as_deref().unwrap_or("-")
    );
    let cursors: Result<Vec<(String, String, String, i64)>, _> = sqlx::query_as(
        "SELECT c.consumer, c.state, c.last_xid::text,
                (SELECT count(*) FROM outbox o
                  WHERE o.topic = 'issue' AND (o.xid, o.id) > (c.last_xid, c.last_id))
         FROM outbox_cursors c ORDER BY c.consumer",
    )
    .fetch_all(pool)
    .await;
    match cursors {
        Ok(v) if v.is_empty() => s += "\n  no consumer cursors",
        Ok(v) => {
            for (consumer, state, last, pending) in v {
                s += &format!("\n  {consumer} ({state}) at xid {last}: {pending} pending");
            }
        }
        Err(e) => s += &format!("\n  cursors: lookup failed: {e}"),
    }
    s
}

/// Where a wait was called from: the wait helpers are `#[track_caller]`
/// functions returning a future (an `async fn` cannot be), so the message
/// names the test line, not this file.
pub type WaitSite = &'static std::panic::Location<'static>;

/// The panic message of a wait that ran out: what it waited for, how long,
/// from where, then [`state_report`].
pub async fn timeout_message(label: &str, waited: std::time::Duration, at: WaitSite) -> String {
    format!(
        "timed out after {waited:?} waiting for {label} (wait at {at})\n{}",
        state_report().await
    )
}

/// Awaits `fut`, or panics after `dur` with [`timeout_message`]. Every test
/// wait goes through this (or [`poll_until`], [`join`], or a fault gate,
/// which use it).
#[track_caller]
pub fn wait_for<F: std::future::Future>(
    label: &str,
    dur: std::time::Duration,
    fut: F,
) -> impl std::future::Future<Output = F::Output> + use<F> {
    let label = label.to_string();
    let at = std::panic::Location::caller();
    async move { wait_at(&label, dur, fut, at).await }
}

/// [`wait_for`] with the call site given (for helpers that are themselves
/// `#[track_caller]`).
pub async fn wait_at<F: std::future::Future>(
    label: &str,
    dur: std::time::Duration,
    fut: F,
    at: WaitSite,
) -> F::Output {
    match tokio::time::timeout(dur, fut).await {
        Ok(v) => v,
        Err(_) => panic!("{}", timeout_message(label, dur, at).await),
    }
}

/// Awaits a spawned task, bounded by [`WAIT_TIMEOUT`]: its value, or its
/// panic raised again here (so the task's own message is what fails).
#[track_caller]
pub fn join<T>(
    label: &str,
    task: tokio::task::JoinHandle<T>,
) -> impl std::future::Future<Output = T> + use<T> {
    let label = label.to_string();
    let at = std::panic::Location::caller();
    async move {
        let label = label.as_str();
        match wait_at(label, WAIT_TIMEOUT, task, at).await {
            Ok(v) => v,
            Err(e) => match e.try_into_panic() {
                Ok(p) => std::panic::resume_unwind(p),
                Err(e) => panic!("the task for {label} failed: {e} (wait at {at})"),
            },
        }
    }
}

/// Polls a future, catching a panic inside it (the `futures` crate's
/// `catch_unwind`, which this crate does not depend on).
struct CatchUnwind<F>(std::pin::Pin<Box<F>>);

impl<F: std::future::Future> std::future::Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Err(p) => std::task::Poll::Ready(Err(p)),
        }
    }
}

/// A panic payload as text.
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "(non-text panic)".into())
}

/// Calls `check` every [`POLL_EVERY`] until it breaks with a value; panics
/// after `dur` (a hanging `check` included) with [`timeout_message`] and
/// what the last `Continue` said it saw. A panic inside `check` (an
/// `unwrap` on a database error, a failed assertion) fails the wait the
/// same way: its message, then the state.
#[track_caller]
pub fn poll_until<T, C>(
    label: &str,
    dur: std::time::Duration,
    mut check: C,
) -> impl std::future::Future<Output = T> + use<T, C>
where
    C: AsyncFnMut() -> std::ops::ControlFlow<T, String>,
{
    let label = label.to_string();
    let at = std::panic::Location::caller();
    async move {
        let label = label.as_str();
        let mut last = String::new();
        let polled = tokio::time::timeout(dur, async {
            loop {
                match CatchUnwind(Box::pin(check())).await {
                    Ok(std::ops::ControlFlow::Break(v)) => return Ok(v),
                    Ok(std::ops::ControlFlow::Continue(seen)) => last = seen,
                    Err(p) => return Err(panic_text(&*p)),
                }
                tokio::time::sleep(POLL_EVERY).await;
            }
        })
        .await;
        match polled {
            Ok(Ok(v)) => v,
            Ok(Err(why)) => panic!(
                "the check failed while waiting for {label} (wait at {at}): {why}\n{}",
                state_report().await
            ),
            Err(_) => {
                let label = if last.is_empty() {
                    label.to_string()
                } else {
                    format!("{label} (last seen: {last})")
                };
                panic!("{}", timeout_message(&label, dur, at).await)
            }
        }
    }
}
