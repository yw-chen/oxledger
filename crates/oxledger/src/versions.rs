// SPDX-License-Identifier: GPL-2.0-or-later
//! Milestones (versions) for people: contract 02 §17.2–§17.4, §17.8 (REST),
//! §17.10 (the next version) and the stats and health of §17.12.
//!
//! Reads see the version list of a visible project with `view_issues`; every
//! number counts only the issues the caller can see (§17.4). Writes need a
//! browser session, an active project and `manage_versions`, and share the
//! operation requests of §6.8 (Idempotency-Key, replay, failures leave no
//! record). Lock order (§17.6): operation_requests → principal, session FOR
//! SHARE → project FOR SHARE → version FOR UPDATE. A version transaction
//! never locks issues; issue writes take versions after issues, so there is
//! no cycle.
//!
//! The timeline (§17.12) and the threshold settings endpoints are S6b; the
//! thresholds already stored are used here.

use crate::auth::AuthContext;
use crate::commands::{self, Executor};
use crate::cursor::{self, Position};
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, MANAGE_VERSIONS, VIEW_ISSUES};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::IssueFacts;
use ox_core::visibility::Scope;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// `operation_requests.action` of the version writes.
pub const ACTION_VERSION_CREATE: &str = "version_create";
pub const ACTION_VERSION_UPDATE: &str = "version_update";
pub const ACTION_VERSION_DELETE: &str = "version_delete";
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_DESCRIPTION_CHARS: usize = 255;
pub const STATUSES: &[&str] = &["open", "locked", "closed"];
const DEFAULT_EVENTS: u32 = 50;
const MAX_EVENTS: u32 = 200;
const DEADLOCK_RETRIES: u32 = 3;

// ---------------------------------------------------------------- views

/// A version as §17.8 shows it. `deleted` versions are only returned by
/// `get`, never by lists or the next-version rule.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionView {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub description: String,
    /// `YYYY-MM-DD`; null = undated.
    pub effective_date: Option<String>,
    /// `open`, `locked` or `closed`.
    pub status: String,
    pub lock_version: i32,
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<VersionStats>,
    /// `closed`, `no_date`, `overdue`, `at_risk` or `on_track` (§17.12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_reasons: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_thresholds: Option<Thresholds>,
}

/// Counts over the issues in the version that the caller can see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VersionStats {
    pub total: i64,
    pub closed: i64,
    pub open: i64,
    /// Open and `due_date < today`.
    pub overdue: i64,
    /// `due_date > effective_date` of the version.
    pub late_for_version: i64,
    /// Neither a start nor a due date.
    pub no_dates: i64,
}

/// The thresholds in use (org default or the project override); a null
/// value turns `deadline_near_incomplete` off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Thresholds {
    pub at_risk_days: Option<i32>,
    pub at_risk_open_pct: Option<i32>,
    /// `org` or `project`.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionList {
    /// The org time zone's date (R10).
    pub today: String,
    pub versions: Vec<VersionView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<NextVersion>,
}

/// §17.10: `resolved`, `ambiguous` or `none`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NextVersion {
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionBrief>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<VersionBrief>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undated: Option<Vec<VersionBrief>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VersionBrief {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_date: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overdue: bool,
}

/// What a date or status change touches, over visible issues only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Impact {
    /// Open issues in the version.
    pub open_issues: i64,
    /// Open issues whose due date is after the new version date.
    pub due_after_new_date: i64,
    /// The version would turn `overdue` (it was not before).
    pub becomes_overdue_version: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactResult {
    pub today: String,
    pub impact: Impact,
}

