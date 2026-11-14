// SPDX-License-Identifier: GPL-2.0-or-later
//! Search index metrics and the long-transaction check (design 03 §4.6–4.7),
//! rendered in the Prometheus text format by hand (no metrics crate).
//!
//! Counters and histograms live in memory; gauges that describe the outbox
//! or PostgreSQL are read when `/metrics` is scraped.

use crate::search::SearchIndex;
use anyhow::Result;
use sqlx::PgPool;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Upper bounds (seconds) of every histogram here.
const BUCKETS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl Histogram {
    fn observe(&mut self, v: f64) {
        for (i, b) in BUCKETS.iter().enumerate() {
            if v <= *b {
                self.counts[i] += 1;
            }
        }
        self.count += 1;
        self.sum += v;
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, b) in BUCKETS.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {}",
                self.counts[i]
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}",
            self.count
        );
        let l = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{l} {}", self.sum);
        let _ = writeln!(out, "{name}_count{l} {}", self.count);
    }
}

#[derive(Default)]
struct Hists {
    lag_upper: Histogram,
    lag_commit: Histogram,
    wait: Histogram,
}

/// In-memory search metrics, owned by the [`SearchIndex`].
pub struct SearchMetrics {
    hists: Mutex<Hists>,
    satisfied: AtomicU64,
    lagging: AtomicU64,
    epoch_changed: AtomicU64,
    rounds_ok: AtomicU64,
    rounds_failed: AtomicU64,
    switches: AtomicU64,
    /// Last F published and when it last moved.
    last_f: AtomicU64,
    last_advance: Mutex<Instant>,
    last_success: Mutex<Option<(Instant, SystemTime)>>,
    /// Set by the server's background sync loop; `search_unavailable` for a
    /// stalled sync is judged only then (tests sync by hand).
    pub loop_running: AtomicBool,
}

impl Default for SearchMetrics {
    fn default() -> Self {
        SearchMetrics {
            hists: Mutex::default(),
            satisfied: AtomicU64::new(0),
            lagging: AtomicU64::new(0),
            epoch_changed: AtomicU64::new(0),
            rounds_ok: AtomicU64::new(0),
            rounds_failed: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            last_f: AtomicU64::new(0),
            last_advance: Mutex::new(Instant::now()),
            last_success: Mutex::new(None),
            loop_running: AtomicBool::new(false),
        }
    }
}

/// The sync loop is considered stopped after this long without a
/// successful round (03 §4.11 `search_unavailable`).
pub const SYNC_STALL: Duration = Duration::from_secs(60);

/// How a search with a token ended (`search_consistency_outcome_total`).
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Satisfied,
    Lagging,
    EpochChanged,
}

impl SearchMetrics {
    pub(crate) fn published(&self, f: u64) {
        if self.last_f.fetch_max(f, Ordering::SeqCst) < f
            && let Ok(mut t) = self.last_advance.lock()
        {
            *t = Instant::now();
        }
    }

