//! CSV export (design 06 §3.9, R-E1–R-E11) and its audit trail.
//!
//! An export reads what the caller sees: an issue view or conditions, or
//! the time report's filters. The whole file comes from one REPEATABLE
//! READ snapshot and is written row by row into the response; nothing is
//! kept whole in memory. Only people export: no agent, nobody anonymous.

use crate::auth::AuthContext;
use crate::error::{ApiError, ErrorCode, api};
use crate::service::{App, read_tx};
use crate::time_reports::{self, ListQuery};
use crate::views::{self, ExportSource};
use anyhow::Result;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

/// R-E6: rows of one export.
pub const MAX_ROWS: usize = 10_000;
/// R-E6: bytes of one export; past this the connection is cut.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;
/// R-E6: one export, start to end.
pub const TIME_LIMIT: Duration = Duration::from_secs(30);
/// R-E6: exports a person starts in a minute.
pub const PER_MINUTE: usize = 5;
const CHUNK: usize = 100;

/// Tests: other limits (0 = the constants above).
pub static LIMIT_ROWS: AtomicUsize = AtomicUsize::new(0);
pub static LIMIT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static LIMIT_PER_MINUTE: AtomicUsize = AtomicUsize::new(0);

fn max_rows() -> usize {
    match LIMIT_ROWS.load(Ordering::Relaxed) {
        0 => MAX_ROWS,
        n => n,
    }
}

fn max_bytes() -> u64 {
    match LIMIT_BYTES.load(Ordering::Relaxed) {
        0 => MAX_BYTES,
        n => n,
    }
}

fn per_minute() -> usize {
    match LIMIT_PER_MINUTE.load(Ordering::Relaxed) {
        0 => PER_MINUTE,
        n => n,
    }
}

// ---------------------------------------------------------------- input

/// `POST /exports/issues`: a view, or conditions (R-E1).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueExport {
    /// A view id (`builtin:<name>` too). Not with `where` or `project_id`.
    #[serde(default)]
    pub view_id: Option<String>,
    /// A Filter AST v2; without it and without `view_id`: every issue the
    /// caller sees (of `project_id`).
    #[serde(default, rename = "where")]
    pub where_: Option<Json>,
    /// A project id or key the conditions stay in.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Column keys (`key`, `subject`, …, `cf:<id>`), in order; default: the
    /// view's columns.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// Adds the description (as written) as the last column.
    #[serde(default)]
    pub include_description: bool,
}

/// `POST /exports/time-entries`: the time report's filters (05 §3.5.2).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeExport {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    /// A user id or `me`.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub activity: Option<Uuid>,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// A tracker id or `none`.
    #[serde(default)]
    pub tracker: Option<String>,
    /// A version id or `none`.
    #[serde(default)]
    pub fixed_version: Option<String>,
}

/// A started export: the file name and the body, piece by piece. An `Err`
/// piece means the file is not whole (R-E6): the response is cut there.
pub struct Export {
    pub filename: String,
    ended: bool,
    body: mpsc::Receiver<std::io::Result<Vec<u8>>>,
}

impl Export {
    /// The next piece. The writer ends a whole file with an empty piece: a
    /// channel that closes without it (the writer failed, or panicked) is
    /// an error, never a short file.
    pub async fn next(&mut self) -> Option<std::io::Result<Vec<u8>>> {
        if self.ended {
            return None;
        }
        match self.body.recv().await {
            Some(Ok(p)) if p.is_empty() => {
                self.ended = true;
                None
            }
            Some(Ok(p)) => Some(Ok(p)),
            Some(Err(e)) => {
                self.ended = true;
                Some(Err(e))
            }
            None => {
                self.ended = true;
                Some(Err(std::io::Error::other(
                    "the export stopped before its end",
                )))
            }
        }
    }