/// The result of a version write. A replay returns the version as it is now
/// with the stored `execution_id`, `changed` and `impact`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionWriteResult {
    pub version: VersionView,
    /// False when nothing differed (no event, no `lock_version` bump).
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<Impact>,
    pub execution_id: Uuid,
    pub today: String,
    /// §6.9: every successful write carries the receipt (the original one on replay).
    pub search: commands::SearchReceipt,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionEvent {
    pub seq: i64,
    pub at: DateTime<Utc>,
    /// `created`, `updated` or `deleted`.
    pub kind: String,
    pub origin: String,
    pub actor: EventActor,
    /// `{field: {old, new}}`; null once redacted (D12).
    pub changes: Option<Json>,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventActor {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VersionEvents {
    pub items: Vec<VersionEvent>,
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------- inputs

/// `POST /projects/{id}/versions`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewVersion {
    pub name: String,
    #[serde(default)]
    pub effective_date: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// `PATCH /versions/{id}`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionPatch {
    pub lock_version: i32,
    pub set: VersionSet,
}

/// Absent = unchanged; `effective_date: null` clears the date.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionSet {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub effective_date: Option<Option<String>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

// ---------------------------------------------------------------- shared reads

#[derive(Debug, Clone, sqlx::FromRow)]
struct VRow {
    id: Uuid,
    project_id: Uuid,
    name: String,
    description: String,
    effective_date: Option<NaiveDate>,
    status: String,
    lock_version: i32,
    deleted: bool,
    last_event_seq: i64,
}

const VCOLS: &str = "id, project_id, name, description, effective_date, status, lock_version,
                     deleted_at IS NOT NULL AS deleted, last_event_seq";

/// One issue in a version, as far as stats need it.
struct Bit {
    version_id: Uuid,
    key: String,
    closed: bool,
    start: Option<NaiveDate>,
    due: Option<NaiveDate>,
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no version")
}

fn forbidden(reason: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

fn invalid_value(field: &str, msg: impl Into<String>, extra: Json) -> anyhow::Error {
    let mut d = json!({"field": field});
    if let (Json::Object(m), Json::Object(x)) = (&mut d, extra) {
        m.extend(x);
    }
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(d)
        .into()
}

/// R10: the org time zone's date, once per transaction.
async fn today(conn: &mut PgConnection) -> Result<NaiveDate> {
    Ok(
        sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// What a caller may do with a project's versions.
struct Access {
    scope: Scope,
    manage: bool,
    writable: bool,
}

/// The project must be visible with `view_issues` and not archived;
/// otherwise `not_found` (unknown and invisible look the same).
async fn access(conn: &mut PgConnection, caller: AuthContext, project: Uuid) -> Result<Access> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    if !scope.project_ids().any(|p| p == project) {
        return Err(not_found());
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(not_found)?;
    if pp.status == "archived" || !pp.allowed_any(VIEW_ISSUES) {
        return Err(not_found());
    }
    Ok(Access {
        manage: pp.allowed_any(MANAGE_VERSIONS),
        writable: pp.writable(),
        scope,
    })
}

/// Visible, unarchived issues of `project` in `versions`.
async fn visible_bits(
    conn: &mut PgConnection,
    scope: &Scope,
    project: Uuid,
    versions: &[Uuid],
) -> Result<Vec<Bit>> {
    type Row = (
        Uuid,
        String,
        Uuid,
        bool,
        Uuid,
        Option<Uuid>,
        bool,
        Option<NaiveDate>,
        Option<NaiveDate>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.fixed_version_id, k.key, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                s.category = 'done', i.start_date, i.due_date
         FROM issues i JOIN issue_statuses s ON s.id = i.status_id
         JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.project_id = $1 AND i.fixed_version_id = ANY($2) AND i.archived_at IS NULL
         ORDER BY k.key",
    )
    .bind(project)
    .bind(versions)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            scope.can_see(&IssueFacts {
                project_id: project,
                tracker_id: r.2,
                is_private: r.3,
                author_id: r.4,
                assigned_to_id: r.5,
            })
        })
        .map(|r| Bit {
            version_id: r.0,
            key: r.1,
            closed: r.6,
            start: r.7,
            due: r.8,
        })
        .collect())
}

async fn thresholds(conn: &mut PgConnection, project: Uuid) -> Result<Thresholds> {
    type Row = (
        Option<i32>,
        Option<i32>,
        Option<bool>,
        Option<i32>,
        Option<i32>,
    );
    let (od, op, inherit, pd, ppct): Row = sqlx::query_as(
        "SELECT o.health_at_risk_days, o.health_at_risk_open_pct,
                ps.inherit_org, ps.health_at_risk_days, ps.health_at_risk_open_pct
         FROM org_settings o LEFT JOIN project_schedule_settings ps ON ps.project_id = $1",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    Ok(if inherit == Some(false) {
        Thresholds {
            at_risk_days: pd,
            at_risk_open_pct: ppct,
            source: "project".into(),
        }
    } else {
        Thresholds {
            at_risk_days: od,
            at_risk_open_pct: op,
            source: "org".into(),
        }
    })
}

fn stats(v: &VRow, bits: &[&Bit], today: NaiveDate) -> VersionStats {
    let mut s = VersionStats::default();
    for b in bits {
        s.total += 1;
        if b.closed {
            s.closed += 1;
        } else {
            s.open += 1;
            if b.due.is_some_and(|d| d < today) {
                s.overdue += 1;
            }
        }
        if let (Some(due), Some(vd)) = (b.due, v.effective_date)
            && due > vd
        {
            s.late_for_version += 1;
        }
        if b.start.is_none() && b.due.is_none() {
            s.no_dates += 1;
        }
    }
    s
}

/// §17.12: the first matching health, and every reason that holds.
fn health(v: &VRow, bits: &[&Bit], today: NaiveDate, t: &Thresholds) -> (String, Vec<String>) {
    if v.status == "closed" {
        return ("closed".into(), Vec::new());
    }
    let Some(date) = v.effective_date else {
        return ("no_date".into(), Vec::new());
    };
    let open: Vec<&&Bit> = bits.iter().filter(|b| !b.closed).collect();
    let total = bits.len() as i64;
    let mut reasons = Vec::new();
    let version_overdue = date < today && !open.is_empty();
    if version_overdue {
        reasons.push("version_overdue".to_string());
    }
    if open.iter().any(|b| b.due.is_some_and(|d| d < today)) {
        reasons.push("issue_overdue".into());
    }
    if open.iter().any(|b| b.due.is_some_and(|d| d > date)) {
        reasons.push("issue_late_for_version".into());
    }
    if let (Some(days), Some(pct)) = (t.at_risk_days, t.at_risk_open_pct)
        && total > 0
        && (date - today).num_days() <= days as i64
        && (open.len() as i64) * 100 > (pct as i64) * total
    {
        reasons.push("deadline_near_incomplete".into());
    }
    let h = if version_overdue {
        "overdue"
    } else if reasons.is_empty() {
        "on_track"
    } else {
        "at_risk"
    };
    (h.into(), reasons)
}

fn view(v: &VRow) -> VersionView {
    VersionView {
        id: v.id,
        project_id: v.project_id,
        name: v.name.clone(),
        description: v.description.clone(),
        effective_date: v.effective_date.map(|d| d.to_string()),
        status: v.status.clone(),
        lock_version: v.lock_version,
        deleted: v.deleted,
        stats: None,
        health: None,
        health_reasons: None,
        health_thresholds: None,
    }
}

/// Views with stats and health over the visible issues.
async fn full_views(
    conn: &mut PgConnection,
    a: &Access,
    project: Uuid,
    rows: &[VRow],
    today: NaiveDate,
) -> Result<Vec<VersionView>> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let bits = visible_bits(&mut *conn, &a.scope, project, &ids).await?;
    let t = thresholds(&mut *conn, project).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mine: Vec<&Bit> = bits.iter().filter(|b| b.version_id == r.id).collect();
            let (h, reasons) = health(r, &mine, today, &t);
            VersionView {
                stats: Some(stats(r, &mine, today)),
                health: Some(h),
                health_reasons: Some(reasons),
                health_thresholds: Some(t.clone()),
                ..view(r)
            }
        })
        .collect())
}

