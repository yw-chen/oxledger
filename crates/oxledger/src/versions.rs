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
use crate::commands::{
    self, Attribution, EvalCtx, Executor, ItemResult, VersionCmd, VersionEffect, VersionFields,
};
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
    /// The caller may create and change versions here (`manage_versions`);
    /// the SPA hides those controls otherwise.
    pub can_manage: bool,
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
pub(crate) async fn today(conn: &mut PgConnection) -> Result<NaiveDate> {
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

/// Whether `caller` can read the versions of `project` now
/// (`GET /projects/{id}/versions`; the home `milestones` widget, M1.6).
pub(crate) async fn readable(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<bool> {
    match access(&mut *conn, caller, project).await {
        Ok(_) => Ok(true),
        Err(e) if crate::error::classify(&e).is_some() => Ok(false),
        Err(e) => Err(e),
    }
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

/// Search `version:` (§17.15) inside one readable project: a name (NFC,
/// casefolded, like R8) or `next` (§17.10). Unknown name or no next version
/// → `invalid_filter` (`details.path`); ambiguous next →
/// `ambiguous_reference` with the candidates.
pub(crate) async fn resolve_for_search(
    conn: &mut PgConnection,
    project: Uuid,
    text: &str,
) -> Result<Uuid> {
    let text = text.trim();
    let bad = |reason: Option<&str>, msg: String| -> anyhow::Error {
        let mut d = json!({"path": "version"});
        if let Some(r) = reason {
            d["reason"] = json!(r);
        }
        ApiError::new(ErrorCode::InvalidFilter, msg)
            .with_details(d)
            .into()
    };
    if text.eq_ignore_ascii_case("next") {
        let today = today(&mut *conn).await?;
        let open: Vec<VRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {VCOLS} FROM versions WHERE project_id = $1 AND deleted_at IS NULL
               AND status = 'open' ORDER BY effective_date NULLS LAST, name_key"
        )))
        .bind(project)
        .fetch_all(&mut *conn)
        .await?;
        let n = next_version(&open, today);
        return match (n.outcome.as_str(), n.version) {
            ("resolved", Some(v)) => Ok(v.id),
            ("ambiguous", _) => Err(ApiError::new(
                ErrorCode::AmbiguousReference,
                "version:next is ambiguous here; name the version",
            )
            .with_details(json!({"path": "version", "reason": n.reason,
                                 "candidates": n.candidates}))
            .into()),
            _ => Err(bad(
                Some("no_next_version"),
                "this project has no next version (open, dated, not overdue)".into(),
            )),
        };
    }
    let key: String = sqlx::query_scalar("SELECT casefold(normalize($1, NFC))")
        .bind(text)
        .fetch_one(&mut *conn)
        .await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM versions WHERE project_id = $1 AND name_key = $2 AND deleted_at IS NULL",
    )
    .bind(project)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    id.ok_or_else(|| bad(None, format!("no version {text:?} in this project")))
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
        can_manage: a.manage,
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
    // `version:next` is a keyword (§17.10); a version by that name could
    // never be searched by name.
    if k == "next" {
        return Err(invalid_value(
            "name",
            "\"next\" is reserved for version:next; pick another name",
            json!({"kind": "reserved_name"}),
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

/// One version event with its attribution (§16.2), then the field origins.
async fn write_event(
    conn: &mut PgConnection,
    version: Uuid,
    seq: i64,
    kind: &str,
    at: &Attribution,
    changes: &Map<String, Json>,
) -> Result<()> {
    let execution_id = at.execution_ref;
    sqlx::query(
        "INSERT INTO version_events (version_id, seq, kind, origin, actor_id, actor_kind,
                                     on_behalf_of_id, approver_id, delegation_id,
                                     execution_id, operation_request_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(version)
    .bind(seq)
    .bind(kind)
    .bind(at.origin)
    .bind(at.actor_id)
    .bind(at.actor_kind)
    .bind(at.on_behalf_of_id)
    .bind(at.approver_id)
    .bind(at.delegation_id)
    .bind(execution_id)
    .bind(at.request_id)
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
        &Attribution::human(who.user_id, request_id, execution_id),
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
            &Attribution::human(who.user_id, request_id, execution_id),
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
        &Attribution::human(who.user_id, request_id, execution_id),
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

// ---------------------------------------------------------------- agent version commands (§17.9a)

/// A version document's write, planned by [`plan`] (under the locks in
/// Direct and Apply) and done by [`write_plan`].
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    op_id: Uuid,
    pub id: Uuid,
    pub project_id: Uuid,
    pub created: bool,
    name: String,
    /// `Some` when the name (key) changes or is new.
    key: Option<String>,
    description: String,
    date: Option<NaiveDate>,
    status: String,
    changes: Map<String, Json>,
    /// The event's seq (the version's last + 1; 1 for a new one).
    seq: i64,
    lock_version: i32,
}

impl Plan {
    pub(crate) fn item(&self) -> ItemResult {
        ItemResult {
            op_id: self.op_id,
            status: if self.changes.is_empty() {
                "no_effect"
            } else {
                "applied"
            }
            .into(),
            issue_id: None,
            version_id: Some(self.id),
            time_entry_id: None,
        }
    }

    pub(crate) fn effect(&self) -> VersionEffect {
        VersionEffect {
            version_id: self.id,
            project_id: self.project_id,
            lock_version: self.lock_version,
            changed: !self.changes.is_empty(),
            created: self.created,
        }
    }
}

fn op_forbidden(op_id: Uuid, reason: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason, "op_index": 0, "op_id": op_id}))
        .into()
}

/// The project (and version) of a version command for the agent's checks
/// (§16.6 quotas, the audience): `(project, Some(version))`; `None` when
/// the version does not exist (the plan reports `not_found`).
pub(crate) async fn cmd_project(
    conn: &mut PgConnection,
    cmd: VersionCmd<'_>,
) -> Result<Option<Uuid>> {
    Ok(match cmd {
        VersionCmd::Create { project_id, .. } => Some(project_id),
        VersionCmd::Update { version_id, .. } => {
            sqlx::query_scalar("SELECT project_id FROM versions WHERE id = $1")
                .bind(version_id)
                .fetch_optional(&mut *conn)
                .await?
        }
    })
}

/// §17.9a: locks (project FOR SHARE, then the version FOR UPDATE for an
/// update), deadlines, visibility, permission (`manage_versions` of the
/// delegator ∩ the delegation or the proposal's ceiling), the precondition
/// and the values (R8 names, R9 dates). Returns the plan and, when locking,
/// the receipt token.
pub(crate) async fn plan(
    conn: &mut PgConnection,
    ectx: &EvalCtx,
    cmd: VersionCmd<'_>,
) -> Result<(Plan, Option<String>)> {
    let locks = ectx.mode.locks();
    let (op_id, kind) = match cmd {
        VersionCmd::Create { op_id, .. } => (op_id, perm::OP_CREATE_VERSION),
        VersionCmd::Update { op_id, .. } => (op_id, perm::OP_UPDATE_VERSION),
    };
    let project = cmd_project(&mut *conn, cmd).await?.ok_or_else(not_found)?;
    let found: Option<i32> = sqlx::query_scalar(if locks {
        "SELECT 1 FROM projects WHERE id = $1 FOR SHARE"
    } else {
        "SELECT 1 FROM projects WHERE id = $1"
    })
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?;
    found.ok_or_else(not_found)?;
    let row = match cmd {
        VersionCmd::Update { version_id, .. } => Some(
            version_row(&mut *conn, version_id, locks)
                .await?
                .ok_or_else(not_found)?,
        ),
        VersionCmd::Create { .. } => None,
    };
    let token = if locks {
        let t = commands::search_token(&mut *conn).await?;
        commands::check_deadlines(&mut *conn, &ectx.deadlines).await?;
        Some(t)
    } else {
        None
    };

    // Visibility: invisible, archived and deleted look like missing.
    let scope = db::load_scope(&mut *conn, ectx.caller).await?;
    if !scope.project_ids().any(|p| p == project) {
        return Err(not_found());
    }
    let mut actor = perm::actor(&mut *conn, ectx.caller).await?;
    if let Some(c) = &ectx.ceiling {
        actor.delegations = Some(c.clone());
    }
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(not_found)?;
    if pp.status == "archived"
        || !pp.allowed_any(VIEW_ISSUES)
        || row.as_ref().is_some_and(|v| v.deleted)
    {
        return Err(not_found());
    }
    if !pp.writable() {
        return Err(op_forbidden(
            op_id,
            "project_not_writable",
            "the project does not accept changes",
        ));
    }
    if !pp.allowed_any(MANAGE_VERSIONS) || !actor.delegation_allows(project, kind, None) {
        return Err(op_forbidden(
            op_id,
            "permission",
            "manage_versions and a delegation that allows this command are needed",
        ));
    }

    match (cmd, row) {
        (VersionCmd::Create { fields, .. }, _) => {
            let (name, key) =
                name_and_key(&mut *conn, fields.name.as_deref().unwrap_or_default()).await?;
            let date = match &fields.effective_date {
                Some(Some(d)) => Some(parse_date(d)?),
                _ => None,
            };
            let description = fields.description.clone().unwrap_or_default();
            check_description(&description)?;
            if let Some(other) = taken(&mut *conn, project, &key, None).await? {
                return Err(name_taken(other));
            }
            let status = "open".to_string();
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
            Ok((
                Plan {
                    op_id,
                    id: Uuid::now_v7(),
                    project_id: project,
                    created: true,
                    name,
                    key: Some(key),
                    description,
                    date,
                    status,
                    changes,
                    seq: 1,
                    lock_version: 0,
                },
                token,
            ))
        }
        (
            VersionCmd::Update {
                lock_version, set, ..
            },
            Some(v),
        ) => {
            if v.lock_version != lock_version {
                return Err(
                    ApiError::new(ErrorCode::Conflict, "the version changed meanwhile")
                        .with_details(
                            json!({"version_id": v.id, "expected_lock_version": lock_version,
                                         "lock_version": v.lock_version}),
                        )
                        .into(),
                );
            }
            let s: &VersionFields = set;
            let mut changes = Map::new();
            let (mut name, mut key) = (v.name.clone(), None);
            if let Some(n) = &s.name {
                let (nn, nk) = name_and_key(&mut *conn, n).await?;
                if nn != v.name {
                    if let Some(other) = taken(&mut *conn, v.project_id, &nk, Some(v.id)).await? {
                        return Err(name_taken(other));
                    }
                    changes.insert("name".into(), json!({"old": v.name, "new": nn}));
                    (name, key) = (nn, Some(nk));
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
            let changed = !changes.is_empty();
            Ok((
                Plan {
                    op_id,
                    id: v.id,
                    project_id: v.project_id,
                    created: false,
                    name,
                    key,
                    description,
                    date,
                    status,
                    changes,
                    seq: v.last_event_seq + 1,
                    lock_version: v.lock_version + i32::from(changed),
                },
                token,
            ))
        }
        (VersionCmd::Update { .. }, None) => unreachable!("an update has its row"),
    }
}

/// Writes a planned version command with `at`'s attribution: the row
/// (`created_by` = the agent, `created_on_behalf_of_id` = the delegator),
/// the event, the field origins and — for agent writes — the before/after
/// copies for undo (§17.9a).
pub(crate) async fn write_plan(conn: &mut PgConnection, p: &Plan, at: &Attribution) -> Result<()> {
    if p.changes.is_empty() {
        return Ok(());
    }
    if p.created {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO versions (id, project_id, name, name_key, description, effective_date, status,
                                   created_by, created_on_behalf_of_id, last_event_seq)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 1)
             ON CONFLICT (project_id, name_key) WHERE deleted_at IS NULL DO NOTHING RETURNING id",
        )
        .bind(p.id)
        .bind(p.project_id)
        .bind(&p.name)
        .bind(p.key.as_deref())
        .bind(&p.description)
        .bind(p.date)
        .bind(&p.status)
        .bind(at.author_id)
        .bind(at.author_on_behalf)
        .fetch_optional(&mut *conn)
        .await?;
        if inserted.is_none() {
            let other = taken(
                &mut *conn,
                p.project_id,
                p.key.as_deref().unwrap_or_default(),
                None,
            )
            .await?
            .unwrap_or_default();
            return Err(name_taken(other));
        }
    } else {
        sqlx::query(
            "UPDATE versions SET name = $2, name_key = COALESCE($3, name_key), description = $4,
                    effective_date = $5, status = $6, lock_version = lock_version + 1,
                    last_event_seq = $7, updated_at = now()
             WHERE id = $1",
        )
        .bind(p.id)
        .bind(&p.name)
        .bind(p.key.as_deref())
        .bind(&p.description)
        .bind(p.date)
        .bind(&p.status)
        .bind(p.seq)
        .execute(&mut *conn)
        .await
        .map_err(|e| unique_to_taken(e, p.id))?;
    }
    write_event(
        &mut *conn,
        p.id,
        p.seq,
        if p.created { "created" } else { "updated" },
        at,
        &p.changes,
    )
    .await?;
    if at.record_changes {
        let rows: Vec<(&str, Json, Json)> = if p.created {
            vec![(
                "_created",
                Json::Null,
                json!({"name": p.name, "description": p.description,
                   "effective_date": p.date.map(|d| d.to_string()), "status": p.status}),
            )]
        } else {
            p.changes
                .iter()
                .map(|(k, c)| (k.as_str(), c["old"].clone(), c["new"].clone()))
                .collect()
        };
        for (field, before, after) in rows {
            sqlx::query(
                "INSERT INTO execution_version_changes (execution_id, version_id, field_key, before_value, after_value)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(at.execution_ref)
            .bind(p.id)
            .bind(field)
            .bind(before)
            .bind(after)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// MCP `version_list` (§17.9): `name` keeps only the version whose
/// `name_key` matches (none → an empty list, the project being visible).
pub async fn list_for_tool(
    app: &App,
    caller: AuthContext,
    project: &str,
    status: Option<&[String]>,
    name: Option<&str>,
    next: bool,
) -> Result<VersionList> {
    let status = status.map(|s| s.join(","));
    let mut out = list(
        app,
        caller,
        project,
        status.as_deref(),
        next.then_some("next"),
    )
    .await?;
    if let Some(n) = name {
        let names: Vec<String> = out.versions.iter().map(|v| v.name.clone()).collect();
        let (want, keys): (String, Vec<String>) = {
            let mut conn = app.pool.acquire().await?;
            let want: String = sqlx::query_scalar("SELECT casefold(normalize(btrim($1), NFC))")
                .bind(n)
                .fetch_one(&mut *conn)
                .await?;
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT casefold(normalize(x, NFC)) FROM unnest($1::text[]) WITH ORDINALITY AS t(x, i) ORDER BY i",
            )
            .bind(&names)
            .fetch_all(&mut *conn)
            .await?;
            (want, keys)
        };
        let mut i = 0;
        out.versions.retain(|_| {
            let keep = keys[i] == want;
            i += 1;
            keep
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- undo (§17.13, S6b.5)

/// A version's project, lock_version, deleted flag and field values by
/// key (the encoding of `execution_version_changes`).
pub(crate) async fn undo_values(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<
    Option<(
        Uuid,
        i32,
        bool,
        String,
        std::collections::BTreeMap<String, Json>,
    )>,
> {
    let Some(v) = version_row(&mut *conn, id, false).await? else {
        return Ok(None);
    };
    let m = std::collections::BTreeMap::from([
        ("name".to_string(), json!(v.name)),
        ("description".to_string(), json!(v.description)),
        (
            "effective_date".to_string(),
            json!(v.effective_date.map(|d| d.to_string())),
        ),
        ("status".to_string(), json!(v.status)),
    ]);
    Ok(Some((v.project_id, v.lock_version, v.deleted, v.name, m)))
}

/// Keys of the issues `caller` can see in the version (B7 `in_use`).
pub(crate) async fn visible_uses(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    id: Uuid,
) -> Result<Vec<String>> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    Ok(visible_bits(&mut *conn, &scope, project, &[id])
        .await?
        .into_iter()
        .map(|b| b.key)
        .collect())
}

/// The undo of `create_version`: a soft delete with its event (the version
/// row is already locked by the caller).
pub(crate) async fn undo_soft_delete(
    conn: &mut PgConnection,
    id: Uuid,
    at: &Attribution,
) -> Result<()> {
    let v = version_row(&mut *conn, id, true)
        .await?
        .ok_or_else(not_found)?;
    let seq = v.last_event_seq + 1;
    sqlx::query(
        "UPDATE versions SET deleted_at = now(), lock_version = lock_version + 1,
                last_event_seq = $2, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(seq)
    .execute(&mut *conn)
    .await?;
    write_event(&mut *conn, id, seq, "deleted", at, &Map::new()).await
}

// ---------------------------------------------------------------- timeline (§17.8, S6b.5)

/// Issues per lane and page (§17.8 L2858).
pub const LANE_LIMIT: usize = 200;
/// The longest range, in days (`to - from` at most 399).
pub const MAX_RANGE_DAYS: i64 = 400;

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelineRange {
    pub from: String,
    pub to: String,
    /// The default end was cut to 400 days; this version was the reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_by_version: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelineVersion {
    pub id: Uuid,
    pub name: String,
    pub effective_date: Option<String>,
    pub status: String,
    pub health: String,
    pub health_reasons: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelineStatus {
    pub id: Uuid,
    pub name: String,
    pub is_closed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelinePerson {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelineItem {
    pub key: String,
    pub subject: String,
    pub status: TimelineStatus,
    pub assigned_to: Option<TimelinePerson>,
    pub start_date: Option<String>,
    pub due_date: Option<String>,
    /// `overdue`, `late_for_version`, `in_closed_version`.
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct TimelineLane {
    /// `null`: the lane of issues without a version.
    pub version: Option<TimelineVersion>,
    pub items: Vec<TimelineItem>,
    /// Visible issues of the lane with neither date (not drawn).
    pub undated_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct Timeline {
    pub today: String,
    pub range: TimelineRange,
    pub lanes: Vec<TimelineLane>,
}

fn bad_arg(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn date_arg(path: &str, v: Option<&str>) -> Result<Option<NaiveDate>> {
    v.map(|s| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| bad_arg(path, format!("{path} must be YYYY-MM-DD")))
    })
    .transpose()
}

/// §17.8 L2849–L2856: the default range around today and the latest open
/// version; one given end keeps it, the other is computed.
fn timeline_range(
    today: NaiveDate,
    open_dates: &[(Uuid, NaiveDate)],
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<(NaiveDate, NaiveDate, Option<Uuid>)> {
    let max = chrono::Duration::days(MAX_RANGE_DAYS - 1);
    let latest = open_dates
        .iter()
        .filter(|(_, d)| *d >= today)
        .max_by_key(|(id, d)| (*d, *id))
        .copied();
    let default_to = |from: NaiveDate| {
        let to = latest
            .map(|(_, d)| d + chrono::Duration::days(14))
            .unwrap_or(today + chrono::Duration::days(90));
        let to = to.max(from);
        if to > from + max {
            (from + max, latest.map(|l| l.0))
        } else {
            (to, None)
        }
    };
    let (from, to, cut) = match (from, to) {
        (Some(f), Some(t)) => (f, t, None),
        (Some(f), None) => {
            let (t, cut) = default_to(f);
            (f, t, cut)
        }
        (None, Some(t)) => {
            let f = (today - chrono::Duration::days(14)).min(t);
            (f.max(t - max), t, None)
        }
        (None, None) => {
            let f = today - chrono::Duration::days(14);
            let (t, cut) = default_to(f);
            (f, t, cut)
        }
    };
    if from > to {
        return Err(bad_arg("from", "from must not be after to"));
    }
    if to - from > max {
        return Err(bad_arg(
            "to",
            format!("the range is at most {MAX_RANGE_DAYS} days"),
        ));
    }
    Ok((from, to, cut))
}

fn lane_cursor(lane: &str, offset: usize) -> String {
    crate::auth::encode(
        json!({"lane": lane, "offset": offset})
            .to_string()
            .as_bytes(),
    )
}

fn open_lane_cursor(raw: &str) -> Result<(String, usize)> {
    let bad = || api(ErrorCode::InvalidCursor, "not a timeline cursor");
    let bytes = crate::auth::decode_bytes(raw).ok_or_else(bad)?;
    let v: Json = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    Ok((
        v["lane"].as_str().ok_or_else(bad)?.to_string(),
        v["offset"].as_u64().ok_or_else(bad)? as usize,
    ))
}

/// `GET /projects/{id}/timeline?from=&to=&versions=&include_closed=&cursor=`
/// (§17.8): read from the database within `load_scope`; an issue the
/// caller cannot see is not counted anywhere and leaves no gap (T-V9).
#[allow(clippy::too_many_arguments)]
pub async fn timeline(
    app: &App,
    caller: AuthContext,
    project: &str,
    from: Option<&str>,
    to: Option<&str>,
    versions: Option<&str>,
    include_closed: bool,
    cursor: Option<&str>,
) -> Result<Timeline> {
    let from = date_arg("from", from)?;
    let to = date_arg("to", to)?;
    // `versions`: ids, or `none` = only the lane without a version (B24).
    let wanted: Option<Vec<String>> = versions.map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let page = cursor.map(open_lane_cursor).transpose()?;
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let today = today(&mut tx).await?;
    let all: Vec<VRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VCOLS} FROM versions WHERE project_id = $1
         ORDER BY effective_date NULLS LAST, name_key"
    )))
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    let open_dates: Vec<(Uuid, NaiveDate)> = all
        .iter()
        .filter(|v| v.status == "open" && !v.deleted)
        .filter_map(|v| v.effective_date.map(|d| (v.id, d)))
        .collect();
    let (from, to, cut) = timeline_range(today, &open_dates, from, to)?;

    // Every visible issue of the project (dates decide where it is drawn).
    type Row = (
        Uuid,
        String,
        String,
        Uuid,
        bool,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        Uuid,
        String,
        bool,
        Option<NaiveDate>,
        Option<NaiveDate>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id, k.key, i.subject, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                i.fixed_version_id, s.id, s.name, s.category = 'done', i.start_date, i.due_date,
                ap.name, ap.kind
         FROM issues i JOIN issue_statuses s ON s.id = i.status_id
         JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         LEFT JOIN principals ap ON ap.id = i.assigned_to_id
         WHERE i.project_id = $1 AND i.archived_at IS NULL",
    )
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| {
            a.scope.can_see(&IssueFacts {
                project_id: project,
                tracker_id: r.3,
                is_private: r.4,
                author_id: r.5,
                assigned_to_id: r.6,
            })
        })
        .collect();
    let bits_all = visible_bits(
        &mut tx,
        &a.scope,
        project,
        &all.iter().map(|v| v.id).collect::<Vec<_>>(),
    )
    .await?;
    let t = thresholds(&mut tx, project).await?;

    // Lanes: versions by date (deleted only with visible issues), then none.
    let mut lanes_def: Vec<Option<&VRow>> = Vec::new();
    for v in &all {
        let has = rows.iter().any(|r| r.7 == Some(v.id));
        if v.deleted && !has {
            continue;
        }
        if !include_closed && v.status == "closed" {
            continue;
        }
        if let Some(w) = &wanted
            && !w.iter().any(|x| *x == v.id.to_string())
        {
            continue;
        }
        lanes_def.push(Some(v));
    }
    if wanted
        .as_ref()
        .is_none_or(|w| w.iter().any(|x| x == "none"))
    {
        lanes_def.push(None);
    }
    if let Some(w) = &wanted {
        for x in w {
            if x != "none" && !all.iter().any(|v| v.id.to_string() == *x) {
                return Err(bad_arg(
                    "versions",
                    "versions takes version ids of this project or none",
                ));
            }
        }
    }

    let mut lanes = Vec::new();
    for lane in lanes_def {
        let lane_id = lane
            .map(|v| v.id.to_string())
            .unwrap_or_else(|| "none".into());
        let offset = match &page {
            Some((l, o)) if *l == lane_id => *o,
            Some(_) => continue, // a cursor pages one lane
            None => 0,
        };
        let mine: Vec<&Row> = rows
            .iter()
            .filter(|r| r.7 == lane.map(|v| v.id))
            .filter(|r| include_closed || !r.10)
            .collect();
        let undated_count = mine
            .iter()
            .filter(|r| r.11.is_none() && r.12.is_none())
            .count() as i64;
        let mut drawn: Vec<&&Row> = mine
            .iter()
            .filter(|r| r.11.is_some() || r.12.is_some())
            .filter(|r| {
                let s = r.11.or(r.12).unwrap();
                let e = r.12.or(r.11).unwrap();
                s <= to && e >= from
            })
            .collect();
        drawn.sort_by(|x, y| (x.11.or(x.12), &x.1).cmp(&(y.11.or(y.12), &y.1)));
        let total = drawn.len();
        let items: Vec<TimelineItem> = drawn
            .into_iter()
            .skip(offset)
            .take(LANE_LIMIT)
            .map(|r| {
                let mut flags = Vec::new();
                if !r.10 && r.12.is_some_and(|d| d < today) {
                    flags.push("overdue".to_string());
                }
                if let (Some(due), Some(v)) = (r.12, lane.and_then(|v| v.effective_date))
                    && due > v
                {
                    flags.push("late_for_version".into());
                }
                if lane.is_some_and(|v| v.status == "closed") {
                    flags.push("in_closed_version".into());
                }
                TimelineItem {
                    key: r.1.clone(),
                    subject: r.2.clone(),
                    status: TimelineStatus {
                        id: r.8,
                        name: r.9.clone(),
                        is_closed: r.10,
                    },
                    assigned_to: match (r.6, &r.13, &r.14) {
                        (Some(id), Some(n), Some(k)) => Some(TimelinePerson {
                            id,
                            name: n.clone(),
                            kind: k.clone(),
                        }),
                        _ => None,
                    },
                    start_date: r.11.map(|d| d.to_string()),
                    due_date: r.12.map(|d| d.to_string()),
                    flags,
                }
            })
            .collect();
        let next_cursor =
            (offset + LANE_LIMIT < total).then(|| lane_cursor(&lane_id, offset + LANE_LIMIT));
        let version = lane.map(|v| {
            let vb: Vec<&Bit> = bits_all.iter().filter(|b| b.version_id == v.id).collect();
            let (h, reasons) = health(v, &vb, today, &t);
            TimelineVersion {
                id: v.id,
                name: v.name.clone(),
                effective_date: v.effective_date.map(|d| d.to_string()),
                status: v.status.clone(),
                health: h,
                health_reasons: reasons,
                deleted: v.deleted,
            }
        });
        lanes.push(TimelineLane {
            version,
            items,
            undated_count,
            next_cursor,
        });
    }
    tx.commit().await?;
    Ok(Timeline {
        today: today.to_string(),
        range: TimelineRange {
            from: from.to_string(),
            to: to.to_string(),
            truncated_by_version: cut,
        },
        lanes,
    })
}

// ---------------------------------------------------------------- schedule settings (§17.12)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct HealthValues {
    pub health_at_risk_days: Option<i32>,
    pub health_at_risk_open_pct: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ProjectHealthSettings {
    pub inherit_org: bool,
    pub health_at_risk_days: Option<i32>,
    pub health_at_risk_open_pct: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ScheduleSettings {
    /// The caller may change these settings (`manage_versions`).
    pub can_manage: bool,
    pub effective: HealthValues,
    /// `org` or `project`.
    pub source: String,
    pub org: HealthValues,
    pub project: ProjectHealthSettings,
    pub lock_version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct OrgScheduleSettings {
    pub health_at_risk_days: Option<i32>,
    pub health_at_risk_open_pct: Option<i32>,
    pub lock_version: i32,
}

/// `PUT /projects/{id}/schedule-settings`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSettingsInput {
    pub lock_version: i32,
    pub inherit_org: bool,
    #[serde(default, deserialize_with = "present")]
    #[schemars(with = "Option<i32>")]
    pub health_at_risk_days: Option<Option<i32>>,
    #[serde(default, deserialize_with = "present")]
    #[schemars(with = "Option<i32>")]
    pub health_at_risk_open_pct: Option<Option<i32>>,
}

/// `PUT /org/schedule-settings` (a system administrator).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgScheduleSettingsInput {
    pub lock_version: i32,
    pub health_at_risk_days: Option<i32>,
    pub health_at_risk_open_pct: Option<i32>,
}

pub const ACTION_SCHEDULE_SETTINGS: &str = "schedule_settings_update";
pub const ACTION_ORG_SCHEDULE_SETTINGS: &str = "org_schedule_settings_update";

fn check_ranges(days: Option<i32>, pct: Option<i32>) -> Result<()> {
    if days.is_some_and(|d| !(0..=90).contains(&d)) {
        return Err(invalid_value(
            "health_at_risk_days",
            "0 to 90 days",
            json!({}),
        ));
    }
    if pct.is_some_and(|p| !(0..=100).contains(&p)) {
        return Err(invalid_value(
            "health_at_risk_open_pct",
            "0 to 100 percent",
            json!({}),
        ));
    }
    Ok(())
}

async fn settings_in(
    conn: &mut PgConnection,
    project: Uuid,
    can_manage: bool,
) -> Result<ScheduleSettings> {
    type Row = (
        Option<i32>,
        Option<i32>,
        bool,
        Option<i32>,
        Option<i32>,
        i32,
    );
    let (od, op, inherit, pd, pp, lv): Row = sqlx::query_as(
        "SELECT o.health_at_risk_days, o.health_at_risk_open_pct,
                ps.inherit_org, ps.health_at_risk_days, ps.health_at_risk_open_pct, ps.lock_version
         FROM org_settings o JOIN project_schedule_settings ps ON ps.project_id = $1",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    let t = thresholds(&mut *conn, project).await?;
    Ok(ScheduleSettings {
        can_manage,
        effective: HealthValues {
            health_at_risk_days: t.at_risk_days,
            health_at_risk_open_pct: t.at_risk_open_pct,
        },
        source: t.source,
        org: HealthValues {
            health_at_risk_days: od,
            health_at_risk_open_pct: op,
        },
        project: ProjectHealthSettings {
            inherit_org: inherit,
            health_at_risk_days: pd,
            health_at_risk_open_pct: pp,
        },
        lock_version: lv,
    })
}

/// `GET /projects/{id}/schedule-settings`.
pub async fn schedule_settings(
    app: &App,
    caller: AuthContext,
    project: &str,
) -> Result<ScheduleSettings> {
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let s = settings_in(&mut tx, project, a.manage).await?;
    tx.commit().await?;
    Ok(s)
}

/// `PUT /projects/{id}/schedule-settings` (Idempotency-Key): `manage_versions`;
/// `lock_version` must match (`conflict`); with `inherit_org = true` the two
/// values must be left out; `null` turns a rule off. Only people.
pub async fn put_schedule_settings(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &ScheduleSettingsInput,
) -> Result<ScheduleSettings> {
    let who = commands::executor(&caller)?;
    if input.inherit_org
        && (input.health_at_risk_days.is_some() || input.health_at_risk_open_pct.is_some())
    {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "with inherit_org true, leave health_at_risk_days and health_at_risk_open_pct out",
        )
        .with_details(json!({"reason": "inherit_with_values"}))
        .into());
    }
    if !input.inherit_org
        && (input.health_at_risk_days.is_none() || input.health_at_risk_open_pct.is_none())
    {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "with inherit_org false, send both values (null turns a rule off)",
        )
        .with_details(json!({"reason": "values_missing"}))
        .into());
    }
    let days = input.health_at_risk_days.flatten();
    let pct = input.health_at_risk_open_pct.flatten();
    check_ranges(days, pct)?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let h = hash(
        &json!({"op": "schedule_settings", "project": project, "lock_version": input.lock_version,
                         "inherit_org": input.inherit_org, "days": days, "pct": pct}),
    );
    let fp = commands::fingerprint(ACTION_SCHEDULE_SETTINGS, &h);
    let Some(request_id) = commands::open_request(
        &mut tx,
        who.user_id,
        operation_id,
        ACTION_SCHEDULE_SETTINGS,
        &fp,
    )
    .await?
    else {
        commands::stored_result(
            &mut tx,
            who.user_id,
            operation_id,
            ACTION_SCHEDULE_SETTINGS,
            &fp,
        )
        .await?;
        let s = settings_in(&mut tx, project, true).await?;
        tx.commit().await?;
        return Ok(s);
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access(&mut tx, caller, project).await?;
    if !a.manage || !a.writable {
        return Err(forbidden(
            "manage_versions",
            "changing the health thresholds needs manage_versions",
        ));
    }
    let lv: i32 = sqlx::query_scalar(
        "SELECT lock_version FROM project_schedule_settings WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await?;
    if lv != input.lock_version {
        return Err(
            ApiError::new(ErrorCode::Conflict, "the settings changed; reload them")
                .with_details(json!({"current_lock_version": lv}))
                .into(),
        );
    }
    sqlx::query(
        "UPDATE project_schedule_settings
         SET inherit_org = $2, health_at_risk_days = $3, health_at_risk_open_pct = $4,
             lock_version = lock_version + 1, updated_by = $5, updated_at = now()
         WHERE project_id = $1",
    )
    .bind(project)
    .bind(input.inherit_org)
    .bind(if input.inherit_org { None } else { days })
    .bind(if input.inherit_org { None } else { pct })
    .bind(who.user_id)
    .execute(&mut *tx)
    .await?;
    commands::complete_request(&mut tx, request_id, &json!({"project_id": project})).await?;
    let s = settings_in(&mut tx, project, true).await?;
    tx.commit().await?;
    Ok(s)
}

async fn org_settings_in(conn: &mut PgConnection) -> Result<OrgScheduleSettings> {
    let (d, p, lv): (Option<i32>, Option<i32>, i32) = sqlx::query_as(
        "SELECT health_at_risk_days, health_at_risk_open_pct, schedule_lock_version FROM org_settings",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(OrgScheduleSettings {
        health_at_risk_days: d,
        health_at_risk_open_pct: p,
        lock_version: lv,
    })
}

/// `GET /org/schedule-settings` (anyone signed in).
pub async fn org_schedule_settings(app: &App, caller: AuthContext) -> Result<OrgScheduleSettings> {
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let s = org_settings_in(&mut tx).await?;
    tx.commit().await?;
    Ok(s)
}

/// `PUT /org/schedule-settings` (a system administrator, Idempotency-Key).
pub async fn put_org_schedule_settings(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &OrgScheduleSettingsInput,
) -> Result<OrgScheduleSettings> {
    let who = commands::executor(&caller)?;
    check_ranges(input.health_at_risk_days, input.health_at_risk_open_pct)?;
    let mut tx = app.pool.begin().await?;
    let me = db::effective(&mut tx, caller).await?;
    if !me.is_admin || me.agent.is_some() {
        return Err(forbidden(
            "admin",
            "only a system administrator changes this",
        ));
    }
    let h = hash(
        &json!({"op": "org_schedule_settings", "lock_version": input.lock_version,
                         "days": input.health_at_risk_days, "pct": input.health_at_risk_open_pct}),
    );
    let fp = commands::fingerprint(ACTION_ORG_SCHEDULE_SETTINGS, &h);
    let Some(request_id) = commands::open_request(
        &mut tx,
        who.user_id,
        operation_id,
        ACTION_ORG_SCHEDULE_SETTINGS,
        &fp,
    )
    .await?
    else {
        commands::stored_result(
            &mut tx,
            who.user_id,
            operation_id,
            ACTION_ORG_SCHEDULE_SETTINGS,
            &fp,
        )
        .await?;
        let s = org_settings_in(&mut tx).await?;
        tx.commit().await?;
        return Ok(s);
    };
    commands::lock_executor(&mut tx, &who).await?;
    let lv: i32 = sqlx::query_scalar("SELECT schedule_lock_version FROM org_settings FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    if lv != input.lock_version {
        return Err(
            ApiError::new(ErrorCode::Conflict, "the settings changed; reload them")
                .with_details(json!({"current_lock_version": lv}))
                .into(),
        );
    }
    sqlx::query(
        "UPDATE org_settings SET health_at_risk_days = $1, health_at_risk_open_pct = $2,
                schedule_lock_version = schedule_lock_version + 1",
    )
    .bind(input.health_at_risk_days)
    .bind(input.health_at_risk_open_pct)
    .execute(&mut *tx)
    .await?;
    commands::complete_request(&mut tx, request_id, &json!({"org": true})).await?;
    let s = org_settings_in(&mut tx).await?;
    tx.commit().await?;
    Ok(s)
}

#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// T-V9b: every default is a legal range.
    #[test]
    fn default_ranges() {
        let today = d("2026-11-08");
        let v = Uuid::now_v7();
        // No version.
        let (f, t, c) = timeline_range(today, &[], None, None).unwrap();
        assert_eq!((f, t, c), (d("2026-10-25"), d("2027-02-06"), None));
        // Open versions only in the past.
        let (_, t, _) = timeline_range(today, &[(v, d("2026-01-01"))], None, None).unwrap();
        assert_eq!(t, d("2027-02-06"));
        // A version in 2199: cut to 400 days, with the reason.
        let (f, t, c) = timeline_range(today, &[(v, d("2199-01-01"))], None, None).unwrap();
        assert_eq!((t - f).num_days(), 399);
        assert_eq!(c, Some(v));
        // A near version: +14 days.
        let (_, t, _) = timeline_range(today, &[(v, d("2026-12-01"))], None, None).unwrap();
        assert_eq!(t, d("2026-12-15"));
        // One end given.
        let (f, t, _) = timeline_range(today, &[], Some(d("2027-06-01")), None).unwrap();
        assert!(f <= t && (t - f).num_days() <= 399);
        let (f, t, _) = timeline_range(today, &[], None, Some(d("2020-01-01"))).unwrap();
        assert!(f <= t && (t - f).num_days() <= 399);
        // Errors.
        assert!(timeline_range(today, &[], Some(d("2027-01-02")), Some(d("2027-01-01"))).is_err());
        assert!(timeline_range(today, &[], Some(d("2026-01-01")), Some(d("2027-02-05"))).is_err());
        assert!(timeline_range(today, &[], Some(d("2026-01-01")), Some(d("2027-02-04"))).is_ok());
    }
}