    /// The whole body (tests, and small files): `Err` when it was cut.
    pub async fn collect(mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(piece) = self.next().await {
            out.extend(piece?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- limits (R-E6)

#[derive(Default)]
struct Usage {
    running: bool,
    started: VecDeque<Instant>,
}

static USAGE: Mutex<Option<HashMap<Uuid, Usage>>> = Mutex::new(None);

/// One running export of a person; dropping it frees the place.
struct Slot(Uuid);

impl Slot {
    fn take(user: Uuid) -> Result<Self> {
        let mut g = USAGE.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        m.retain(|_, u| {
            u.started
                .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
            u.running || !u.started.is_empty()
        });
        let u = m.entry(user).or_default();
        if u.running {
            return Err(
                ApiError::new(ErrorCode::RateLimited, "an export of yours is running")
                    .with_details(json!({"reason": "export_running"}))
                    .into(),
            );
        }
        if u.started.len() >= per_minute() {
            let wait = u
                .started
                .front()
                .map(|t| Duration::from_secs(60).saturating_sub(now.duration_since(*t)))
                .unwrap_or_default();
            return Err(ApiError::new(
                ErrorCode::RateLimited,
                "too many exports in a minute; try again later",
            )
            .with_details(json!({"reason": "export_rate"}))
            .with_retry_after(wait.as_millis() as u64)
            .into());
        }
        u.started.push_back(now);
        u.running = true;
        Ok(Self(user))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut g = USAGE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(u) = g.as_mut().and_then(|m| m.get_mut(&self.0)) {
            u.running = false;
        }
    }
}

fn too_large() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ExportTooLarge,
        "more rows than one export holds; narrow the conditions",
    )
    .with_details(json!({"limit": max_rows()}))
    .into()
}

fn timed_out() -> anyhow::Error {
    api(
        ErrorCode::QueryTimeout,
        "the export took too long; narrow the conditions",
    )
}

// ---------------------------------------------------------------- CSV (R-E3–R-E5)

/// R-E4: a text cell a spreadsheet would read as a formula gets a leading
/// apostrophe (OWASP CSV injection).
pub fn guard(s: &str) -> std::borrow::Cow<'_, str> {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r' | '＝' | '＋' | '－' | '＠') => {
            std::borrow::Cow::Owned(format!("'{s}"))
        }
        _ => std::borrow::Cow::Borrowed(s),
    }
}

/// R-E5: rows as RFC 4180 with CRLF. `plain[i]`: column `i` is a number or
/// a date oxledger wrote, never guarded.
fn csv_rows(rows: &[Vec<String>], plain: &[bool]) -> Result<Vec<u8>> {
    let mut w = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(Vec::new());
    for r in rows {
        w.write_record(r.iter().enumerate().map(|(i, c)| {
            if plain.get(i).copied().unwrap_or(false) {
                std::borrow::Cow::Borrowed(c.as_str())
            } else {
                guard(c)
            }
            .into_owned()
        }))?;
    }
    w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

// ---------------------------------------------------------------- audit (R-E8)

/// The key of the audit HMAC (`server_secrets.export_filter`), created on
/// first use. Read each time: replacing it changes every later hash.
async fn audit_key(pool: &sqlx::PgPool) -> Result<Vec<u8>> {
    let fresh = crate::auth::random_secret();
    sqlx::query(
        "INSERT INTO server_secrets (name, secret) VALUES ('export_filter', $1)
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(&fresh[..])
    .execute(pool)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT secret FROM server_secrets WHERE name = 'export_filter'")
            .fetch_one(pool)
            .await?,
    )
}

struct Audit {
    actor: Uuid,
    kind: &'static str,
    project: Option<Uuid>,
    view: Option<Uuid>,
    /// The normalized conditions; only their HMAC is stored.
    filter: Json,
    columns: Vec<String>,
}

/// R-E8: the audit row. A file's row is written before its first byte
/// (`truncated` until [`settle`] says the file went out whole), so nothing
/// leaves without one; a failure here refuses the export.
async fn record(app: &App, a: &Audit, rows: usize, bytes: u64, truncated: bool) -> Result<Uuid> {
    let key = audit_key(&app.pool).await?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).expect("any key length");
    mac.update(b"oxledger/export-filter/v1\n");
    mac.update(a.filter.to_string().as_bytes());
    let hash = mac.finalize().into_bytes().to_vec();
    sqlx::query_scalar(
        "INSERT INTO export_events (actor_id, kind, project_id, view_id, filter_hash, columns, row_count,
                                    bytes, truncated)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(a.actor)
    .bind(a.kind)
    .bind(a.project)
    .bind(a.view)
    .bind(hash)
    .bind(&a.columns)
    .bind(rows as i32)
    .bind(bytes as i64)
    .bind(truncated)
    .fetch_one(&app.pool)
    .await
    .map_err(Into::into)
}