/// §17.10 over the open, undeleted versions of a project.
fn next_version(open: &[VRow], today: NaiveDate) -> NextVersion {
    let brief = |v: &VRow, overdue: bool| VersionBrief {
        id: v.id,
        name: v.name.clone(),
        effective_date: v.effective_date.map(|d| d.to_string()),
        overdue,
    };
    let mut upcoming: Vec<&VRow> = open
        .iter()
        .filter(|v| v.effective_date.is_some_and(|d| d >= today))
        .collect();
    // `open` is sorted by (effective_date, name_key) already; keep that order.
    upcoming.sort_by_key(|v| v.effective_date);
    let overdue: Vec<&VRow> = open
        .iter()
        .filter(|v| v.effective_date.is_some_and(|d| d < today))
        .collect();
    let earliest: Vec<&VRow> = match upcoming.first() {
        Some(f) => upcoming
            .iter()
            .copied()
            .filter(|v| v.effective_date == f.effective_date)
            .collect(),
        None => Vec::new(),
    };
    let none = || NextVersion {
        outcome: String::new(),
        reason: None,
        version: None,
        candidates: None,
        undated: None,
    };
    if !overdue.is_empty() {
        let mut c: Vec<VersionBrief> = overdue.iter().map(|v| brief(v, true)).collect();
        c.extend(earliest.iter().map(|v| brief(v, false)));
        return NextVersion {
            outcome: "ambiguous".into(),
            reason: Some("overdue_open_exists".into()),
            candidates: Some(c),
            ..none()
        };
    }
    match earliest.as_slice() {
        [] => NextVersion {
            outcome: "none".into(),
            undated: Some(
                open.iter()
                    .filter(|v| v.effective_date.is_none())
                    .map(|v| brief(v, false))
                    .collect(),
            ),
            ..none()
        },
        [one] => NextVersion {
            outcome: "resolved".into(),
            version: Some(brief(one, false)),
            ..none()
        },
        many => NextVersion {
            outcome: "ambiguous".into(),
            reason: Some("same_date".into()),
            candidates: Some(many.iter().map(|v| brief(v, false)).collect()),
            ..none()
        },
    }
}

async fn version_row(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<Option<VRow>> {
    let sql = format!(
        "SELECT {VCOLS} FROM versions WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

// ---------------------------------------------------------------- reads

/// `GET /projects/{id}/versions?status=&include=stats,next`.
pub async fn list(
    app: &App,
    caller: AuthContext,
    project: &str,
    status: Option<&str>,
    include: Option<&str>,
) -> Result<VersionList> {
    let mut statuses: Vec<String> = Vec::new();
    for s in status.unwrap_or("open,locked").split(',') {
        let s = s.trim();
        if !STATUSES.contains(&s) {
            return Err(api(
                ErrorCode::InvalidArgument,
                format!("status must be among {STATUSES:?}"),
            ));
        }
        statuses.push(s.to_string());
    }
    let (mut with_stats, mut with_next) = (false, false);
    for i in include
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match i {
            "stats" => with_stats = true,
            "next" => with_next = true,
            _ => {
                return Err(api(
                    ErrorCode::InvalidArgument,
                    "include takes stats and next",
                ));
            }
        }
    }
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let today = today(&mut tx).await?;
    let all: Vec<VRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VCOLS} FROM versions WHERE project_id = $1 AND deleted_at IS NULL
         ORDER BY effective_date NULLS LAST, name_key"
    )))
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    let rows: Vec<VRow> = all
        .iter()
        .filter(|v| statuses.contains(&v.status))
        .cloned()
        .collect();
    let versions = if with_stats {
        full_views(&mut tx, &a, project, &rows, today).await?
    } else {
        rows.iter().map(view).collect()
    };
    let next = with_next.then(|| {
        let open: Vec<VRow> = all.iter().filter(|v| v.status == "open").cloned().collect();
        next_version(&open, today)
    });
    tx.commit().await?;
    Ok(VersionList {
        today: today.to_string(),
        versions,
        next,
    })
}

/// `GET /versions/{id}`: one version (deleted ones too) with stats.
pub async fn get(app: &App, caller: AuthContext, id: Uuid) -> Result<VersionView> {
    let mut tx = read_tx(app).await?;
    let v = version_row(&mut tx, id, false)
        .await?
        .ok_or_else(not_found)?;
    let a = access(&mut tx, caller, v.project_id).await?;
    let today = today(&mut tx).await?;
    let mut views = full_views(&mut tx, &a, v.project_id, std::slice::from_ref(&v), today).await?;
    tx.commit().await?;
    Ok(views.remove(0))
}