    pub(crate) fn round(&self, _took: Duration, ok: bool) {
        if ok {
            self.rounds_ok.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut s) = self.last_success.lock() {
                *s = Some((Instant::now(), SystemTime::now()));
            }
        } else {
            self.rounds_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn switched(&self) {
        self.switches.fetch_add(1, Ordering::Relaxed);
    }

    /// A search with `consistent_after` ended (`waited`: how long it waited).
    pub fn consistency(&self, o: Outcome, waited: Option<Duration>) {
        match o {
            Outcome::Satisfied => &self.satisfied,
            Outcome::Lagging => &self.lagging,
            Outcome::EpochChanged => &self.epoch_changed,
        }
        .fetch_add(1, Ordering::Relaxed);
        if let (Some(w), Ok(mut h)) = (waited, self.hists.lock()) {
            h.wait.observe(w.as_secs_f64());
        }
    }

    /// The background loop runs and has not succeeded for [`SYNC_STALL`].
    pub fn sync_stalled(&self) -> bool {
        if !self.loop_running.load(Ordering::SeqCst) {
            return false;
        }
        match self.last_success.lock().ok().and_then(|s| *s) {
            Some((t, _)) => t.elapsed() > SYNC_STALL,
            None => false,
        }
    }

    /// Tests: pretend the last successful round was `ago` ago.
    #[cfg(feature = "test-support")]
    pub fn set_last_success_ago(&self, ago: Duration) {
        if let Ok(mut s) = self.last_success.lock() {
            *s = Some((Instant::now() - ago, SystemTime::now() - ago));
        }
    }

    /// Lag of the events a round just published (03 §4.6): the conservative
    /// upper bound from the transaction start, and — with
    /// `track_commit_timestamp` on — from the commit.
    pub(crate) async fn applied(&self, pool: &PgPool, rows: &[(i64, String, uuid::Uuid)]) {
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let r: Result<Vec<(f64, Option<f64>)>, sqlx::Error> = sqlx::query_as(
            "SELECT extract(epoch FROM clock_timestamp() - o.created_at)::float8,
                    CASE WHEN current_setting('track_commit_timestamp') = 'on' THEN
                      extract(epoch FROM clock_timestamp()
                        - pg_xact_commit_timestamp((o.xid::text::numeric % 4294967296)::text::xid))::float8
                    END
             FROM outbox o WHERE o.id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await;
        match r {
            Ok(v) => {
                if let Ok(mut h) = self.hists.lock() {
                    for (upper, commit) in v {
                        h.lag_upper.observe(upper.max(0.0));
                        if let Some(c) = commit {
                            h.lag_commit.observe(c.max(0.0));
                        }
                    }
                }
            }
            Err(e) => tracing::debug!("search lag metric: {e}"),
        }
    }
}

/// Notification dispatch (design 05 §3.11.2, M1.8): rounds and the delay
/// from an event's transaction start to its dispatch commit.
pub struct NotifyMetrics {
    delay: Mutex<Histogram>,
    rounds_ok: AtomicU64,
    rounds_failed: AtomicU64,
    events: AtomicU64,
    delivered: AtomicU64,
    skipped: AtomicU64,
    last_success_unix: AtomicU64,
}

pub static NOTIFY: NotifyMetrics = NotifyMetrics {
    delay: Mutex::new(Histogram {
        counts: [0; BUCKETS.len()],
        count: 0,
        sum: 0.0,
    }),
    rounds_ok: AtomicU64::new(0),
    rounds_failed: AtomicU64::new(0),
    events: AtomicU64::new(0),
    delivered: AtomicU64::new(0),
    skipped: AtomicU64::new(0),
    last_success_unix: AtomicU64::new(0),
};

impl NotifyMetrics {
    pub(crate) fn round(&self, _took: Duration, ok: Option<&crate::notifications::Round>) {
        match ok {
            Some(r) => {
                self.rounds_ok.fetch_add(1, Ordering::Relaxed);
                self.events.fetch_add(r.events as u64, Ordering::Relaxed);
                self.delivered
                    .fetch_add(r.delivered as u64, Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.last_success_unix.store(now, Ordering::Relaxed);
            }
            None => {
                self.rounds_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// An issue whose pending events were skipped after repeated failures.
    pub(crate) fn skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn delay(&self, d: Duration) {
        if let Ok(mut h) = self.delay.lock() {
            h.observe(d.as_secs_f64());
        }
    }

    /// Renders, and returns the outbox events after the notify cursor.
    async fn render(&self, out: &mut String, pool: &PgPool) -> Result<i64> {
        {
            let h = self
                .delay
                .lock()
                .map_err(|_| anyhow::anyhow!("metrics lock poisoned"))?;
            let _ = writeln!(
                out,
                "# HELP notify_dispatch_delay_seconds Time from an issue event's transaction start to its dispatch"
            );
            let _ = writeln!(out, "# TYPE notify_dispatch_delay_seconds histogram");
            h.render(out, "notify_dispatch_delay_seconds", "");
        }
        let _ = writeln!(out, "# HELP notify_rounds_total Dispatch rounds by result");
        let _ = writeln!(out, "# TYPE notify_rounds_total counter");
        for (k, c) in [("ok", &self.rounds_ok), ("failed", &self.rounds_failed)] {
            let _ = writeln!(
                out,
                "notify_rounds_total{{result=\"{k}\"}} {}",
                c.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(out, "# HELP notify_events_total Issue events dispatched");
        let _ = writeln!(out, "# TYPE notify_events_total counter");
        let _ = writeln!(
            out,
            "notify_events_total {}",
            self.events.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP notify_deliveries_total Notification rows written or extended"
        );
        let _ = writeln!(out, "# TYPE notify_deliveries_total counter");
        let _ = writeln!(
            out,
            "notify_deliveries_total {}",
            self.delivered.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP notify_skipped_issues_total Issues whose pending events were skipped after repeated dispatch failures"
        );
        let _ = writeln!(out, "# TYPE notify_skipped_issues_total counter");
        let _ = writeln!(
            out,
            "notify_skipped_issues_total {}",
            self.skipped.load(Ordering::Relaxed)
        );
        gauge(
            out,
            "notify_last_success_timestamp_seconds",
            "Unix time of the last successful dispatch round",
            self.last_success_unix.load(Ordering::Relaxed) as f64,
        );
        // The outbox after the notify cursor: while it grows the consumer
        // lags and also holds back the outbox GC.
        let (pending, oldest): (i64, Option<f64>) = sqlx::query_as(
            "SELECT count(o.id), extract(epoch FROM now() - min(o.created_at))::float8
             FROM outbox_cursors c LEFT JOIN outbox o
               ON o.topic = 'issue' AND (o.xid, o.id) > (c.last_xid, c.last_id)
             WHERE c.consumer = 'notify'",
        )
        .fetch_optional(pool)
        .await?
        .unwrap_or((0, None));
        gauge(
            out,
            "notify_outbox_pending",
            "Outbox events after the notify cursor",
            pending as f64,
        );
        gauge(
            out,
            "notify_outbox_oldest_pending_age_seconds",
            "Age of the oldest event the notify consumer has not passed",
            oldest.unwrap_or(0.0).max(0.0),
        );
        Ok(pending)
    }
}

/// An open transaction that holds an xid (03 §4.7): these hold back the
/// search frontier, in any database of the instance.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct XactHolder {
    pub datname: Option<String>,
    pub pid: i32,
    pub usename: Option<String>,
    pub application_name: Option<String>,
    pub state: Option<String>,
    pub xact_age_seconds: f64,
    pub query: Option<String>,
    /// `xact_start` as PostgreSQL prints it (the prepare time for prepared
    /// transactions).
    pub xact_start: Option<String>,
    /// `wait_event_type:wait_event`, e.g. `IPC:CheckpointDone` for a
    /// `DROP DATABASE` waiting on its checkpoint.
    pub wait_event: Option<String>,
}

/// The oldest xid holders, instance-wide (needs `pg_read_all_stats` to see
/// other users' sessions), plus prepared transactions.
pub async fn xid_holders(pool: &PgPool) -> Result<Vec<XactHolder>> {
    let mut v: Vec<XactHolder> = sqlx::query_as(
        "SELECT datname::text, pid, usename::text, application_name, state,
                extract(epoch FROM now() - xact_start)::float8 AS xact_age_seconds,
                left(query, 120) AS query,
                xact_start::text AS xact_start,
                wait_event_type || ':' || wait_event AS wait_event
         FROM pg_stat_activity
         WHERE backend_xid IS NOT NULL AND pid <> pg_backend_pid()
         ORDER BY xact_start LIMIT 5",
    )
    .fetch_all(pool)
    .await?;
    let prepared: Vec<XactHolder> = sqlx::query_as(
        "SELECT database::text AS datname, 0 AS pid, owner::text AS usename,
                ('prepared ' || gid) AS application_name, 'prepared' AS state,
                extract(epoch FROM now() - prepared)::float8 AS xact_age_seconds,
                NULL::text AS query, prepared::text AS xact_start,
                NULL::text AS wait_event
         FROM pg_prepared_xacts ORDER BY prepared LIMIT 5",
    )
    .fetch_all(pool)
    .await?;
    v.extend(prepared);
    v.sort_by(|a, b| b.xact_age_seconds.total_cmp(&a.xact_age_seconds));
    Ok(v)
}

/// The xid holders as text for an error message: who holds the cluster-wide
/// snapshot horizon back (G21). Never fails; a failed lookup says so.
pub async fn describe_xid_holders(pool: &PgPool) -> String {
    let holders = match xid_holders(pool).await {
        Ok(h) => h,
        Err(e) => return format!("xid holders: lookup failed: {e:#}"),
    };
    if holders.is_empty() {
        return "xid holders: none right now".into();
    }
    let mut s = String::from("xid holders (oldest first):");
    for h in &holders {
        let _ = write!(
            s,
            "\n  datname={} pid={} xact_start={} age={:.1}s state={} wait_event={} application={} query={:?}",
            h.datname.as_deref().unwrap_or("-"),
            h.pid,
            h.xact_start.as_deref().unwrap_or("-"),
            h.xact_age_seconds,
            h.state.as_deref().unwrap_or("-"),
            h.wait_event.as_deref().unwrap_or("-"),
            h.application_name.as_deref().unwrap_or("-"),
            h.query.as_deref().unwrap_or(""),
        );
    }
    s
}

/// The error of `SearchIndex::sync_all` and `notifications::dispatch_all`
/// when the outbox did not catch up in time (D1(d)): what timed out, then
/// [`describe_xid_holders`].
pub async fn horizon_timeout_message(pool: &PgPool, what: &str) -> String {
    format!(
        "{what} did not catch up with the outbox within 20 s; {}",
        describe_xid_holders(pool).await
    )
}

/// Alert levels for long xid holders (03 §4.7 initial thresholds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Level {
    Warning,
    Critical,
}

pub const XACT_WARN: Duration = Duration::from_secs(15);
pub const XACT_CRIT: Duration = Duration::from_secs(60);

/// Which holders cross the thresholds. Never kills anything (03 §4.7).
pub fn xact_alerts(
    holders: &[XactHolder],
    warn: Duration,
    crit: Duration,
) -> Vec<(Level, XactHolder)> {
    holders
        .iter()
        .filter_map(|h| {
            let age = h.xact_age_seconds;
            if age > crit.as_secs_f64() {
                Some((Level::Critical, h.clone()))
            } else if age > warn.as_secs_f64() {
                Some((Level::Warning, h.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// One check of the long-transaction monitor: logs and returns the alerts.
pub async fn check_long_xacts(
    pool: &PgPool,
    warn: Duration,
    crit: Duration,
) -> Result<Vec<(Level, XactHolder)>> {
    let alerts = xact_alerts(&xid_holders(pool).await?, warn, crit);
    for (level, h) in &alerts {
        match level {
            Level::Critical => tracing::error!(
                pid = h.pid,
                database = h.datname.as_deref().unwrap_or(""),
                application = h.application_name.as_deref().unwrap_or(""),
                age_s = h.xact_age_seconds,
                query = h.query.as_deref().unwrap_or(""),
                "transaction holding an xid blocks the search frontier"
            ),
            Level::Warning => tracing::warn!(
                pid = h.pid,
                database = h.datname.as_deref().unwrap_or(""),
                application = h.application_name.as_deref().unwrap_or(""),
                age_s = h.xact_age_seconds,
                "long transaction holding an xid"
            ),
        }
    }
    Ok(alerts)
}

fn gauge(out: &mut String, name: &str, help: &str, v: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {v}");
}

/// A label value as the text format wants it (`\`, `"` and newlines
/// escaped). Label values here are route templates and fixed names.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- HTTP (T2)

const STATUS_CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

/// The status class index of `status` (anything outside 100–499 counts as
/// a server error).
fn status_class(status: u16) -> usize {
    match status {
        100..=199 => 0,
        200..=299 => 1,
        300..=399 => 2,
        400..=499 => 3,
        _ => 4,
    }
}

struct RouteStats {
    by_class: [u64; STATUS_CLASSES.len()],
    duration: Histogram,
}

/// HTTP RED metrics by route template × method (T2, research 06 I-3).
/// Keys come from the router's own route table plus
/// [`crate::telemetry::UNMATCHED_ROUTE`], and the method is one of eight
/// labels, so the number of series is bounded by the code, not by clients.
pub struct HttpMetrics {
    /// route template → method → counts.
    routes: Mutex<RouteMap>,
}

type RouteMap =
    std::collections::BTreeMap<String, std::collections::BTreeMap<&'static str, RouteStats>>;

pub static HTTP: HttpMetrics = HttpMetrics {
    routes: Mutex::new(std::collections::BTreeMap::new()),
};

/// Every (route, method, stats), in label order.
fn each(m: &RouteMap) -> impl Iterator<Item = (&String, &'static str, &RouteStats)> {
    m.iter()
        .flat_map(|(r, by)| by.iter().map(move |(me, s)| (r, *me, s)))
}

impl HttpMetrics {
    /// One finished request. The route string is copied only the first
    /// time that route is seen.
    pub fn observe(&self, route: &str, method: &'static str, status: u16, took: Duration) {
        let Ok(mut m) = self.routes.lock() else {
            return;
        };
        if !m.contains_key(route) {
            m.insert(route.to_string(), std::collections::BTreeMap::new());
        }
        let Some(by_method) = m.get_mut(route) else {
            return;
        };
        let s = by_method.entry(method).or_insert_with(|| RouteStats {
            by_class: [0; STATUS_CLASSES.len()],
            duration: Histogram::default(),
        });
        s.by_class[status_class(status)] += 1;
        s.duration.observe(took.as_secs_f64());
    }

    /// Requests counted for `route` × `method` × status class (tests).
    pub fn count(&self, route: &str, method: &str, class: &str) -> u64 {
        let Some(i) = STATUS_CLASSES.iter().position(|c| *c == class) else {
            return 0;
        };
        self.routes
            .lock()
            .map(|m| {
                each(&m)
                    .filter(|(r, me, _)| *r == route && *me == method)
                    .map(|(_, _, s)| s.by_class[i])
                    .sum()
            })
            .unwrap_or(0)
    }

    /// The route labels seen so far (tests: no path values among them).
    pub fn routes(&self) -> Vec<String> {
        self.routes
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn render(&self, out: &mut String) -> Result<()> {
        let m = self
            .routes
            .lock()
            .map_err(|_| anyhow::anyhow!("metrics lock poisoned"))?;
        let _ = writeln!(
            out,
            "# HELP http_requests_total HTTP requests by route template, method and status class"
        );
        let _ = writeln!(out, "# TYPE http_requests_total counter");
        for (route, method, s) in each(&m) {
            for (i, class) in STATUS_CLASSES.iter().enumerate() {
                if s.by_class[i] > 0 {
                    let _ = writeln!(
                        out,
                        "http_requests_total{{route=\"{}\",method=\"{method}\",status_class=\"{class}\"}} {}",
                        label(route),
                        s.by_class[i]
                    );
                }
            }
        }
        let _ = writeln!(
            out,
            "# HELP http_request_errors_total HTTP requests answered with a 5xx status"
        );
        let _ = writeln!(out, "# TYPE http_request_errors_total counter");
        for (route, method, s) in each(&m) {
            let _ = writeln!(
                out,
                "http_request_errors_total{{route=\"{}\",method=\"{method}\"}} {}",
                label(route),
                s.by_class[4]
            );
        }
        let _ = writeln!(
            out,
            "# HELP http_request_duration_seconds Time from the request reaching the router to the response head"
        );
        let _ = writeln!(out, "# TYPE http_request_duration_seconds histogram");
        for (route, method, s) in each(&m) {
            s.duration.render(
                out,
                "http_request_duration_seconds",
                &format!("route=\"{}\",method=\"{method}\"", label(route)),
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- pool (T2)

/// `sqlx::Error::PoolTimedOut` seen on a request or a worker round, since
/// the process started: the pool had no free connection within
/// `acquire_timeout` (sqlx has no wait-time API, research 06 §2.4).
pub static POOL_ACQUIRE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Counts `e` in [`POOL_ACQUIRE_TIMEOUTS`] when its chain holds a pool
/// timeout. Called where errors end: the 500 of a request, the internal
/// error of an MCP tool, a failed worker round.
pub fn note_error(e: &anyhow::Error) {
    if e.chain().any(|c| {
        matches!(
            c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::PoolTimedOut)
        )
    }) {
        POOL_ACQUIRE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn render_pool(out: &mut String, pool: &PgPool) {
    let size = pool.size();
    let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX).min(size);
    let _ = writeln!(
        out,
        "# HELP db_pool_connections Open pool connections by state"
    );
    let _ = writeln!(out, "# TYPE db_pool_connections gauge");
    let _ = writeln!(out, "db_pool_connections{{state=\"idle\"}} {idle}");
    let _ = writeln!(
        out,
        "db_pool_connections{{state=\"in_use\"}} {}",
        size - idle
    );
    gauge(
        out,
        "db_pool_max_connections",
        "The pool's connection cap",
        pool.options().get_max_connections() as f64,
    );
    counter(
        out,
        "db_pool_acquire_timeouts_total",
        "Requests and worker rounds that failed waiting for a pool connection",
        POOL_ACQUIRE_TIMEOUTS.load(Ordering::Relaxed),
    );
    counter(
        out,
        "db_pool_released_in_transaction_total",
        "Connections returned inside a transaction and closed (G20)",
        crate::db::RELEASED_IN_TRANSACTION.load(Ordering::Relaxed),
    );
}

// ---------------------------------------------------------------- workers (T2)

/// The background workers, one label each (research 06 §2.5, I-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worker {
    /// Search index sync (`serve` and `stdio`).
    Search,
    /// Notification dispatch.
    Notify,
    /// Custom field index reconcile and conversion resume.
    FieldIndex,
    /// Expired login challenges.
    LoginSweeper,
    /// GitHub jobs and scheduled passes (only with GitHub configured).
    Github,
    /// The built-in AI's lease sweep and retention.
    AiJobs,
    /// One built-in AI run (one round per run).
    AiRun,
}

impl Worker {
    pub const ALL: [Worker; 7] = [
        Worker::Search,
        Worker::Notify,
        Worker::FieldIndex,
        Worker::LoginSweeper,
        Worker::Github,
        Worker::AiJobs,
        Worker::AiRun,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Worker::Search => "search",
            Worker::Notify => "notify",
            Worker::FieldIndex => "field_index",
            Worker::LoginSweeper => "login_sweeper",
            Worker::Github => "github",
            Worker::AiJobs => "ai_jobs",
            Worker::AiRun => "ai_run",
        }
    }

    fn stats(self) -> &'static WorkerStats {
        &WORKERS[self as usize]
    }
}

struct WorkerStats {
    /// The process runs this worker, so `/readyz` waits for it.
    expected: AtomicBool,
    started: AtomicBool,
    ok: AtomicU64,
    failed: AtomicU64,
    last_success_unix: AtomicU64,
    duration: Mutex<Histogram>,
}

impl WorkerStats {
    const fn new() -> Self {
        WorkerStats {
            expected: AtomicBool::new(false),
            started: AtomicBool::new(false),
            ok: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            last_success_unix: AtomicU64::new(0),
            duration: Mutex::new(Histogram {
                counts: [0; BUCKETS.len()],
                count: 0,
                sum: 0.0,
            }),
        }
    }
}

static WORKERS: [WorkerStats; Worker::ALL.len()] =
    [const { WorkerStats::new() }; Worker::ALL.len()];

/// This process is going to run `w`: `/readyz` is not ready until it has
/// started. Called before the task is spawned.
pub fn worker_expected(w: Worker) {
    w.stats().expected.store(true, Ordering::SeqCst);
}

/// `w`'s task is running (first thing inside the spawned task).
pub fn worker_started(w: Worker) {
    w.stats().started.store(true, Ordering::SeqCst);
}

/// One round of `w`: how long it took and whether it succeeded. A failure
/// also goes through [`note_error`].
pub fn worker_round(w: Worker, took: Duration, outcome: std::result::Result<(), &anyhow::Error>) {
    let s = w.stats();
    match outcome {
        Ok(()) => {
            s.ok.fetch_add(1, Ordering::Relaxed);
            s.last_success_unix.store(unix_now(), Ordering::Relaxed);
        }
        Err(e) => {
            s.failed.fetch_add(1, Ordering::Relaxed);
            note_error(e);
        }
    }
    if let Ok(mut h) = s.duration.lock() {
        h.observe(took.as_secs_f64());
    }
}

/// Runs one round of `w` and records it ([`worker_round`]).
pub async fn timed_round<T>(
    w: Worker,
    f: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let started = Instant::now();
    let r = f.await;
    worker_round(w, started.elapsed(), r.as_ref().map(|_| ()));
    r
}

/// Every worker this process expects has started (`/readyz`).
pub fn workers_ready() -> bool {
    Worker::ALL.iter().all(|w| {
        let s = w.stats();
        !s.expected.load(Ordering::SeqCst) || s.started.load(Ordering::SeqCst)
    })
}

/// Rounds of `w` so far: (ok, failed). For tests.
pub fn worker_rounds(w: Worker) -> (u64, u64) {
    let s = w.stats();
    (
        s.ok.load(Ordering::Relaxed),
        s.failed.load(Ordering::Relaxed),
    )
}

/// Whether `w` has started. For tests.
pub fn worker_is_started(w: Worker) -> bool {
    w.stats().started.load(Ordering::SeqCst)
}

/// Tests: forget that `w` is expected or started.
#[cfg(feature = "test-support")]
pub fn reset_worker(w: Worker) {
    let s = w.stats();
    s.expected.store(false, Ordering::SeqCst);
    s.started.store(false, Ordering::SeqCst);
}

async fn render_workers(out: &mut String, pool: &PgPool, search: i64, notify: i64) -> Result<()> {
    let _ = writeln!(
        out,
        "# HELP worker_started Whether the worker's task is running in this process"
    );
    let _ = writeln!(out, "# TYPE worker_started gauge");
    for w in Worker::ALL {
        let _ = writeln!(
            out,
            "worker_started{{worker=\"{}\"}} {}",
            w.name(),
            u8::from(w.stats().started.load(Ordering::SeqCst))
        );
    }
    let _ = writeln!(out, "# HELP worker_rounds_total Worker rounds by result");
    let _ = writeln!(out, "# TYPE worker_rounds_total counter");
    for w in Worker::ALL {
        let (ok, failed) = worker_rounds(w);
        let n = w.name();
        let _ = writeln!(
            out,
            "worker_rounds_total{{worker=\"{n}\",result=\"ok\"}} {ok}"
        );
        let _ = writeln!(
            out,
            "worker_rounds_total{{worker=\"{n}\",result=\"failed\"}} {failed}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP worker_last_success_timestamp_seconds Unix time of the worker's last successful round (0: none yet)"
    );
    let _ = writeln!(out, "# TYPE worker_last_success_timestamp_seconds gauge");
    for w in Worker::ALL {
        let _ = writeln!(
            out,
            "worker_last_success_timestamp_seconds{{worker=\"{}\"}} {}",
            w.name(),
            w.stats().last_success_unix.load(Ordering::Relaxed)
        );
    }
    let _ = writeln!(
        out,
        "# HELP worker_round_duration_seconds How long one worker round took"
    );
    let _ = writeln!(out, "# TYPE worker_round_duration_seconds histogram");
    for w in Worker::ALL {
        let h = w
            .stats()
            .duration
            .lock()
            .map_err(|_| anyhow::anyhow!("metrics lock poisoned"))?;
        h.render(
            out,
            "worker_round_duration_seconds",
            &format!("worker=\"{}\"", w.name()),
        );
    }
    // Work waiting for a worker: outbox events (search, notify), due GitHub
    // jobs, AI runs not finished.
    let (github, ai): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM github_event_jobs
                  WHERE payload IS NOT NULL AND next_at <= clock_timestamp()),
                (SELECT count(*) FROM ai_runs WHERE state IN ('queued','running'))",
    )
    .fetch_one(pool)
    .await?;
    let _ = writeln!(out, "# HELP worker_backlog Items waiting for the worker");
    let _ = writeln!(out, "# TYPE worker_backlog gauge");
    for (w, n) in [
        (Worker::Search, search),
        (Worker::Notify, notify),
        (Worker::Github, github),
        (Worker::AiRun, ai),
    ] {
        let _ = writeln!(out, "worker_backlog{{worker=\"{}\"}} {n}", w.name());
    }
    Ok(())
}

fn counter(out: &mut String, name: &str, help: &str, v: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {v}");
}

/// The `/metrics` body (Prometheus text format 0.0.4).
pub async fn render(pool: &PgPool, index: &SearchIndex) -> Result<String> {
    let m = &index.metrics;
    let mut out = String::new();
    {
        let h = m
            .hists
            .lock()
            .map_err(|_| anyhow::anyhow!("metrics lock poisoned"))?;
        let _ = writeln!(
            out,
            "# HELP search_index_lag_seconds Time from transaction start (upper) or commit to the view that shows it"
        );
        let _ = writeln!(out, "# TYPE search_index_lag_seconds histogram");
        h.lag_upper
            .render(&mut out, "search_index_lag_seconds", "bound=\"upper\"");
        h.lag_commit
            .render(&mut out, "search_index_lag_seconds", "bound=\"commit\"");
        let _ = writeln!(
            out,
            "# HELP search_consistency_wait_seconds How long searches with consistent_after waited"
        );
        let _ = writeln!(out, "# TYPE search_consistency_wait_seconds histogram");
        h.wait
            .render(&mut out, "search_consistency_wait_seconds", "");
    }
    let _ = writeln!(
        out,
        "# HELP search_consistency_outcome_total Searches with consistent_after by outcome"
    );
    let _ = writeln!(out, "# TYPE search_consistency_outcome_total counter");
    for (k, c) in [
        ("satisfied", &m.satisfied),
        ("lagging", &m.lagging),
        ("epoch_changed", &m.epoch_changed),
    ] {
        let _ = writeln!(
            out,
            "search_consistency_outcome_total{{outcome=\"{k}\"}} {}",
            c.load(Ordering::Relaxed)
        );
    }
    let _ = writeln!(out, "# HELP search_sync_rounds_total Sync rounds by result");
    let _ = writeln!(out, "# TYPE search_sync_rounds_total counter");
    let _ = writeln!(
        out,
        "search_sync_rounds_total{{result=\"ok\"}} {}",
        m.rounds_ok.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "search_sync_rounds_total{{result=\"failed\"}} {}",
        m.rounds_failed.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# HELP search_generation_switches_total Blue-green switches"
    );
    let _ = writeln!(out, "# TYPE search_generation_switches_total counter");
    let _ = writeln!(
        out,
        "search_generation_switches_total {}",
        m.switches.load(Ordering::Relaxed)
    );

    // Outbox after the serving generation's durable cursor.
    let (pending, oldest): (i64, Option<f64>) = match index.durable_state() {
        Some((c, _)) => sqlx::query_as(
            "SELECT count(*), extract(epoch FROM now() - min(created_at))::float8 FROM outbox
             WHERE topic = 'issue' AND (xid, id) > ($1::text::xid8, $2::bigint)",
        )
        .bind(c.xid.to_string())
        .bind(c.id)
        .fetch_one(pool)
        .await?,
        None => sqlx::query_as(
            "SELECT count(*), extract(epoch FROM now() - min(created_at))::float8 FROM outbox WHERE topic = 'issue'",
        )
        .fetch_one(pool)
        .await?,
    };
    gauge(
        &mut out,
        "search_outbox_pending",
        "Outbox events after the serving generation's cursor",
        pending as f64,
    );
    gauge(
        &mut out,
        "search_outbox_oldest_pending_age_seconds",
        "Age of the oldest unapplied event (transaction start time)",
        oldest.unwrap_or(0.0).max(0.0),
    );
    let stalled = if pending > 0 || index.waiting() > 0 {
        m.last_advance
            .lock()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    } else {
        0.0
    };
    gauge(
        &mut out,
        "search_frontier_stalled_seconds",
        "How long F has not moved while events or waiters are pending",
        stalled,
    );
    let last = m
        .last_success
        .lock()
        .ok()
        .and_then(|s| *s)
        .and_then(|(_, t)| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    gauge(
        &mut out,
        "search_sync_last_success_timestamp_seconds",
        "Unix time of the last successful sync round",
        last,
    );
    gauge(
        &mut out,
        "search_frontier",
        "Published exclusive frontier F (xid8)",
        index.frontier() as f64,
    );
    let oldest_xid = xid_holders(pool)
        .await?
        .first()
        .map(|h| h.xact_age_seconds)
        .unwrap_or(0.0);
    gauge(
        &mut out,
        "pg_oldest_xid_holder_age_seconds",
        "Age of the oldest transaction holding an xid, instance-wide",
        oldest_xid.max(0.0),
    );
    let notify_pending = NOTIFY.render(&mut out, pool).await?;
    HTTP.render(&mut out)?;
    render_pool(&mut out, pool);
    render_workers(&mut out, pool, pending, notify_pending).await?;
    Ok(out)
}