/// What really went out: the rows and bytes sent, and whether the file
/// stopped before its end (byte limit, time limit, an error, or a reader
/// that went away).
async fn settle(app: &App, id: Uuid, rows: usize, bytes: u64, truncated: bool) {
    let r = sqlx::query(
        "UPDATE export_events SET row_count = $2, bytes = $3, truncated = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(rows as i32)
    .bind(bytes as i64)
    .bind(truncated)
    .execute(&app.pool)
    .await;
    if let Err(e) = r {
        // The row stays as written before the first byte: every row of the
        // snapshot, not known to be whole.
        tracing::error!("export audit: {e}");
    }
}

// ---------------------------------------------------------------- the writer

/// Sends the file's pieces; counts what went out.
struct Out {
    tx: mpsc::Sender<std::io::Result<Vec<u8>>>,
    bytes: u64,
    limit: u64,
    /// R-E6: nothing waits past it, a reader that stopped reading included.
    deadline: Instant,
}

enum Sent {
    Ok,
    /// Past the byte limit: the reader was told the file is not whole.
    TooBig,
    /// Nobody is reading any more, or not before the time limit.
    Gone,
}

impl Out {
    async fn send(&mut self, piece: Vec<u8>) -> Sent {
        // An empty piece is the end mark ([`Export::next`]).
        if piece.is_empty() {
            return Sent::Ok;
        }
        let len = piece.len() as u64;
        if self.bytes + len > self.limit {
            self.fail("the export is larger than the byte limit").await;
            return Sent::TooBig;
        }
        match tokio::time::timeout_at(self.deadline.into(), self.tx.send(Ok(piece))).await {
            Ok(Ok(())) => {
                self.bytes += len;
                Sent::Ok
            }
            _ => Sent::Gone,
        }
    }

    async fn fail(&mut self, why: &str) {
        let piece = Err(std::io::Error::other(why.to_string()));
        let _ = tokio::time::timeout(Duration::from_secs(2), self.tx.send(piece)).await;
    }

    /// The end mark: the file is whole only if the reader took it.
    async fn finish(&mut self) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(10), self.tx.send(Ok(Vec::new()))).await,
            Ok(Ok(()))
        )
    }
}

/// Today in the organisation's time zone, as the timestamps in the file
/// (U42): `20261117`.
async fn today_stamp(conn: &mut sqlx::PgConnection) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT replace(left(org_iso(now()), 10), '-', '')")
            .fetch_one(conn)
            .await?,
    )
}

fn person(caller: AuthContext) -> Result<Uuid> {
    // R-E9, R-E10: a person signed in; no agent, nobody anonymous.
    caller
        .require_human_session()
        .map(|(user, _)| user)
        .map_err(|_| api(ErrorCode::Unauthenticated, "sign in to export"))
}

// ---------------------------------------------------------------- issues