/// `GET /versions/{id}/events?cursor=&limit=`, oldest first.
pub async fn events(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    cursor_text: Option<&str>,
    limit: Option<u32>,
) -> Result<VersionEvents> {
    let limit = limit.unwrap_or(DEFAULT_EVENTS).clamp(1, MAX_EVENTS) as i64;
    let qhash = cursor::query_hash(&json!({"version_events": id}));
    let after = match cursor_text {
        None => 0,
        Some(c) => match cursor::decode(&app.pool, c, caller.principal_id(), &qhash).await? {
            Position::Seq { s } => s,
            _ => return Err(api(ErrorCode::InvalidCursor, "invalid cursor")),
        },
    };
    let mut tx = read_tx(app).await?;
    let v = version_row(&mut tx, id, false)
        .await?
        .ok_or_else(not_found)?;
    access(&mut tx, caller, v.project_id).await?;
    type Row = (
        i64,
        DateTime<Utc>,
        String,
        String,
        Uuid,
        String,
        String,
        Json,
        bool,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT e.seq, e.at, e.kind, e.origin, e.actor_id, p.name, e.actor_kind, e.payload,
                e.redaction_id IS NOT NULL
         FROM version_events e JOIN principals p ON p.id = e.actor_id
         WHERE e.version_id = $1 AND e.seq > $2 ORDER BY e.seq LIMIT $3",
    )
    .bind(id)
    .bind(after)
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let more = rows.len() as i64 > limit;
    let items: Vec<VersionEvent> = rows
        .into_iter()
        .take(limit as usize)
        .map(
            |(seq, at, kind, origin, aid, name, akind, payload, redacted)| VersionEvent {
                seq,
                at,
                kind,
                origin,
                actor: EventActor {
                    id: aid,
                    name,
                    kind: akind,
                },
                changes: if redacted {
                    None
                } else {
                    Some(payload["changes"].clone())
                },
                redacted,
            },
        )
        .collect();
    let next_cursor = match (more, items.last()) {
        (true, Some(last)) => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                Position::Seq { s: last.seq },
            )
            .await?,
        ),
        _ => None,
    };
    Ok(VersionEvents { items, next_cursor })
}

/// `GET /versions/{id}/impact?effective_date=&status=`: the preview of a
/// change, nothing written. `effective_date` empty = clear it.
pub async fn impact(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    effective_date: Option<&str>,
    status: Option<&str>,
) -> Result<ImpactResult> {
    let new_date = match effective_date {
        None => None,
        Some("") => Some(None),
        Some(d) => Some(Some(parse_date(d)?)),
    };
    if let Some(s) = status
        && !STATUSES.contains(&s)
    {
        return Err(invalid_value(
            "status",
            "status is open, locked or closed",
            json!({}),
        ));
    }
    let mut tx = read_tx(app).await?;
    let v = version_row(&mut tx, id, false)
        .await?
        .ok_or_else(not_found)?;
    if v.deleted {
        return Err(not_found());
    }
    let a = access(&mut tx, caller, v.project_id).await?;
    if !a.manage {
        return Err(forbidden("permission", "manage_versions is needed"));
    }
    let today = today(&mut tx).await?;
    let date = new_date.unwrap_or(v.effective_date);
    let status = status.unwrap_or(&v.status).to_string();
    let i = compute_impact(&mut tx, &a, &v, date, &status, today).await?;
    tx.commit().await?;
    Ok(ImpactResult {
        today: today.to_string(),
        impact: i,
    })
}

async fn compute_impact(
    conn: &mut PgConnection,
    a: &Access,
    v: &VRow,
    date: Option<NaiveDate>,
    status: &str,
    today: NaiveDate,
) -> Result<Impact> {
    let bits = visible_bits(&mut *conn, &a.scope, v.project_id, &[v.id]).await?;
    let open: Vec<&Bit> = bits.iter().filter(|b| !b.closed).collect();
    let overdue = |d: Option<NaiveDate>, s: &str| {
        s != "closed" && d.is_some_and(|d| d < today) && !open.is_empty()
    };
    Ok(Impact {
        open_issues: open.len() as i64,
        due_after_new_date: match date {
            Some(d) => open.iter().filter(|b| b.due.is_some_and(|x| x > d)).count() as i64,
            None => 0,
        },
        becomes_overdue_version: overdue(date, status) && !overdue(v.effective_date, &v.status),
    })
}

// ---------------------------------------------------------------- writes

fn parse_date(s: &str) -> Result<NaiveDate> {
    let ok_shape = s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-';
    let d = ok_shape
        .then(|| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .flatten()
        .ok_or_else(|| {
            ApiError::new(ErrorCode::InvalidRequest, "dates are YYYY-MM-DD")
                .with_details(json!({"field": "effective_date"}))
        })?;
    let lo = NaiveDate::from_ymd_opt(1900, 1, 1).expect("date");
    let hi = NaiveDate::from_ymd_opt(2199, 12, 31).expect("date");
    if d < lo || d > hi {
        return Err(invalid_value(
            "effective_date",
            "dates are between 1900-01-01 and 2199-12-31",
            json!({"kind": "date_out_of_range"}),
        ));
    }
    Ok(d)
}

/// A name checked and normalized (NFC, trimmed) with its `name_key`
/// (casefolded), both computed by PostgreSQL.
async fn name_and_key(conn: &mut PgConnection, name: &str) -> Result<(String, String)> {
    let trimmed = name.trim();
    let (n, k): (String, String) =
        sqlx::query_as("SELECT normalize($1, NFC), casefold(normalize($1, NFC))")
            .bind(trimmed)
            .fetch_one(&mut *conn)
            .await?;
    let chars = n.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS {
        return Err(invalid_value(
            "name",
            format!("a version name is 1 to {MAX_NAME_CHARS} characters"),
            json!({}),
        ));
    }
    Ok((n, k))
}

fn check_description(d: &str) -> Result<()> {
    if d.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(invalid_value(
            "description",
            format!("a description is at most {MAX_DESCRIPTION_CHARS} characters"),
            json!({}),
        ));
    }
    Ok(())
}

fn check_status(s: &str) -> Result<()> {
    if !STATUSES.contains(&s) {
        return Err(invalid_value(
            "status",
            "status is open, locked or closed",
            json!({}),
        ));
    }
    Ok(())
}

