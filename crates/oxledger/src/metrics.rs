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

    async fn render(&self, out: &mut String, pool: &PgPool) -> Result<()> {
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
        Ok(())
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
    NOTIFY.render(&mut out, pool).await?;
    Ok(out)
}