/// `POST /exports/issues`. Everything that can be refused is refused
/// before the first byte; afterwards the file is written by a task that
/// owns the snapshot.
pub async fn issues(app: Arc<App>, caller: AuthContext, input: &IssueExport) -> Result<Export> {
    let user = person(caller)?;
    if input.view_id.is_some() && (input.where_.is_some() || input.project_id.is_some()) {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            "view_id, or where and project_id",
        )
        .with_details(json!({"path": "view_id"}))
        .into());
    }
    let started = Instant::now();
    let deadline = started + TIME_LIMIT;
    let prepare = async {
        let mut tx = read_tx(&app).await?;
        let src = match &input.view_id {
            Some(id) => ExportSource::View(id),
            None => ExportSource::Where {
                where_: input.where_.as_ref(),
                project: input.project_id.as_deref(),
            },
        };
        let plan = views::export_plan(&mut tx, caller, src, input.columns.as_deref()).await?;
        // A request that was malformed has not used up one of the minute's
        // exports; one that makes the server search has.
        let slot = Slot::take(user)?;
        let ids = views::export_ids(&mut tx, &app, caller, &plan, max_rows())
            .await?
            .ok_or_else(too_large)?;
        anyhow::Ok((tx, plan, ids, slot))
    };
    let (mut tx, plan, ids, slot) = tokio::time::timeout_at(deadline.into(), prepare)
        .await
        .map_err(|_| timed_out())??;
    let project_key: Option<String> = match plan.project {
        Some(p) => {
            sqlx::query_scalar("SELECT key FROM projects WHERE id = $1")
                .bind(p)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let filename = format!(
        "{}-issues-{}.csv",
        project_key.as_deref().unwrap_or("all"),
        today_stamp(&mut tx).await?
    );
    let description = input.include_description;
    let mut labels = plan.labels.clone();
    let mut plain = plan.plain.clone();
    let mut columns = plan.columns.clone();
    if description {
        labels.push("說明".into());
        plain.push(false);
        columns.push("description".into());
    }
    let audit = Audit {
        actor: user,
        kind: "issues",
        project: plan.project,
        view: plan.view_id,
        filter: plan.audit.clone(),
        columns,
    };
    let event = record(&app, &audit, ids.len(), 0, true).await?;
    let (sender, body) = mpsc::channel(4);
    tokio::spawn(async move {
        let slot = slot;
        let mut out = Out {
            tx: sender,
            bytes: 0,
            limit: max_bytes(),
            deadline,
        };
        let mut rows = 0usize;
        let work = async {
            let mut head = BOM.to_vec();
            head.extend(csv_rows(std::slice::from_ref(&labels), &[])?);
            if !matches!(out.send(head).await, Sent::Ok) {
                return anyhow::Ok(false);
            }
            for chunk in ids.chunks(CHUNK) {
                let read = views::export_rows(&mut tx, caller, &plan, chunk, description);
                let Ok(lines) = tokio::time::timeout_at(deadline.into(), read).await else {
                    out.fail("the export took too long").await;
                    return Ok(false);
                };
                let lines = lines?;
                match out.send(csv_rows(&lines, &plain)?).await {
                    Sent::Ok => rows += lines.len(),
                    Sent::TooBig | Sent::Gone => return Ok(false),
                }
            }
            Ok(true)
        };
        let whole = match work.await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("export: {}", e.root_cause());
                out.fail("the export failed").await;
                false
            }
        };
        drop(tx);
        // R-E8: what went out, before the reader sees the end of the file.
        settle(&app, event, rows, out.bytes, !whole).await;
        // The place is free before the end of the file too.
        drop(slot);
        if whole {
            out.finish().await;
        }
    });
    Ok(Export {
        filename,
        body,
        ended: false,
    })
}

// ---------------------------------------------------------------- time entries

const TIME_HEADER: &[&str] = &[
    "日期",
    "專案",
    "單號",
    "標題",
    "使用者",
    "活動",
    "時數",
    "說明",
];
/// 日期 and 時數 are oxledger's own; the rest is text.
const TIME_PLAIN: &[bool] = &[true, false, false, false, false, false, true, false];