async fn taken(
    conn: &mut PgConnection,
    project: Uuid,
    key: &str,
    except: Option<Uuid>,
) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM versions WHERE project_id = $1 AND name_key = $2 AND deleted_at IS NULL
           AND id IS DISTINCT FROM $3",
    )
    .bind(project)
    .bind(key)
    .bind(except)
    .fetch_optional(&mut *conn)
    .await?)
}

fn name_taken(id: Uuid) -> anyhow::Error {
    ApiError::new(
        ErrorCode::VersionNameTaken,
        "another version of this project has that name",
    )
    .with_details(json!({"version_id": id}))
    .into()
}

fn hash(v: &Json) -> [u8; 32] {
    // serde_json maps are sorted, so this is canonical.
    Sha256::digest(
        [
            b"oxledger/version/v1\n".as_slice(),
            v.to_string().as_bytes(),
        ]
        .concat(),
    )
    .into()
}

/// Runs `$body` (an expression using the transaction `$tx`) with the
/// deadlock retries and lock timeout of §3.7.
macro_rules! retrying {
    ($app:expr, $tx:ident, $body:expr) => {{
        let mut attempt = 0;
        loop {
            let mut $tx = $app.pool.begin().await?;
            sqlx::query("SET LOCAL lock_timeout = '10s'")
                .execute(&mut *$tx)
                .await?;
            match $body {
                Ok(v) => {
                    $tx.commit().await?;
                    break Ok(v);
                }
                Err(e) => {
                    drop($tx);
                    if commands::is_retryable_db(&e) && attempt < DEADLOCK_RETRIES {
                        attempt += 1;
                        continue;
                    }
                    if commands::is_lock_timeout(&e) {
                        break Err(api(
                            ErrorCode::TemporarilyUnavailable,
                            "the version is busy; send the same request again",
                        ));
                    }
                    break Err(e);
                }
            }
        }
    }};
}

/// The version write's shared beginning: the operation request (or the
/// replay of a used key), then the executor's locks. `Err(Ok(result))`
/// short-circuits with a replay.
async fn begin(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    action: &str,
    hash: &[u8; 32],
) -> Result<Result<(Uuid, Option<DateTime<Utc>>), VersionWriteResult>> {
    let fp = commands::fingerprint(action, hash);
    let Some(request_id) =
        commands::open_request(&mut *conn, who.user_id, operation_id, action, &fp).await?
    else {
        let r = commands::stored_result(&mut *conn, who.user_id, operation_id, action, &fp).await?;
        return replay(&mut *conn, caller, &r).await.map(Err);
    };
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    Ok(Ok((request_id, deadline)))
}