/// `POST /exports/time-entries`: the entries the caller may read (05
/// §3.5.4) that match the report's filters, newest first.
pub async fn time_entries(
    app: Arc<App>,
    caller: AuthContext,
    input: &TimeExport,
) -> Result<Export> {
    let user = person(caller)?;
    let slot = Slot::take(user)?;
    let deadline = Instant::now() + TIME_LIMIT;
    let q = ListQuery {
        project: input.project.clone(),
        issue: input.issue.clone(),
        user: input.user.clone(),
        activity: input.activity,
        from: input.from.clone(),
        to: input.to.clone(),
        cursor: None,
        limit: Some(200),
        tracker: input.tracker.clone(),
        fixed_version: input.fixed_version.clone(),
    };
    let prepare = async {
        let mut tx = read_tx(&app).await?;
        let (count, project) =
            time_reports::export_count(&mut tx, caller, &q, max_rows() as i64).await?;
        if count as usize > max_rows() {
            return Err(too_large());
        }
        anyhow::Ok((tx, project, count as usize))
    };
    let (mut tx, project, planned) = tokio::time::timeout_at(deadline.into(), prepare)
        .await
        .map_err(|_| timed_out())??;
    let project_key: Option<String> = match project {
        Some(p) => {
            sqlx::query_scalar("SELECT key FROM projects WHERE id = $1")
                .bind(p)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let filename = format!(
        "{}-time-entries-{}.csv",
        project_key.as_deref().unwrap_or("all"),
        today_stamp(&mut tx).await?
    );
    let audit = Audit {
        actor: user,
        kind: "time_entries",
        project,
        view: None,
        filter: json!({
            "project": project, "issue": q.issue, "user": q.user, "activity": q.activity,
            "from": q.from, "to": q.to, "tracker": q.tracker, "fixed_version": q.fixed_version,
        }),
        columns: [
            "spent_on", "project", "issue", "subject", "user", "activity", "hours", "comments",
        ]
        .iter()
        .map(|c| c.to_string())
        .collect(),
    };
    let event = record(&app, &audit, planned, 0, true).await?;
    let (sender, body) = mpsc::channel(4);
    tokio::spawn(async move {
        let slot = slot;
        let mut out = Out {
            tx: sender,
            bytes: 0,
            limit: max_bytes(),
            deadline,
        };
        let mut rows = 0usize;
        let mut q = q;
        let work = async {
            let mut head = BOM.to_vec();
            let labels: Vec<String> = TIME_HEADER.iter().map(|c| c.to_string()).collect();
            head.extend(csv_rows(std::slice::from_ref(&labels), &[])?);
            if !matches!(out.send(head).await, Sent::Ok) {
                return anyhow::Ok(false);
            }
            loop {
                let read = time_reports::export_page(&mut tx, &app, caller, &q);
                let Ok(page) = tokio::time::timeout_at(deadline.into(), read).await else {
                    out.fail("the export took too long").await;
                    return Ok(false);
                };
                let page = page?;
                let lines: Vec<Vec<String>> = page
                    .items
                    .iter()
                    .map(|e| {
                        vec![
                            e.spent_on.to_string(),
                            e.project.key.clone(),
                            e.issue.as_ref().map(|i| i.key.clone()).unwrap_or_default(),
                            e.issue
                                .as_ref()
                                .map(|i| i.subject.clone())
                                .unwrap_or_default(),
                            e.user.name.clone(),
                            e.activity.name.clone(),
                            e.hours.clone(),
                            e.comments.clone(),
                        ]
                    })
                    .collect();
                match out.send(csv_rows(&lines, TIME_PLAIN)?).await {
                    Sent::Ok => rows += lines.len(),
                    Sent::TooBig | Sent::Gone => return Ok(false),
                }
                match page.next_cursor {
                    Some(c) => q.cursor = Some(c),
                    None => return Ok(true),
                }
            }
        };
        let whole = match work.await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("export: {}", e.root_cause());
                out.fail("the export failed").await;
                false
            }
        };
        drop(tx);
        settle(&app, event, rows, out.bytes, !whole).await;
        drop(slot);
        if whole {
            out.finish().await;
        }
    });
    Ok(Export {
        filename,
        body,
        ended: false,
    })
}

// ---------------------------------------------------------------- wiki page (R-W12)

/// A wiki page was downloaded as Markdown: the audit row only (the page
/// itself is read by the wiki module, with its own checks).
pub async fn wiki_downloaded(app: &App, actor: Uuid, project: Uuid, bytes: u64) -> Result<()> {
    let a = Audit {
        actor,
        kind: "wiki_page",
        project: Some(project),
        view: None,
        filter: Json::Null,
        columns: Vec::new(),
    };
    record(app, &a, 1, bytes, false).await.map(|_| ())
}

// ---------------------------------------------------------------- GET /admin/export-events (R-E8)

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExportEvent {
    pub id: Uuid,
    pub actor: time_reports::Named,
    /// `issues`, `time_entries` or `wiki_page`.
    pub kind: String,
    /// The project's key; null for an export across projects.
    pub project: Option<String>,
    pub view_id: Option<Uuid>,
    /// Column keys, never values.
    pub columns: Vec<String>,
    pub row_count: i32,
    pub bytes: i64,
    /// The file did not go out whole: cut at the byte or time limit, an
    /// error, or the reader went away. `row_count` and `bytes` are what
    /// was sent.
    pub truncated: bool,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExportEvents {
    pub items: Vec<ExportEvent>,
    pub next_cursor: Option<String>,
}

/// System administrators: who exported what range, newest first. Neither
/// content nor conditions are kept.
pub async fn events(app: &App, caller: AuthContext, cursor: Option<&str>) -> Result<ExportEvents> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let eff = crate::db::effective(&mut tx, caller).await?;
    if !eff.is_admin {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "only a system administrator reads this",
        )
        .with_details(json!({"reason": "admin_only"}))
        .into());
    }
    let before: Option<Uuid> = match cursor {
        None => None,
        Some(c) => Some(
            c.parse()
                .map_err(|_| api(ErrorCode::InvalidCursor, "not an export-events cursor"))?,
        ),
    };
    const PAGE: usize = 50;
    type Row = (
        Uuid,
        Uuid,
        String,
        String,
        Option<String>,
        Option<Uuid>,
        Vec<String>,
        i32,
        i64,
        bool,
        DateTime<Utc>,
    );
    let mut rows: Vec<Row> = sqlx::query_as(
        "SELECT e.id, e.actor_id, u.name, e.kind, p.key, e.view_id, e.columns, e.row_count, e.bytes,
                e.truncated, e.at
         FROM export_events e JOIN principals u ON u.id = e.actor_id
         LEFT JOIN projects p ON p.id = e.project_id
         WHERE ($1::uuid IS NULL OR e.id < $1) ORDER BY e.id DESC LIMIT $2",
    )
    .bind(before)
    .bind(PAGE as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let more = rows.len() > PAGE;
    rows.truncate(PAGE);
    let next_cursor = match (more, rows.last()) {
        (true, Some(r)) => Some(r.0.to_string()),
        _ => None,
    };
    Ok(ExportEvents {
        items: rows
            .into_iter()
            .map(|r| ExportEvent {
                id: r.0,
                actor: time_reports::Named { id: r.1, name: r.2 },
                kind: r.3,
                project: r.4,
                view_id: r.5,
                columns: r.6,
                row_count: r.7,
                bytes: r.8,
                truncated: r.9,
                at: r.10,
            })
            .collect(),
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_guard() {
        for s in [
            "=HYPERLINK(\"x\")",
            "+1",
            "-cmd",
            "@SUM(A1)",
            "＝1",
            "\tx",
            "\rx",
            "＋1",
            "－1",
            "＠a",
        ] {
            assert_eq!(guard(s), format!("'{s}"), "{s:?}");
        }
        for s in ["", "a=1", "1-2", " =1", "修正"] {
            assert_eq!(guard(s), s);
        }
    }

    #[test]
    fn rfc_4180() {
        let rows = vec![vec![
            "中文，逗號,在這".to_string(),
            "兩行\n文字".to_string(),
            "他說 \"好\"".to_string(),
            "-5".to_string(),
        ]];
        let out = csv_rows(&rows, &[false, false, false, true]).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\"中文，逗號,在這\",\"兩行\n文字\",\"他說 \"\"好\"\"\",-5\r\n"
        );
    }
}