async fn replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    r: &Json,
) -> Result<VersionWriteResult> {
    let id = r["version_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(not_found)?;
    let v = version_row(&mut *conn, id, false)
        .await?
        .ok_or_else(not_found)?;
    let a = access(&mut *conn, caller, v.project_id).await?;
    let today = today(&mut *conn).await?;
    let mut views = full_views(
        &mut *conn,
        &a,
        v.project_id,
        std::slice::from_ref(&v),
        today,
    )
    .await?;
    Ok(VersionWriteResult {
        version: views.remove(0),
        changed: r["changed"].as_bool().unwrap_or(false),
        impact: serde_json::from_value::<ImpactStored>(r["impact"].clone())
            .ok()
            .map(Into::into),
        execution_id: r["execution_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_default(),
        today: today.to_string(),
        search: commands::SearchReceipt {
            consistent_after: r["consistent_after"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
    })
}

#[derive(Deserialize)]
struct ImpactStored {
    open_issues: i64,
    due_after_new_date: i64,
    becomes_overdue_version: bool,
}

impl From<ImpactStored> for Impact {
    fn from(i: ImpactStored) -> Self {
        Impact {
            open_issues: i.open_issues,
            due_after_new_date: i.due_after_new_date,
            becomes_overdue_version: i.becomes_overdue_version,
        }
    }
}

/// The project row FOR SHARE (§17.6), then the deadline, then access.
async fn lock_project(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    deadline: Option<DateTime<Utc>>,
) -> Result<(Access, String)> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .fetch_optional(&mut *conn)
        .await?;
    exists.ok_or_else(not_found)?;
    access_after_locks(conn, caller, project, deadline).await
}

/// After the project and version locks: step 8b (the search epoch FOR
/// SHARE, as the receipt token), then the deadline, then access.
async fn access_after_locks(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    deadline: Option<DateTime<Utc>>,
) -> Result<(Access, String)> {
    let token = commands::search_token(&mut *conn).await?;
    commands::check_deadline(&mut *conn, deadline).await?;
    let a = access(&mut *conn, caller, project).await?;
    if !a.writable {
        return Err(forbidden(
            "project_not_writable",
            "the project does not accept changes",
        ));
    }
    if !a.manage {
        return Err(forbidden("permission", "manage_versions is needed"));
    }
    Ok((a, token))
}

#[allow(clippy::too_many_arguments)]
async fn write_event(
    conn: &mut PgConnection,
    version: Uuid,
    seq: i64,
    kind: &str,
    who: Uuid,
    execution_id: Uuid,
    request_id: Uuid,
    changes: &Map<String, Json>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO version_events (version_id, seq, kind, origin, actor_id, actor_kind,
                                     execution_id, operation_request_id, payload)
         VALUES ($1, $2, $3, 'human_direct', $4, 'user', $5, $6, $7)",
    )
    .bind(version)
    .bind(seq)
    .bind(kind)
    .bind(who)
    .bind(execution_id)
    .bind(request_id)
    .bind(json!({"changes": changes}))
    .execute(&mut *conn)
    .await?;
    for field in changes.keys() {
        sqlx::query(
            "INSERT INTO version_field_origins (version_id, field_key, origin_execution_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (version_id, field_key) DO UPDATE SET origin_execution_id = EXCLUDED.origin_execution_id",
        )
        .bind(version)
        .bind(field)
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn finish(
    conn: &mut PgConnection,
    request_id: Uuid,
    who: Uuid,
    hash: &[u8; 32],
    token: &str,
) -> Result<(Uuid, String)> {
    let execution_id =
        commands::record_human_execution(&mut *conn, request_id, who, hash, token).await?;
    Ok((execution_id, token.to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn result(
    conn: &mut PgConnection,
    a: &Access,
    request_id: Uuid,
    v: &VRow,
    changed: bool,
    impact: Option<Impact>,
    execution_id: Uuid,
    consistent_after: &str,
) -> Result<VersionWriteResult> {
    commands::complete_request(
        &mut *conn,
        request_id,
        &json!({"version_id": v.id, "execution_id": execution_id, "changed": changed,
                "lock_version": v.lock_version, "impact": impact,
                "consistent_after": consistent_after}),
    )
    .await?;
    let today = today(&mut *conn).await?;
    let mut views = full_views(&mut *conn, a, v.project_id, std::slice::from_ref(v), today).await?;
    Ok(VersionWriteResult {
        version: views.remove(0),
        changed,
        impact,
        execution_id,
        today: today.to_string(),
        search: commands::SearchReceipt {
            consistent_after: consistent_after.to_string(),
        },
    })
}

/// `POST /projects/{id}/versions`.
pub async fn create(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &NewVersion,
) -> Result<VersionWriteResult> {
    let who = commands::executor(&caller)?;
    retrying!(
        app,
        tx,
        create_in(&mut tx, caller, &who, operation_id, project, input).await
    )
}

async fn create_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    project: &str,
    input: &NewVersion,
) -> Result<VersionWriteResult> {
    let project = describe::visible_project(&mut *conn, caller, project)
        .await
        .map_err(|_| not_found())?;
    let h = hash(&json!({
        "op": "create", "project_id": project, "name": input.name.trim(),
        "effective_date": input.effective_date, "description": input.description,
        "status": input.status,
    }));
    let (request_id, deadline) = match begin(
        &mut *conn,
        caller,
        who,
        operation_id,
        ACTION_VERSION_CREATE,
        &h,
    )
    .await?
    {
        Ok(x) => x,
        Err(replayed) => return Ok(replayed),
    };
    let (a, token) = lock_project(&mut *conn, caller, project, deadline).await?;
    let (name, key) = name_and_key(&mut *conn, &input.name).await?;
    let date = input
        .effective_date
        .as_deref()
        .map(parse_date)
        .transpose()?;
    let description = input.description.clone().unwrap_or_default();
    check_description(&description)?;
    let status = input.status.clone().unwrap_or_else(|| "open".into());
    check_status(&status)?;
    if let Some(id) = taken(&mut *conn, project, &key, None).await? {
        return Err(name_taken(id));
    }
    // A concurrent create of the same name waits on the unique index; the
    // loser sees the winner's row afterwards.
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO versions (project_id, name, name_key, description, effective_date, status,
                               created_by, last_event_seq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 1)
         ON CONFLICT (project_id, name_key) WHERE deleted_at IS NULL DO NOTHING RETURNING id",
    )
    .bind(project)
    .bind(&name)
    .bind(&key)
    .bind(&description)
    .bind(date)
    .bind(&status)
    .bind(who.user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = inserted else {
        let other = taken(&mut *conn, project, &key, None)
            .await?
            .unwrap_or_default();
        return Err(name_taken(other));
    };
    let (execution_id, token) = finish(&mut *conn, request_id, who.user_id, &h, &token).await?;
    let mut changes = Map::new();
    changes.insert("name".into(), json!({"old": null, "new": name}));
    changes.insert(
        "description".into(),
        json!({"old": null, "new": description}),
    );
    changes.insert(
        "effective_date".into(),
        json!({"old": null, "new": date.map(|d| d.to_string())}),
    );
    changes.insert("status".into(), json!({"old": null, "new": status}));
    write_event(
        &mut *conn,
        id,
        1,
        "created",
        who.user_id,
        execution_id,
        request_id,
        &changes,
    )
    .await?;
    let v = version_row(&mut *conn, id, false)
        .await?
        .ok_or_else(not_found)?;
    result(
        &mut *conn,
        &a,
        request_id,
        &v,
        true,
        None,
        execution_id,
        &token,
    )
    .await
}

/// `PATCH /versions/{id}`.
pub async fn update(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    patch: &VersionPatch,
) -> Result<VersionWriteResult> {
    let who = commands::executor(&caller)?;
    retrying!(
        app,
        tx,
        update_in(&mut tx, caller, &who, operation_id, id, patch).await
    )
}

/// Locks the version for a change: project FOR SHARE, version FOR UPDATE,
/// the deadline and access; deleted or invisible → `not_found`; a stale
/// `lock_version` → `conflict`.
async fn lock_version_row(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    expected: i32,
    deadline: Option<DateTime<Utc>>,
) -> Result<(VRow, Access, String)> {
    let project: Uuid = sqlx::query_scalar("SELECT project_id FROM versions WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(not_found)?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *conn)
        .await?;
    let v = version_row(&mut *conn, id, true)
        .await?
        .ok_or_else(not_found)?;
    let a = access_after_locks(&mut *conn, caller, project, deadline).await;
    // Invisible and deleted look the same; permission errors only on a
    // version the caller can see.
    if v.deleted {
        return Err(not_found());
    }
    let (a, token) = a?;
    if v.lock_version != expected {
        return Err(
            ApiError::new(ErrorCode::Conflict, "the version changed meanwhile")
                .with_details(json!({"lock_version": v.lock_version}))
                .into(),
        );
    }
    Ok((v, a, token))
}

async fn update_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    id: Uuid,
    patch: &VersionPatch,
) -> Result<VersionWriteResult> {
    let s = &patch.set;
    let h = hash(&json!({
        "op": "update", "version_id": id, "lock_version": patch.lock_version,
        "name": s.name.as_deref().map(str::trim), "description": s.description,
        "status": s.status,
        "effective_date": s.effective_date.as_ref().map(|d| json!(d)).unwrap_or(json!("(unchanged)")),
    }));
    let (request_id, deadline) = match begin(
        &mut *conn,
        caller,
        who,
        operation_id,
        ACTION_VERSION_UPDATE,
        &h,
    )
    .await?
    {
        Ok(x) => x,
        Err(replayed) => return Ok(replayed),
    };
    let (v, a, token) =
        lock_version_row(&mut *conn, caller, id, patch.lock_version, deadline).await?;

    let mut changes = Map::new();
    let mut new_name = (v.name.clone(), None::<String>);
    if let Some(n) = &s.name {
        let (name, key) = name_and_key(&mut *conn, n).await?;
        if name != v.name {
            if let Some(other) = taken(&mut *conn, v.project_id, &key, Some(v.id)).await? {
                return Err(name_taken(other));
            }
            changes.insert("name".into(), json!({"old": v.name, "new": name}));
            new_name = (name, Some(key));
        }
    }
    let mut description = v.description.clone();
    if let Some(d) = &s.description {
        check_description(d)?;
        if *d != v.description {
            changes.insert(
                "description".into(),
                json!({"old": v.description, "new": d}),
            );
            description = d.clone();
        }
    }
    let mut date = v.effective_date;
    if let Some(d) = &s.effective_date {
        let nd = d.as_deref().map(parse_date).transpose()?;
        if nd != v.effective_date {
            changes.insert(
                "effective_date".into(),
                json!({"old": v.effective_date.map(|x| x.to_string()), "new": nd.map(|x| x.to_string())}),
            );
            date = nd;
        }
    }
    let mut status = v.status.clone();
    if let Some(st) = &s.status {
        check_status(st)?;
        if *st != v.status {
            changes.insert("status".into(), json!({"old": v.status, "new": st}));
            status = st.clone();
        }
    }
    let impact = if s.effective_date.is_some() || s.status.is_some() {
        let today = today(&mut *conn).await?;
        Some(compute_impact(&mut *conn, &a, &v, date, &status, today).await?)
    } else {
        None
    };
    let (execution_id, token) = finish(&mut *conn, request_id, who.user_id, &h, &token).await?;
    let changed = !changes.is_empty();
    if changed {
        let seq = v.last_event_seq + 1;
        sqlx::query(
            "UPDATE versions SET name = $2, name_key = COALESCE($3, name_key), description = $4,
                    effective_date = $5, status = $6, lock_version = lock_version + 1,
                    last_event_seq = $7, updated_at = now()
             WHERE id = $1",
        )
        .bind(v.id)
        .bind(&new_name.0)
        .bind(&new_name.1)
        .bind(&description)
        .bind(date)
        .bind(&status)
        .bind(seq)
        .execute(&mut *conn)
        .await
        .map_err(|e| unique_to_taken(e, v.id))?;
        write_event(
            &mut *conn,
            v.id,
            seq,
            "updated",
            who.user_id,
            execution_id,
            request_id,
            &changes,
        )
        .await?;
    }
    let v = version_row(&mut *conn, id, false)
        .await?
        .ok_or_else(not_found)?;
    result(
        &mut *conn,
        &a,
        request_id,
        &v,
        changed,
        impact,
        execution_id,
        &token,
    )
    .await
}

/// A rename that raced another one into the same name.
fn unique_to_taken(e: sqlx::Error, id: Uuid) -> anyhow::Error {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => name_taken(id),
        _ => e.into(),
    }
}

/// `DELETE /versions/{id}?lock_version=` (R5): a soft delete, refused while
/// an issue the caller can see is in the version.
pub async fn delete(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    lock_version: i32,
) -> Result<VersionWriteResult> {
    let who = commands::executor(&caller)?;
    retrying!(
        app,
        tx,
        delete_in(&mut tx, caller, &who, operation_id, id, lock_version).await
    )
}

async fn delete_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    id: Uuid,
    lock_version: i32,
) -> Result<VersionWriteResult> {
    let h = hash(&json!({"op": "delete", "version_id": id, "lock_version": lock_version}));
    let (request_id, deadline) = match begin(
        &mut *conn,
        caller,
        who,
        operation_id,
        ACTION_VERSION_DELETE,
        &h,
    )
    .await?
    {
        Ok(x) => x,
        Err(replayed) => return Ok(replayed),
    };
    let (v, a, token) = lock_version_row(&mut *conn, caller, id, lock_version, deadline).await?;
    let used: Vec<String> = visible_bits(&mut *conn, &a.scope, v.project_id, &[v.id])
        .await?
        .into_iter()
        .map(|b| b.key)
        .collect();
    if !used.is_empty() {
        return Err(ApiError::new(
            ErrorCode::VersionInUse,
            "issues are still in this version; move them first",
        )
        .with_details(json!({"issues": used}))
        .into());
    }
    let (execution_id, token) = finish(&mut *conn, request_id, who.user_id, &h, &token).await?;
    let seq = v.last_event_seq + 1;
    sqlx::query(
        "UPDATE versions SET deleted_at = now(), lock_version = lock_version + 1,
                last_event_seq = $2, updated_at = now()
         WHERE id = $1",
    )
    .bind(v.id)
    .bind(seq)
    .execute(&mut *conn)
    .await?;
    write_event(
        &mut *conn,
        v.id,
        seq,
        "deleted",
        who.user_id,
        execution_id,
        request_id,
        &Map::new(),
    )
    .await?;
    let v = version_row(&mut *conn, id, false)
        .await?
        .ok_or_else(not_found)?;
    result(
        &mut *conn,
        &a,
        request_id,
        &v,
        true,
        None,
        execution_id,
        &token,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, date: Option<&str>) -> VRow {
        VRow {
            id: Uuid::now_v7(),
            project_id: Uuid::nil(),
            name: name.into(),
            description: String::new(),
            effective_date: date.map(|d| d.parse().unwrap()),
            status: "open".into(),
            lock_version: 0,
            deleted: false,
            last_event_seq: 0,
        }
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn next_version_rules() {
        let today = d("2026-11-07");
        // overdue open version → ambiguous, with the earliest upcoming.
        let n = next_version(
            &[
                v("v1.9", Some("2026-10-30")),
                v("v2.0", Some("2026-11-20")),
                v("v3", Some("2026-12-01")),
            ],
            today,
        );
        assert_eq!(n.outcome, "ambiguous");
        assert_eq!(n.reason.as_deref(), Some("overdue_open_exists"));
        let c = n.candidates.unwrap();
        assert_eq!(
            c.iter()
                .map(|b| (b.name.as_str(), b.overdue))
                .collect::<Vec<_>>(),
            [("v1.9", true), ("v2.0", false)]
        );
        // two on the same date
        let n = next_version(
            &[v("a", Some("2026-11-20")), v("b", Some("2026-11-20"))],
            today,
        );
        assert_eq!(
            (n.outcome.as_str(), n.candidates.unwrap().len()),
            ("ambiguous", 2)
        );
        // only undated
        let n = next_version(&[v("Backlog", None)], today);
        assert_eq!(n.outcome, "none");
        assert_eq!(n.undated.unwrap()[0].name, "Backlog");
        // nothing open
        let n = next_version(&[], today);
        assert_eq!((n.outcome.as_str(), n.undated.unwrap().len()), ("none", 0));
        // today counts as upcoming
        let n = next_version(
            &[v("now", Some("2026-11-07")), v("later", Some("2026-12-01"))],
            today,
        );
        assert_eq!(
            (n.outcome.as_str(), n.version.unwrap().name.as_str()),
            ("resolved", "now")
        );
    }

    fn bit(closed: bool, start: Option<&str>, due: Option<&str>) -> Bit {
        Bit {
            version_id: Uuid::nil(),
            key: String::new(),
            closed,
            start: start.map(d),
            due: due.map(d),
        }
    }

    #[test]
    fn health_table() {
        let today = d("2026-11-07");
        let t = Thresholds {
            at_risk_days: Some(7),
            at_risk_open_pct: Some(50),
            source: "org".into(),
        };
        let run = |status: &str, date: Option<&str>, bits: &[Bit]| {
            let mut ver = v("x", date);
            ver.status = status.into();
            let refs: Vec<&Bit> = bits.iter().collect();
            health(&ver, &refs, today, &t)
        };
        assert_eq!(
            run("closed", Some("2026-10-01"), &[bit(false, None, None)]).0,
            "closed"
        );
        assert_eq!(run("open", None, &[bit(false, None, None)]).0, "no_date");
        let (h, r) = run("open", Some("2026-11-01"), &[bit(false, None, None)]);
        // every reason that holds is listed: the past date is also "near" (§17.12)
        assert_eq!(
            (h.as_str(), r),
            (
                "overdue",
                vec![
                    "version_overdue".to_string(),
                    "deadline_near_incomplete".into()
                ]
            )
        );
        // past date, everything closed: on_track
        assert_eq!(
            run("open", Some("2026-11-01"), &[bit(true, None, None)]).0,
            "on_track"
        );
        let (h, r) = run(
            "open",
            Some("2026-12-31"),
            &[bit(false, None, Some("2026-11-01"))],
        );
        assert_eq!(
            (h.as_str(), r),
            ("at_risk", vec!["issue_overdue".to_string()])
        );
        let (h, r) = run(
            "open",
            Some("2026-12-01"),
            &[bit(false, None, Some("2026-12-05"))],
        );
        assert_eq!(
            (h.as_str(), r),
            ("at_risk", vec!["issue_late_for_version".to_string()])
        );
        // 5 days left, 2 of 3 open (66% > 50%)
        let b = [
            bit(false, None, None),
            bit(false, None, None),
            bit(true, None, None),
        ];
        let (h, r) = run("open", Some("2026-11-12"), &b);
        assert_eq!(
            (h.as_str(), r),
            ("at_risk", vec!["deadline_near_incomplete".to_string()])
        );
        // exactly 50% is not above the threshold
        let b = [bit(false, None, None), bit(true, None, None)];
        assert_eq!(run("open", Some("2026-11-12"), &b).0, "on_track");
        // a closed issue due after the version date is not a health reason
        assert_eq!(
            run(
                "open",
                Some("2026-12-01"),
                &[bit(true, None, Some("2026-12-05"))]
            )
            .0,
            "on_track"
        );
        // no issues at all
        assert_eq!(run("open", Some("2026-11-12"), &[]).0, "on_track");
        let st = stats(
            &v("x", Some("2026-11-10")),
            &[
                &bit(false, None, Some("2026-11-01")),
                &bit(true, None, Some("2026-11-20")),
                &bit(false, None, None),
            ],
            today,
        );
        assert_eq!(
            st,
            VersionStats {
                total: 3,
                closed: 1,
                open: 2,
                overdue: 1,
                late_for_version: 1,
                no_dates: 1
            }
        );
    }

    #[test]
    fn dates() {
        assert!(parse_date("2026-11-20").is_ok());
        assert!(parse_date("2026-2-1").is_err());
        assert!(parse_date("2200-01-01").is_err());
    }
}
