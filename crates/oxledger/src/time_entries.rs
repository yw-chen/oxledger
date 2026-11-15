// SPDX-License-Identifier: GPL-2.0-or-later
//! Time tracking (contract 05 §3.5, §3.10.2; Redmine `TimeEntry`).
//!
//! Writes go through `POST /commands` (`log_time`, `update_time_entry`,
//! `delete_time_entry`): [`plan`] runs inside the command engine's
//! evaluation (locks, visibility, permissions, T1–T7) and [`write_plan`]
//! writes what it planned. Lock order (§3.5.5): projects → issues →
//! versions → time_entries (id order, FOR UPDATE) → time_daily_totals
//! ((user_id, spent_on) order, FOR UPDATE, created first when missing) →
//! the receipt token and deadlines.
//!
//! Hours are kept in hundredths (`i64`) in Rust and bound as `numeric`
//! text in SQL; responses carry decimal strings (`"1.50"`, `"0"`).

use crate::auth::AuthContext;
use crate::commands::{
    self, Attribution, EvalCtx, ItemResult, TimeEntrySet, TimeFields, issue_not_found,
};

use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{
    self, EDIT_OWN_TIME_ENTRIES, EDIT_TIME_ENTRIES, LOG_TIME, LOG_TIME_FOR_OTHER_USERS,
    ProjectPerms,
};

use anyhow::Result;
use chrono::NaiveDate;
use ox_core::IssueFacts;
use ox_core::visibility::Scope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const TIME_MODULE: &str = "time_tracking";
/// T1: at most 1000 hours, two decimals.
const MAX_CENTS: i64 = 100_000;
/// T7.
pub const MAX_COMMENTS: usize = 1024;
/// The comments of a redacted entry (D12).
pub const REDACTED: &str = "[已遮蔽]";

// ---------------------------------------------------------------- hours

/// A decimal string with at most two decimals, 0–1000 → hundredths.
pub fn parse_hours(v: &Json) -> Option<i64> {
    let s = v.as_str()?.trim();
    let (int, frac) = match s.split_once('.') {
        Some((a, b)) => {
            if b.is_empty() {
                return None;
            }
            (a, b)
        }
        None => (s, ""),
    };
    if int.is_empty()
        || int.len() > 4
        || frac.len() > 2
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut cents = int.parse::<i64>().ok()? * 100;
    match frac.len() {
        1 => cents += frac.parse::<i64>().ok()? * 10,
        2 => cents += frac.parse::<i64>().ok()?,
        _ => {}
    }
    (cents <= MAX_CENTS).then_some(cents)
}

/// Hundredths → `"1.50"`; zero is `"0"` (contract 05 §3.5.2 examples).
pub fn fmt_hours(cents: i64) -> String {
    if cents == 0 {
        "0".into()
    } else {
        format!("{}.{:02}", cents / 100, cents % 100)
    }
}

fn sql_hours(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

// ---------------------------------------------------------------- errors

fn entry_not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no time entry")
}

fn project_not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no project")
}

fn forbidden_at(index: usize, op_id: Uuid, reason: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason, "op_index": index, "op_id": op_id}))
        .into()
}

pub(crate) fn forbidden(reason: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

fn invalid_at(index: usize, op_id: Uuid, field: &str, kind: &str, msg: &str) -> anyhow::Error {
    invalid_with(index, op_id, field, kind, msg, json!({}))
}

fn invalid_with(
    index: usize,
    op_id: Uuid,
    field: &str,
    kind: &str,
    msg: &str,
    extra: Json,
) -> anyhow::Error {
    let mut d = json!({"op_index": index, "op_id": op_id, "field": field, "kind": kind});
    if let (Json::Object(m), Json::Object(x)) = (&mut d, extra) {
        m.extend(x);
    }
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(d)
        .into()
}

fn required_at(index: usize, op_id: Uuid, field: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::RequiredFieldMissing,
        format!("{field} is required"),
    )
    .with_details(json!({"op_index": index, "op_id": op_id, "fields": [field]}))
    .into()
}

fn bad(path: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidRequest, msg)
        .with_details(json!({"path": path}))
        .into()
}

// ---------------------------------------------------------------- the commands

/// One time op of a parsed command document.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TimeCmd<'a> {
    Log {
        index: usize,
        op_id: Uuid,
        project_id: Option<Uuid>,
        issue: Option<Uuid>,
        fields: &'a TimeFields,
    },
    Update {
        index: usize,
        op_id: Uuid,
        entry_id: Uuid,
        lock_version: i32,
        set: &'a TimeEntrySet,
        /// `set.issue` resolved: `Some(None)` clears it.
        issue: Option<Option<Uuid>>,
    },
    Delete {
        index: usize,
        op_id: Uuid,
        entry_id: Uuid,
        lock_version: i32,
    },
}

impl TimeCmd<'_> {
    fn index(&self) -> usize {
        match self {
            Self::Log { index, .. } | Self::Update { index, .. } | Self::Delete { index, .. } => {
                *index
            }
        }
    }
    fn op_id(&self) -> Uuid {
        match self {
            Self::Log { op_id, .. } | Self::Update { op_id, .. } | Self::Delete { op_id, .. } => {
                *op_id
            }
        }
    }
}

/// The issue of a mixed document (`log_time` with issue ops, §3.10.2),
/// already locked and checked by the engine: T5 compares its status at the
/// start of the transaction and after the document's ops.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IssueCtx {
    pub id: Uuid,
    pub closed_before: bool,
    pub closed_after: bool,
}

/// A time entry's stored row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct EntryRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub issue_id: Option<Uuid>,
    pub user_id: Uuid,
    pub activity_id: Uuid,
    pub spent_on: NaiveDate,
    pub cents: i64,
    pub comments: String,
    pub lock_version: i32,
    pub last_event_seq: i64,
    pub deleted: bool,
}

pub(crate) const ENTRY_COLS: &str = "id, project_id, issue_id, user_id,
    activity_id, spent_on, (hours * 100)::bigint AS cents, comments, lock_version, last_event_seq,
    deleted_at IS NOT NULL AS deleted";

pub(crate) async fn entry_row(
    conn: &mut PgConnection,
    id: Uuid,
    lock: bool,
) -> Result<Option<EntryRow>> {
    let sql = format!(
        "SELECT {ENTRY_COLS} FROM time_entries WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

/// What one op will write.
#[derive(Debug, Clone)]
pub(crate) struct EntryPlan {
    pub op_id: Uuid,
    /// `created`, `updated` or `deleted`.
    pub kind: &'static str,
    pub id: Uuid,
    pub project_id: Uuid,
    pub issue_id: Option<Uuid>,
    pub user_id: Uuid,
    pub activity_id: Uuid,
    pub spent_on: NaiveDate,
    pub cents: i64,
    pub comments: String,
    /// field → {"old", "new"} (the event payload and undo copies).
    pub changes: Map<String, Json>,
    pub seq: i64,
    /// After the write.
    pub lock_version: i32,
}

/// What a time document will write.
#[derive(Debug, Clone, Default)]
pub(crate) struct TimePlan {
    pub entries: Vec<EntryPlan>,
    /// (user, day) → change in hundredths.
    pub deltas: BTreeMap<(Uuid, NaiveDate), i64>,
}

/// `effects` of a time op (contract 05 §3.10.2).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TimeEntryEffect {
    /// Always `time_entry`.
    pub kind: String,
    pub id: Uuid,
    pub lock_version: i32,
    pub changed: bool,
    /// `created`, `updated` or `deleted`.
    pub action: String,
}

impl TimePlan {
    pub(crate) fn items(&self) -> Vec<ItemResult> {
        self.entries.iter().map(EntryPlan::item).collect()
    }
    pub(crate) fn effects(&self) -> Vec<TimeEntryEffect> {
        self.entries
            .iter()
            .map(|e| TimeEntryEffect {
                kind: "time_entry".into(),
                id: e.id,
                lock_version: e.lock_version,
                changed: !e.changes.is_empty(),
                action: e.kind.into(),
            })
            .collect()
    }
}

impl EntryPlan {
    fn item(&self) -> ItemResult {
        ItemResult {
            op_id: self.op_id,
            status: if self.changes.is_empty() {
                "no_effect"
            } else {
                "applied"
            }
            .into(),
            issue_id: None,
            version_id: None,
            time_entry_id: Some(self.id),
            wiki_page_id: None,
        }
    }
}

/// The timelog settings (`org_settings`, §3.5.3).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Rules {
    pub max_cents: i64,
    pub accept_0_hours: bool,
    pub accept_future_dates: bool,
    pub accept_closed_issues: bool,
    pub require_issue: bool,
    pub require_comments: bool,
    pub lock_version: i32,
}

pub(crate) async fn rules(conn: &mut PgConnection) -> Result<Rules> {
    Ok(sqlx::query_as(
        "SELECT (timelog_max_hours_per_day * 100)::bigint AS max_cents,
                timelog_accept_0_hours AS accept_0_hours,
                timelog_accept_future_dates AS accept_future_dates,
                timelog_accept_closed_issues AS accept_closed_issues,
                timelog_require_issue AS require_issue,
                timelog_require_comments AS require_comments,
                timelog_lock_version AS lock_version
         FROM org_settings",
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// `perm(x)` in the project, ignoring trackers (time permissions are not
/// tracker-scoped).
pub(crate) fn has(pp: &ProjectPerms, p: &str) -> bool {
    pp.status != "archived" && (pp.admin || pp.roles.iter().any(|r| r.perms.contains_key(p)))
}

pub(crate) async fn module_enabled(conn: &mut PgConnection, project: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM enabled_modules WHERE project_id = $1 AND module = $2)",
    )
    .bind(project)
    .bind(TIME_MODULE)
    .fetch_one(&mut *conn)
    .await?)
}

/// `time_visibility(project, viewer)`: `all`, `own` or none.
pub(crate) async fn visibility(
    conn: &mut PgConnection,
    project: Uuid,
    viewer: Uuid,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT time_visibility($1, $2)")
        .bind(project)
        .bind(viewer)
        .fetch_one(&mut *conn)
        .await?)
}

/// Whether `user` may be the person of a time entry in `project`: an active
/// user who is a member (directly or through a group) with a role that has
/// `log_time` (Redmine `TimeEntry#assignable_users`).
async fn assignable(conn: &mut PgConnection, project: Uuid, user: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM principals u
            JOIN members m ON m.project_id = $1
             AND (m.principal_id = u.id
                  OR m.principal_id IN (SELECT group_id FROM group_members WHERE user_id = u.id))
            JOIN member_roles mr ON mr.member_id = m.id
            JOIN role_permissions rp ON rp.role_id = mr.role_id AND rp.permission = 'log_time'
            WHERE u.id = $2 AND u.kind = 'user' AND u.status = 'active')",
    )
    .bind(project)
    .bind(user)
    .fetch_one(&mut *conn)
    .await?)
}

/// The activities a project may use now: active on the site and not turned
/// off in the project; and the default among them.
pub(crate) async fn available_activities(
    conn: &mut PgConnection,
    project: Uuid,
) -> Result<Vec<(Uuid, String, bool)>> {
    Ok(sqlx::query_as(
        "SELECT a.id, a.name, a.is_default FROM time_entry_activities a
         LEFT JOIN project_time_activities o ON o.activity_id = a.id AND o.project_id = $1
         WHERE a.active AND COALESCE(o.active, true)
         ORDER BY a.position, a.name_key",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?)
}

struct IssueBit {
    project_id: Uuid,
    facts: IssueFacts,
    closed: bool,
}

async fn issue_bits(
    conn: &mut PgConnection,
    ids: &[Uuid],
    lock: bool,
) -> Result<BTreeMap<Uuid, IssueBit>> {
    type BitRow = (Uuid, Uuid, Uuid, bool, Uuid, Option<Uuid>, bool);
    let rows: Vec<BitRow> = sqlx::query_as(if lock {
        "SELECT i.id, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                s.category = 'done'
         FROM issues i JOIN issue_statuses s ON s.id = i.status_id
         WHERE i.id = ANY($1) AND i.archived_at IS NULL ORDER BY i.id FOR SHARE OF i"
    } else {
        "SELECT i.id, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                s.category = 'done'
         FROM issues i JOIN issue_statuses s ON s.id = i.status_id
         WHERE i.id = ANY($1) AND i.archived_at IS NULL ORDER BY i.id"
    })
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, p, t, private, a, asg, closed)| {
            (
                id,
                IssueBit {
                    project_id: p,
                    facts: IssueFacts {
                        project_id: p,
                        tracker_id: t,
                        is_private: private,
                        author_id: a,
                        assigned_to_id: asg,
                    },
                    closed,
                },
            )
        })
        .collect())
}

/// Locks (or reads) the daily totals of `keys` in (user, day) order, creating
/// missing rows first when locking. Returns hundredths per key.
async fn lock_totals(
    conn: &mut PgConnection,
    keys: &[(Uuid, NaiveDate)],
    lock: bool,
) -> Result<BTreeMap<(Uuid, NaiveDate), i64>> {
    let users: Vec<Uuid> = keys.iter().map(|k| k.0).collect();
    let days: Vec<NaiveDate> = keys.iter().map(|k| k.1).collect();
    if lock {
        sqlx::query(
            "INSERT INTO time_daily_totals (user_id, spent_on)
             SELECT u, d FROM unnest($1::uuid[], $2::date[]) AS k(u, d) ORDER BY u, d
             ON CONFLICT DO NOTHING",
        )
        .bind(&users)
        .bind(&days)
        .execute(&mut *conn)
        .await?;
    }
    let mut out: BTreeMap<(Uuid, NaiveDate), i64> = keys.iter().map(|k| (*k, 0)).collect();
    if !lock {
        let rows: Vec<(Uuid, NaiveDate, i64)> = sqlx::query_as(
            "SELECT t.user_id, t.spent_on, (t.hours * 100)::bigint FROM time_daily_totals t
             JOIN unnest($1::uuid[], $2::date[]) AS k(u, d) ON t.user_id = k.u AND t.spent_on = k.d",
        )
        .bind(&users)
        .bind(&days)
        .fetch_all(&mut *conn)
        .await?;
        for (u, d, h) in rows {
            out.insert((u, d), h);
        }
        return Ok(out);
    }
    // One row at a time in (user, day) order, so the order is the code's and
    // a test can stop a document between two rows (T-M1-43).
    let mut sorted: Vec<(Uuid, NaiveDate)> = keys.to_vec();
    sorted.sort();
    sorted.dedup();
    for (n, (u, d)) in sorted.into_iter().enumerate() {
        let h: Option<i64> = sqlx::query_scalar(
            "SELECT (hours * 100)::bigint FROM time_daily_totals
             WHERE user_id = $1 AND spent_on = $2 FOR UPDATE",
        )
        .bind(u)
        .bind(d)
        .fetch_optional(&mut *conn)
        .await?;
        out.insert((u, d), h.unwrap_or(0));
        if n == 0 {
            fault("time.after_first_total").await?;
        }
    }
    Ok(out)
}

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

/// §3.5.5 for a mixed document (`log_time` next to issue ops): the engine
/// takes the field-option and search locks after the issues, so it calls
/// this first to lock every `(user, day)` the document's `log_time` ops may
/// add to, in the same order [`plan`] does. A value [`plan`] will reject
/// (an unparsable date) adds no key; [`plan`] locks the same rows again,
/// which is a no-op.
pub(crate) async fn prelock_totals(
    conn: &mut PgConnection,
    ectx: &EvalCtx,
    cmds: &[TimeCmd<'_>],
) -> Result<()> {
    if !ectx.mode.locks() {
        return Ok(());
    }
    let actor = perm::actor(&mut *conn, ectx.caller).await?;
    let Some(me) = actor.user_id else {
        return Ok(());
    };
    let today = crate::versions::today(&mut *conn).await?;
    let mut keys: BTreeSet<(Uuid, NaiveDate)> = BTreeSet::new();
    for c in cmds {
        if let TimeCmd::Log { fields, .. } = c {
            let day = match fields.spent_on.as_deref() {
                None => Some(today),
                Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").ok(),
            };
            if let Some(d) = day {
                keys.insert((fields.user_id.unwrap_or(me), d));
            }
        }
    }
    if !keys.is_empty() {
        let keys: Vec<(Uuid, NaiveDate)> = keys.into_iter().collect();
        lock_totals(&mut *conn, &keys, true).await?;
    }
    Ok(())
}

/// Whether `viewer` sees every live entry of `user` on `day` (T6: only then
/// may the error tell the remaining hours).
async fn sees_whole_day(
    conn: &mut PgConnection,
    viewer: Uuid,
    scope: &Scope,
    user: Uuid,
    day: NaiveDate,
) -> Result<bool> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT NOT EXISTS (SELECT 1 FROM time_entries te
         LEFT JOIN issues i ON i.id = te.issue_id
         WHERE te.deleted_at IS NULL AND te.user_id = ",
    );
    qb.push_bind(user)
        .push(" AND te.spent_on = ")
        .push_bind(day)
        .push(" AND (COALESCE(time_visibility(te.project_id, ")
        .push_bind(viewer)
        .push("), '') <> 'all' OR (te.issue_id IS NOT NULL AND i.archived_at IS NOT NULL)")
        .push(" OR (te.issue_id IS NOT NULL AND NOT COALESCE((");
    // COALESCE: the predicate is NULL (not false) for an unassigned issue.
    db::push_pred(&mut qb, "i", &scope.predicate());
    qb.push("), false))))");
    Ok(qb.build_query_scalar().fetch_one(&mut *conn).await?)
}

/// The projects a document's time ops write to (for the agent's one-project
/// rule); a missing entry or issue is left to the plan (`not_found`).
pub(crate) async fn cmd_projects(
    conn: &mut PgConnection,
    cmds: &[TimeCmd<'_>],
) -> Result<BTreeSet<Uuid>> {
    let mut out = BTreeSet::new();
    for c in cmds {
        let (p, issue): (Option<Uuid>, Option<Uuid>) = match c {
            TimeCmd::Log {
                project_id, issue, ..
            } => (*project_id, *issue),
            TimeCmd::Update {
                entry_id, issue, ..
            } => (
                sqlx::query_scalar("SELECT project_id FROM time_entries WHERE id = $1")
                    .bind(entry_id)
                    .fetch_optional(&mut *conn)
                    .await?,
                issue.flatten(),
            ),
            TimeCmd::Delete { entry_id, .. } => (
                sqlx::query_scalar("SELECT project_id FROM time_entries WHERE id = $1")
                    .bind(entry_id)
                    .fetch_optional(&mut *conn)
                    .await?,
                None,
            ),
        };
        out.extend(p);
        if let Some(i) = issue {
            let ip: Option<Uuid> =
                sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
                    .bind(i)
                    .fetch_optional(&mut *conn)
                    .await?;
            out.extend(ip);
        }
    }
    Ok(out)
}

/// The project of each of `ids` that `scope` sees. Missing, archived and
/// unseen issues are all left out, so a caller cannot tell them apart
/// (看不到＝不存在, M1.11 gate).
pub(crate) async fn seen_issue_projects(
    conn: &mut PgConnection,
    ids: &[Uuid],
    scope: &Scope,
) -> Result<BTreeMap<Uuid, Uuid>> {
    Ok(issue_bits(&mut *conn, ids, false)
        .await?
        .into_iter()
        .filter(|(_, b)| scope.can_see(&b.facts))
        .map(|(id, b)| (id, b.project_id))
        .collect())
}

/// [`cmd_projects`] for an agent's proposal (`scope` is what the agent
/// reads): an issue it cannot see is `not_found` like a missing one, and so
/// is an entry in a project it does not read, before any project is
/// compared (M1.11 gate).
pub(crate) async fn cmd_projects_seen(
    conn: &mut PgConnection,
    cmds: &[TimeCmd<'_>],
    scope: &Scope,
) -> Result<BTreeSet<Uuid>> {
    let readable: BTreeSet<Uuid> = scope.project_ids().collect();
    let mut out = BTreeSet::new();
    for c in cmds {
        let (entry, project, issue) = match c {
            TimeCmd::Log {
                project_id, issue, ..
            } => (None, *project_id, *issue),
            TimeCmd::Update {
                entry_id, issue, ..
            } => (Some(*entry_id), None, issue.flatten()),
            TimeCmd::Delete { entry_id, .. } => (Some(*entry_id), None, None),
        };
        if let Some(e) = entry {
            let p: Option<Uuid> =
                sqlx::query_scalar("SELECT project_id FROM time_entries WHERE id = $1")
                    .bind(e)
                    .fetch_optional(&mut *conn)
                    .await?;
            match p.filter(|p| readable.contains(p)) {
                Some(p) => {
                    out.insert(p);
                }
                None => return Err(entry_not_found()),
            }
        }
        if let Some(i) = issue {
            match seen_issue_projects(&mut *conn, &[i], scope).await?.get(&i) {
                Some(p) => {
                    out.insert(*p);
                }
                None => return Err(issue_not_found(&i.to_string())),
            }
        }
        out.extend(project);
    }
    Ok(out)
}

/// Plans the time ops of a document (§3.5.4, §3.5.5). `issue_ctx` is set for
/// a mixed document, whose issue the engine already locked and checked.
/// Returns the plan and, when locking and the document has only time ops,
/// the receipt token (a mixed document takes its token in the engine).
pub(crate) async fn plan(
    conn: &mut PgConnection,
    ectx: &EvalCtx,
    cmds: &[TimeCmd<'_>],
    issue_ctx: Option<IssueCtx>,
) -> Result<(TimePlan, Option<String>)> {
    let locks = ectx.mode.locks();
    // Entries first (read without a lock: project_id never changes).
    let mut stored: BTreeMap<Uuid, EntryRow> = BTreeMap::new();
    for c in cmds {
        if let TimeCmd::Update { entry_id, .. } | TimeCmd::Delete { entry_id, .. } = c {
            let row = entry_row(&mut *conn, *entry_id, false)
                .await?
                .filter(|r| !r.deleted)
                .ok_or_else(entry_not_found)?;
            stored.insert(*entry_id, row);
        }
    }
    // Issues the document names (the log's issue, an entry's issue, a new one).
    let mut issue_ids: BTreeSet<Uuid> = BTreeSet::new();
    for c in cmds {
        match c {
            TimeCmd::Log { issue: Some(i), .. } => {
                issue_ids.insert(*i);
            }
            TimeCmd::Update {
                entry_id, issue, ..
            } => {
                if let Some(i) = stored[entry_id].issue_id {
                    issue_ids.insert(i);
                }
                if let Some(Some(i)) = issue {
                    issue_ids.insert(*i);
                }
            }
            TimeCmd::Delete { entry_id, .. } => {
                if let Some(i) = stored[entry_id].issue_id {
                    issue_ids.insert(i);
                }
            }
            TimeCmd::Log { issue: None, .. } => {}
        }
    }
    let issue_ids: Vec<Uuid> = issue_ids.into_iter().collect();
    let pre_issues = issue_bits(&mut *conn, &issue_ids, false).await?;
    // The project of every op.
    let mut projects: Vec<Uuid> = Vec::new();
    // A project_id that differs from the issue's project is reported only
    // after the issue's visibility is known (else the answer tells an
    // unseen issue's project apart from a missing issue; M1.11 gate).
    let mut differs: Option<usize> = None;
    for c in cmds {
        let p = match c {
            TimeCmd::Log {
                index,
                project_id,
                issue,
                ..
            } => match (project_id, issue) {
                (_, Some(i)) => {
                    let ip = pre_issues
                        .get(i)
                        .map(|b| b.project_id)
                        .ok_or_else(|| issue_not_found(&i.to_string()))?;
                    if project_id.is_some_and(|p| p != ip) && differs.is_none() {
                        differs = Some(*index);
                    }
                    ip
                }
                (Some(p), None) => *p,
                (None, None) => {
                    return Err(bad(
                        &format!("ops[{index}].project_id"),
                        "log_time without an issue needs project_id",
                    ));
                }
            },
            TimeCmd::Update { entry_id, .. } | TimeCmd::Delete { entry_id, .. } => {
                stored[entry_id].project_id
            }
        };
        projects.push(p);
    }
    let project_set: BTreeSet<Uuid> = projects.iter().copied().collect();

    // Locks: projects, issues, entries (the engine already holds the issue of
    // a mixed document).
    if locks {
        let ids: Vec<Uuid> = project_set.iter().copied().collect();
        sqlx::query("SELECT 1 FROM projects WHERE id = ANY($1) ORDER BY id FOR SHARE")
            .bind(&ids)
            .execute(&mut *conn)
            .await?;
    }
    let issues = if locks && issue_ctx.is_none() {
        issue_bits(&mut *conn, &issue_ids, true).await?
    } else {
        pre_issues
    };
    if locks {
        for (id, row) in stored.iter_mut() {
            *row = entry_row(&mut *conn, *id, true)
                .await?
                .filter(|r| !r.deleted)
                .ok_or_else(entry_not_found)?;
        }
    }

    // Visibility and permissions.
    let scope = db::load_scope(&mut *conn, ectx.caller).await?;
    let mut actor = perm::actor(&mut *conn, ectx.caller).await?;
    if let Some(c) = &ectx.ceiling {
        actor.delegations = Some(c.clone());
    }
    let agent = actor.is_agent();
    let me = actor
        .user_id
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "sign in to log time"))?;
    let visible_projects: BTreeSet<Uuid> = scope.project_ids().collect();
    let issue_visible = |id: Uuid| issues.get(&id).is_some_and(|b| scope.can_see(&b.facts));
    // An entry the caller cannot read does not exist for them: checked before
    // anything else about it (project, module, permissions), so every such
    // case is the same `not_found`.
    for c in cmds {
        if let TimeCmd::Update { entry_id, .. } | TimeCmd::Delete { entry_id, .. } = c {
            let row = &stored[entry_id];
            // With the module off nobody reads time there; the roles still
            // tell whether this caller could (then `module_disabled` below).
            let vis: Option<String> = if visible_projects.contains(&row.project_id) {
                sqlx::query_scalar("SELECT time_role_visibility($1, $2)")
                    .bind(row.project_id)
                    .bind(me)
                    .fetch_one(&mut *conn)
                    .await?
            } else {
                None
            };
            let sees = match vis.as_deref() {
                Some("all") => true,
                Some("own") => row.user_id == me,
                _ => false,
            };
            if !sees || row.issue_id.is_some_and(|i| !issue_visible(i)) {
                return Err(entry_not_found());
            }
        }
    }
    // Likewise an issue the caller cannot read: before its project is
    // looked at, or an issue in a project they cannot see would answer
    // "no project" where a missing one answers "no issue" (M1.11 gate).
    for c in cmds {
        let named = match c {
            TimeCmd::Log { issue: Some(i), .. } => Some(*i),
            TimeCmd::Update {
                issue: Some(Some(i)),
                ..
            } => Some(*i),
            _ => None,
        };
        if let Some(i) = named.filter(|i| !issue_visible(*i)) {
            return Err(issue_not_found(&i.to_string()));
        }
    }
    if let Some(index) = differs {
        return Err(bad(
            &format!("ops[{index}].project_id"),
            "project_id and the issue's project differ",
        ));
    }
    let mut perms: BTreeMap<Uuid, ProjectPerms> = BTreeMap::new();
    for p in &project_set {
        if !visible_projects.contains(p) {
            return Err(project_not_found());
        }
        let pp = perm::project_perms(&mut *conn, &actor, *p)
            .await?
            .ok_or_else(project_not_found)?;
        perms.insert(*p, pp);
    }
    let rules = rules(&mut *conn).await?;
    let today = crate::versions::today(&mut *conn).await?;

    let mut plan = TimePlan::default();
    for (c, project) in cmds.iter().zip(&projects) {
        let (index, op_id) = (c.index(), c.op_id());
        let pp = &perms[project];
        if !module_enabled(&mut *conn, *project).await? {
            return Err(forbidden_at(
                index,
                op_id,
                "module_disabled",
                "time tracking is not enabled in this project",
            ));
        }
        if pp.status != "active" {
            return Err(forbidden_at(
                index,
                op_id,
                "project_not_writable",
                "the project does not accept changes",
            ));
        }
        let kind = match c {
            TimeCmd::Log { .. } => perm::OP_LOG_TIME,
            TimeCmd::Update { .. } => perm::OP_UPDATE_TIME_ENTRY,
            TimeCmd::Delete { .. } => "delete_time_entry",
        };
        if agent {
            if matches!(c, TimeCmd::Delete { .. }) {
                return Err(bad(
                    &format!("ops[{index}].kind"),
                    "delete_time_entry is not a command agents have",
                ));
            }
            if !actor.delegation_allows(*project, kind, None) {
                return Err(forbidden_at(
                    index,
                    op_id,
                    "permission",
                    "the delegation does not allow this command here",
                ));
            }
        }
        match c {
            TimeCmd::Log { issue, fields, .. } => {
                if issue.is_some_and(|i| !issue_visible(i)) {
                    return Err(issue_not_found(&issue.unwrap_or_default().to_string()));
                }
                if !has(pp, LOG_TIME) {
                    return Err(forbidden_at(
                        index,
                        op_id,
                        "permission",
                        "log_time is needed",
                    ));
                }
                let user = fields.user_id.unwrap_or(me);
                if user != me {
                    if agent {
                        return Err(forbidden_at(
                            index,
                            op_id,
                            "agent_time_scope",
                            "an agent logs time only for its delegator",
                        ));
                    }
                    if !has(pp, LOG_TIME_FOR_OTHER_USERS) {
                        return Err(forbidden_at(
                            index,
                            op_id,
                            "permission",
                            "log_time_for_other_users is needed to log time for someone else",
                        ));
                    }
                    if !assignable(&mut *conn, *project, user).await? {
                        return Err(invalid_at(
                            index,
                            op_id,
                            "user_id",
                            "user_invalid",
                            "time can be logged only for an active member who may log time here",
                        ));
                    }
                }
                let cents = match &fields.hours {
                    Some(h) => check_hours(index, op_id, h, &rules)?,
                    None => return Err(required_at(index, op_id, "hours")),
                };
                let spent_on = match &fields.spent_on {
                    Some(s) => check_date(index, op_id, s, &rules, today)?,
                    None => today,
                };
                let avail = available_activities(&mut *conn, *project).await?;
                let activity = match fields.activity_id {
                    Some(a) => {
                        if !avail.iter().any(|x| x.0 == a) {
                            return Err(invalid_at(
                                index,
                                op_id,
                                "activity_id",
                                "activity_inactive",
                                "the activity is not available in this project",
                            ));
                        }
                        a
                    }
                    None => avail
                        .iter()
                        .find(|x| x.2)
                        .map(|x| x.0)
                        .ok_or_else(|| required_at(index, op_id, "activity_id"))?,
                };
                let comments = fields.comments.clone().unwrap_or_default();
                check_texts(index, op_id, *issue, &comments, &rules)?;
                if let Some(i) = issue
                    && !rules.accept_closed_issues
                {
                    let closed = match issue_ctx {
                        Some(ctx) if ctx.id == *i => ctx.closed_before && ctx.closed_after,
                        _ => issues[i].closed,
                    };
                    if closed {
                        return Err(invalid_at(
                            index,
                            op_id,
                            "issue",
                            "closed_issue",
                            "time cannot be logged on a closed issue",
                        ));
                    }
                }
                *plan.deltas.entry((user, spent_on)).or_default() += cents;
                let mut changes = Map::new();
                changes.insert(
                    "hours".into(),
                    json!({"old": null, "new": sql_hours(cents)}),
                );
                changes.insert(
                    "spent_on".into(),
                    json!({"old": null, "new": spent_on.to_string()}),
                );
                changes.insert("activity_id".into(), json!({"old": null, "new": activity}));
                changes.insert("comments".into(), json!({"old": null, "new": comments}));
                changes.insert("issue_id".into(), json!({"old": null, "new": issue}));
                changes.insert("user_id".into(), json!({"old": null, "new": user}));
                plan.entries.push(EntryPlan {
                    op_id,
                    kind: "created",
                    id: Uuid::now_v7(),
                    project_id: *project,
                    issue_id: *issue,
                    user_id: user,
                    activity_id: activity,
                    spent_on,
                    cents,
                    comments,
                    changes,
                    seq: 1,
                    lock_version: 0,
                });
            }
            TimeCmd::Update {
                entry_id,
                lock_version,
                set,
                issue,
                ..
            } => {
                let row = &stored[entry_id];
                check_edit(index, op_id, pp, row, me, agent)?;
                if agent && set.user_id.is_some() {
                    return Err(forbidden_at(
                        index,
                        op_id,
                        "agent_time_scope",
                        "an agent cannot move time to another person",
                    ));
                }
                if row.lock_version != *lock_version {
                    return Err(stale(row, *lock_version));
                }
                let mut changes = Map::new();
                let mut next = row.clone();
                if let Some(u) = set.user_id
                    && u != row.user_id
                {
                    if !has(pp, LOG_TIME_FOR_OTHER_USERS) {
                        return Err(forbidden_at(
                            index,
                            op_id,
                            "permission",
                            "log_time_for_other_users is needed to change whose time this is",
                        ));
                    }
                    if !assignable(&mut *conn, row.project_id, u).await? {
                        return Err(invalid_at(
                            index,
                            op_id,
                            "user_id",
                            "user_invalid",
                            "time can be logged only for an active member who may log time here",
                        ));
                    }
                    changes.insert("user_id".into(), json!({"old": row.user_id, "new": u}));
                    next.user_id = u;
                }
                if let Some(h) = &set.hours {
                    let cents = check_hours(index, op_id, h, &rules)?;
                    if cents != row.cents {
                        changes.insert(
                            "hours".into(),
                            json!({"old": sql_hours(row.cents), "new": sql_hours(cents)}),
                        );
                        next.cents = cents;
                    }
                }
                if let Some(s) = &set.spent_on {
                    let d = check_date(index, op_id, s, &rules, today)?;
                    if d != row.spent_on {
                        changes.insert(
                            "spent_on".into(),
                            json!({"old": row.spent_on.to_string(), "new": d.to_string()}),
                        );
                        next.spent_on = d;
                    }
                }
                if let Some(a) = set.activity_id
                    && a != row.activity_id
                {
                    let avail = available_activities(&mut *conn, row.project_id).await?;
                    if !avail.iter().any(|x| x.0 == a) {
                        return Err(invalid_at(
                            index,
                            op_id,
                            "activity_id",
                            "activity_inactive",
                            "the activity is not available in this project",
                        ));
                    }
                    changes.insert(
                        "activity_id".into(),
                        json!({"old": row.activity_id, "new": a}),
                    );
                    next.activity_id = a;
                }
                if let Some(cm) = &set.comments
                    && *cm != row.comments
                {
                    changes.insert("comments".into(), json!({"old": row.comments, "new": cm}));
                    next.comments = cm.clone();
                }
                if let Some(ni) = issue
                    && *ni != row.issue_id
                {
                    if let Some(i) = ni {
                        let same = issues
                            .get(i)
                            .is_some_and(|b| b.project_id == row.project_id);
                        if !same || !issue_visible(*i) {
                            return Err(issue_not_found(&i.to_string()));
                        }
                    }
                    changes.insert("issue_id".into(), json!({"old": row.issue_id, "new": ni}));
                    next.issue_id = *ni;
                }
                check_texts(index, op_id, next.issue_id, &next.comments, &rules)?;
                if let Some(i) = next.issue_id
                    && !rules.accept_closed_issues
                    && !changes.is_empty()
                    && issues.get(&i).is_some_and(|b| b.closed)
                {
                    return Err(invalid_at(
                        index,
                        op_id,
                        "issue",
                        "closed_issue",
                        "time on a closed issue cannot be changed",
                    ));
                }
                if !changes.is_empty() {
                    *plan.deltas.entry((row.user_id, row.spent_on)).or_default() -= row.cents;
                    *plan
                        .deltas
                        .entry((next.user_id, next.spent_on))
                        .or_default() += next.cents;
                }
                let changed = !changes.is_empty();
                plan.entries.push(EntryPlan {
                    op_id,
                    kind: "updated",
                    id: row.id,
                    project_id: row.project_id,
                    issue_id: next.issue_id,
                    user_id: next.user_id,
                    activity_id: next.activity_id,
                    spent_on: next.spent_on,
                    cents: next.cents,
                    comments: next.comments,
                    changes,
                    seq: row.last_event_seq + 1,
                    lock_version: row.lock_version + i32::from(changed),
                });
            }
            TimeCmd::Delete {
                entry_id,
                lock_version,
                ..
            } => {
                let row = &stored[entry_id];
                check_edit(index, op_id, pp, row, me, agent)?;
                if row.lock_version != *lock_version {
                    return Err(stale(row, *lock_version));
                }
                *plan.deltas.entry((row.user_id, row.spent_on)).or_default() -= row.cents;
                let mut changes = Map::new();
                changes.insert(
                    "_deleted".into(),
                    json!({"old": snapshot(row), "new": null}),
                );
                plan.entries.push(EntryPlan {
                    op_id,
                    kind: "deleted",
                    id: row.id,
                    project_id: row.project_id,
                    issue_id: row.issue_id,
                    user_id: row.user_id,
                    activity_id: row.activity_id,
                    spent_on: row.spent_on,
                    cents: row.cents,
                    comments: row.comments.clone(),
                    changes,
                    seq: row.last_event_seq + 1,
                    lock_version: row.lock_version + 1,
                });
            }
        }
    }

    // Daily totals (T6), locked in (user, day) order.
    plan.deltas.retain(|_, d| *d != 0);
    let keys: Vec<(Uuid, NaiveDate)> = plan.deltas.keys().copied().collect();
    let totals = if keys.is_empty() {
        BTreeMap::new()
    } else {
        lock_totals(&mut *conn, &keys, locks).await?
    };
    if rules.max_cents > 0 {
        for (k, d) in &plan.deltas {
            let now = totals.get(k).copied().unwrap_or(0);
            if *d > 0 && now + d > rules.max_cents {
                let e = plan
                    .entries
                    .iter()
                    .find(|e| (e.user_id, e.spent_on) == *k)
                    .expect("a positive delta comes from an entry");
                let index = cmds
                    .iter()
                    .find(|c| c.op_id() == e.op_id)
                    .map_or(0, TimeCmd::index);
                let mut extra = json!({"max_hours_per_day": fmt_hours(rules.max_cents)});
                // §3.5.4 / A1: an agent's `me` is its delegator, whose day
                // spans projects the agent may not read: never the remaining.
                if !agent && (k.0 == me || sees_whole_day(&mut *conn, me, &scope, k.0, k.1).await?)
                {
                    extra["remaining"] = json!(fmt_hours((rules.max_cents - now).max(0)));
                }
                return Err(invalid_with(
                    index,
                    e.op_id,
                    "hours",
                    "max_hours_per_day",
                    "this would exceed the hours allowed per day",
                    extra,
                ));
            }
        }
    }
    let token = if locks && issue_ctx.is_none() {
        let t = commands::search_token(&mut *conn).await?;
        commands::check_deadlines(&mut *conn, &ectx.deadlines).await?;
        Some(t)
    } else {
        None
    };
    Ok((plan, token))
}

fn stale(row: &EntryRow, expected: i32) -> anyhow::Error {
    ApiError::new(
        ErrorCode::PreconditionFailed,
        "the time entry changed meanwhile",
    )
    .with_details(
        json!({"time_entry_id": row.id, "expected_lock_version": expected,
                         "lock_version": row.lock_version}),
    )
    .into()
}

/// Redmine `TimeEntry#editable_by?` plus the agent limit (§3.5.4).
fn check_edit(
    index: usize,
    op_id: Uuid,
    pp: &ProjectPerms,
    row: &EntryRow,
    me: Uuid,
    agent: bool,
) -> Result<()> {
    if agent && row.user_id != me {
        return Err(forbidden_at(
            index,
            op_id,
            "agent_time_scope",
            "an agent changes only its delegator's time",
        ));
    }
    if has(pp, EDIT_TIME_ENTRIES) || (row.user_id == me && has(pp, EDIT_OWN_TIME_ENTRIES)) {
        Ok(())
    } else {
        Err(forbidden_at(
            index,
            op_id,
            "permission",
            "edit_time_entries, or edit_own_time_entries on your own time, is needed",
        ))
    }
}

fn check_hours(index: usize, op_id: Uuid, v: &Json, rules: &Rules) -> Result<i64> {
    let cents = parse_hours(v).ok_or_else(|| {
        invalid_at(
            index,
            op_id,
            "hours",
            "hours_invalid",
            "hours is a decimal string from 0 to 1000 with at most two decimals",
        )
    })?;
    if cents == 0 && !rules.accept_0_hours {
        return Err(invalid_at(
            index,
            op_id,
            "hours",
            "hours_invalid",
            "zero hours are not accepted",
        ));
    }
    Ok(cents)
}

fn check_date(
    index: usize,
    op_id: Uuid,
    s: &str,
    rules: &Rules,
    today: NaiveDate,
) -> Result<NaiveDate> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .filter(|d| {
            *d >= NaiveDate::from_ymd_opt(1900, 1, 1).expect("valid")
                && *d <= NaiveDate::from_ymd_opt(2199, 12, 31).expect("valid")
                && s.len() == 10
        })
        .ok_or_else(|| {
            invalid_at(
                index,
                op_id,
                "spent_on",
                "date_invalid",
                "spent_on is a date YYYY-MM-DD from 1900-01-01 to 2199-12-31",
            )
        })?;
    if !rules.accept_future_dates && d > today {
        return Err(invalid_at(
            index,
            op_id,
            "spent_on",
            "future_date",
            "time cannot be logged on a future date",
        ));
    }
    Ok(d)
}

fn check_texts(
    index: usize,
    op_id: Uuid,
    issue: Option<Uuid>,
    comments: &str,
    rules: &Rules,
) -> Result<()> {
    if rules.require_issue && issue.is_none() {
        return Err(required_at(index, op_id, "issue"));
    }
    if rules.require_comments && comments.trim().is_empty() {
        return Err(required_at(index, op_id, "comments"));
    }
    if comments.chars().count() > MAX_COMMENTS {
        return Err(invalid_at(
            index,
            op_id,
            "comments",
            "too_long",
            "comments are at most 1024 characters",
        ));
    }
    Ok(())
}

fn snapshot(row: &EntryRow) -> Json {
    json!({"hours": sql_hours(row.cents), "spent_on": row.spent_on.to_string(),
           "activity_id": row.activity_id, "comments": row.comments,
           "issue_id": row.issue_id, "user_id": row.user_id})
}

/// Writes a planned time document with `at`'s attribution: the rows, the
/// daily totals, the events, the field origins and — for agent writes —
/// the before/after copies for undo.
pub(crate) async fn write_plan(
    conn: &mut PgConnection,
    plan: &TimePlan,
    at: &Attribution,
) -> Result<()> {
    for e in &plan.entries {
        if e.changes.is_empty() {
            continue;
        }
        match e.kind {
            "created" => {
                sqlx::query(
                    "INSERT INTO time_entries (id, project_id, issue_id, user_id, author_id,
                         created_on_behalf_of_id, activity_id, spent_on, hours, comments, last_event_seq)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, $10, 1)",
                )
                .bind(e.id)
                .bind(e.project_id)
                .bind(e.issue_id)
                .bind(e.user_id)
                .bind(at.author_id)
                .bind(at.author_on_behalf)
                .bind(e.activity_id)
                .bind(e.spent_on)
                .bind(sql_hours(e.cents))
                .bind(&e.comments)
                .execute(&mut *conn)
                .await?;
            }
            "updated" => {
                sqlx::query(
                    "UPDATE time_entries SET issue_id = $2, user_id = $3, activity_id = $4, spent_on = $5,
                            hours = $6::numeric, comments = $7, lock_version = lock_version + 1,
                            redacted_at = CASE WHEN comments IS DISTINCT FROM $7 THEN NULL ELSE redacted_at END,
                            last_event_seq = $8, updated_at = now()
                     WHERE id = $1",
                )
                .bind(e.id)
                .bind(e.issue_id)
                .bind(e.user_id)
                .bind(e.activity_id)
                .bind(e.spent_on)
                .bind(sql_hours(e.cents))
                .bind(&e.comments)
                .bind(e.seq)
                .execute(&mut *conn)
                .await?;
            }
            _ => {
                sqlx::query(
                    "UPDATE time_entries SET deleted_at = now(), deleted_by_execution_id = $2,
                            lock_version = lock_version + 1, last_event_seq = $3, updated_at = now()
                     WHERE id = $1",
                )
                .bind(e.id)
                .bind(at.execution_ref)
                .bind(e.seq)
                .execute(&mut *conn)
                .await?;
            }
        }
        write_event(&mut *conn, e.id, e.seq, e.kind, at, &e.changes).await?;
        if at.record_changes {
            let rows: Vec<(&str, Json, Json)> = match e.kind {
                "created" => vec![(
                    "_created",
                    Json::Null,
                    json!({"hours": sql_hours(e.cents), "spent_on": e.spent_on.to_string(),
                           "activity_id": e.activity_id, "comments": e.comments,
                           "issue_id": e.issue_id, "user_id": e.user_id}),
                )],
                "deleted" => vec![("_deleted", e.changes["_deleted"]["old"].clone(), Json::Null)],
                _ => e
                    .changes
                    .iter()
                    .map(|(k, c)| (k.as_str(), c["old"].clone(), c["new"].clone()))
                    .collect(),
            };
            for (field, before, after) in rows {
                sqlx::query(
                    "INSERT INTO execution_time_entry_changes
                         (execution_id, time_entry_id, field_key, before_value, after_value)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(at.execution_ref)
                .bind(e.id)
                .bind(field)
                .bind(before)
                .bind(after)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    apply_deltas(&mut *conn, &plan.deltas).await
}

/// Adds the changes to the (already locked) daily totals.
pub(crate) async fn apply_deltas(
    conn: &mut PgConnection,
    deltas: &BTreeMap<(Uuid, NaiveDate), i64>,
) -> Result<()> {
    for ((u, d), c) in deltas {
        if *c == 0 {
            continue;
        }
        // The plan locked (and created) every row it changes.
        let r = sqlx::query(
            "UPDATE time_daily_totals SET hours = hours + $3::numeric
             WHERE user_id = $1 AND spent_on = $2",
        )
        .bind(u)
        .bind(d)
        .bind(if *c < 0 {
            format!("-{}", sql_hours(-c))
        } else {
            sql_hours(*c)
        })
        .execute(&mut *conn)
        .await?;
        anyhow::ensure!(
            r.rows_affected() == 1,
            "daily total ({u}, {d}) was not locked"
        );
    }
    Ok(())
}

pub(crate) async fn write_event(
    conn: &mut PgConnection,
    entry: Uuid,
    seq: i64,
    kind: &str,
    at: &Attribution,
    changes: &Map<String, Json>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO time_entry_events (time_entry_id, seq, kind, origin, actor_id, actor_kind,
                                        on_behalf_of_id, approver_id, delegation_id,
                                        execution_id, operation_request_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(entry)
    .bind(seq)
    .bind(kind)
    .bind(at.origin)
    .bind(at.actor_id)
    .bind(at.actor_kind)
    .bind(at.on_behalf_of_id)
    .bind(at.approver_id)
    .bind(at.delegation_id)
    .bind(at.execution_ref)
    .bind(at.request_id)
    .bind(json!({"changes": changes}))
    .execute(&mut *conn)
    .await?;
    for field in changes.keys().filter(|k| !k.starts_with('_')) {
        sqlx::query(
            "INSERT INTO time_entry_field_origins (time_entry_id, field_key, origin_execution_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (time_entry_id, field_key) DO UPDATE SET origin_execution_id = EXCLUDED.origin_execution_id",
        )
        .bind(entry)
        .bind(field)
        .bind(at.execution_ref)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- undo (T-M1-41)

/// One time entry change of an agent execution (contract 05 §3.10.2,
/// T-M1-41): a created entry is undone by a soft delete, a changed field
/// goes back to its value before.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoTimeEntry {
    pub time_entry_id: Uuid,
    /// `created` (undo deletes the entry) or the field: hours, spent_on,
    /// activity_id, comments, issue_id.
    pub field: String,
    pub before: Json,
    pub after: Json,
    /// The value now; for `created`, the entry now (null once deleted).
    pub current: Json,
    pub restorable: bool,
    /// `expired`, `redacted`, `changed_since` (changed or deleted after the
    /// execution), `permission` (cannot edit or delete that entry now) or
    /// `not_supported` (issue_id: change it by hand).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The time entries an execution changed and has not had undone (sorted).
pub(crate) async fn undo_entries(conn: &mut PgConnection, exec: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT time_entry_id FROM execution_time_entry_changes
         WHERE execution_id = $1 AND reverted_by IS NULL ORDER BY 1",
    )
    .bind(exec)
    .fetch_all(&mut *conn)
    .await?)
}

fn field_value(row: &EntryRow, field: &str) -> Json {
    match field {
        "hours" => json!(sql_hours(row.cents)),
        "spent_on" => json!(row.spent_on.to_string()),
        "activity_id" => json!(row.activity_id),
        "comments" => json!(row.comments),
        "issue_id" => json!(row.issue_id),
        "user_id" => json!(row.user_id),
        _ => Json::Null,
    }
}

/// The preview rows of `exec` that `caller` (the person) can see now.
/// `visible_issues`: the issues of those entries the caller can see.
pub(crate) async fn undo_preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    exec: Uuid,
    expired: bool,
    visible_issues: &BTreeSet<Uuid>,
) -> Result<Vec<UndoTimeEntry>> {
    let Some(me) = caller.user_id() else {
        return Ok(Vec::new());
    };
    type Change = (Uuid, String, Option<Json>, Option<Json>);
    let changes: Vec<Change> = sqlx::query_as(
        "SELECT time_entry_id, field_key, before_value, after_value FROM execution_time_entry_changes
         WHERE execution_id = $1 AND reverted_by IS NULL AND field_key <> '_deleted'
         ORDER BY time_entry_id, field_key",
    )
    .bind(exec)
    .fetch_all(&mut *conn)
    .await?;
    let actor = crate::perm::actor(&mut *conn, caller).await?;
    let mut out = Vec::new();
    for (entry, field, before, after) in changes {
        let Some(row) = entry_row(&mut *conn, entry, false).await? else {
            continue;
        };
        // Not readable now: not even listed.
        let readable = match visibility(&mut *conn, row.project_id, me).await?.as_deref() {
            Some("all") => true,
            Some("own") => row.user_id == me,
            _ => false,
        } && row.issue_id.is_none_or(|i| visible_issues.contains(&i));
        if !readable {
            continue;
        }
        let created = field == "_created";
        let others: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM time_entry_field_origins
                            WHERE time_entry_id = $1 AND ($3 OR field_key = $4)
                              AND origin_execution_id IS DISTINCT FROM $2)",
        )
        .bind(entry)
        .bind(exec)
        .bind(created)
        .bind(&field)
        .fetch_one(&mut *conn)
        .await?;
        let before = before.unwrap_or(Json::Null);
        let after = after.unwrap_or(Json::Null);
        let redacted = |v: &Json| v == &json!(REDACTED) || v["comments"] == json!(REDACTED);
        let reason = if expired {
            Some("expired")
        } else if !created && (redacted(&before) || redacted(&after)) {
            Some("redacted")
        } else if row.deleted || others {
            Some("changed_since")
        } else if field == "issue_id" || field == "user_id" {
            Some("not_supported")
        } else {
            let ok = match crate::perm::project_perms(&mut *conn, &actor, row.project_id).await? {
                Some(pp) => {
                    // Taking back an entry the agent created for me (nobody
                    // changed it since: `others` above) needs only log_time,
                    // the permission it was created under; putting fields
                    // back is an edit (M1.4-D15, pending the user's call).
                    pp.writable()
                        && module_enabled(&mut *conn, row.project_id).await?
                        && (has(&pp, EDIT_TIME_ENTRIES)
                            || (row.user_id == me && has(&pp, EDIT_OWN_TIME_ENTRIES))
                            || (created && row.user_id == me && has(&pp, LOG_TIME)))
                }
                None => false,
            };
            (!ok).then_some("permission")
        };
        out.push(UndoTimeEntry {
            time_entry_id: entry,
            field: if created {
                "created".into()
            } else {
                field.clone()
            },
            current: if created {
                if row.deleted {
                    Json::Null
                } else {
                    snapshot(&row)
                }
            } else {
                field_value(&row, &field)
            },
            before,
            after,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }
    Ok(out)
}

/// What undoing `items` does to the entries (locked by the caller): the
/// new rows, and the daily totals they touch.
pub(crate) struct UndoTimePlan {
    rows: Vec<(EntryRow, Option<EntryRow>, Map<String, Json>)>,
    pub keys: Vec<(Uuid, NaiveDate)>,
}

pub(crate) async fn undo_plan(
    conn: &mut PgConnection,
    items: &[&UndoTimeEntry],
) -> Result<UndoTimePlan> {
    let mut by_entry: BTreeMap<Uuid, Vec<&UndoTimeEntry>> = BTreeMap::new();
    for it in items {
        by_entry.entry(it.time_entry_id).or_default().push(it);
    }
    let mut rows = Vec::new();
    let mut keys = BTreeSet::new();
    for (entry, its) in by_entry {
        let Some(row) = entry_row(&mut *conn, entry, false).await? else {
            continue;
        };
        keys.insert((row.user_id, row.spent_on));
        let mut changes = Map::new();
        if its.iter().any(|i| i.field == "created") {
            changes.insert(
                "_deleted".into(),
                json!({"old": snapshot(&row), "new": null}),
            );
            rows.push((row, None, changes));
            continue;
        }
        let mut next = row.clone();
        for it in its {
            let bad = || anyhow::anyhow!("undo value of {} is not usable", it.field);
            match it.field.as_str() {
                "hours" => next.cents = parse_hours(&it.before).ok_or_else(bad)?,
                "spent_on" => {
                    next.spent_on = it
                        .before
                        .as_str()
                        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                        .ok_or_else(bad)?
                }
                "activity_id" => {
                    next.activity_id =
                        serde_json::from_value(it.before.clone()).map_err(|_| bad())?
                }
                "comments" => next.comments = it.before.as_str().ok_or_else(bad)?.to_string(),
                _ => continue,
            }
            changes.insert(
                it.field.clone(),
                json!({"old": field_value(&row, &it.field), "new": field_value(&next, &it.field)}),
            );
        }
        keys.insert((next.user_id, next.spent_on));
        rows.push((row, Some(next), changes));
    }
    Ok(UndoTimePlan {
        rows,
        keys: keys.into_iter().collect(),
    })
}

/// Locks (creating where missing) the daily totals of an undo plan; after
/// the entries, before anything later in the lock order (§3.10.2).
pub(crate) async fn undo_lock(conn: &mut PgConnection, plan: &UndoTimePlan) -> Result<()> {
    lock_totals(&mut *conn, &plan.keys, true).await?;
    Ok(())
}

/// Writes an undo plan as `at` (the person's undo execution); marks the
/// changes of `exec` reverted. The number of entries changed.
pub(crate) async fn undo_write(
    conn: &mut PgConnection,
    plan: &UndoTimePlan,
    exec: Uuid,
    at: &Attribution,
) -> Result<usize> {
    let mut deltas: BTreeMap<(Uuid, NaiveDate), i64> = BTreeMap::new();
    for (row, next, changes) in &plan.rows {
        let seq = row.last_event_seq + 1;
        *deltas.entry((row.user_id, row.spent_on)).or_default() -= row.cents;
        let kind = match next {
            None => {
                sqlx::query(
                    "UPDATE time_entries SET deleted_at = now(), deleted_by_execution_id = $2,
                            lock_version = lock_version + 1, last_event_seq = $3, updated_at = now()
                     WHERE id = $1",
                )
                .bind(row.id)
                .bind(at.execution_ref)
                .bind(seq)
                .execute(&mut *conn)
                .await?;
                "deleted"
            }
            Some(n) => {
                *deltas.entry((n.user_id, n.spent_on)).or_default() += n.cents;
                sqlx::query(
                    "UPDATE time_entries SET activity_id = $2, spent_on = $3, hours = $4::numeric,
                            comments = $5, lock_version = lock_version + 1, last_event_seq = $6,
                            redacted_at = CASE WHEN comments IS DISTINCT FROM $5 THEN NULL ELSE redacted_at END,
                            updated_at = now()
                     WHERE id = $1",
                )
                .bind(row.id)
                .bind(n.activity_id)
                .bind(n.spent_on)
                .bind(sql_hours(n.cents))
                .bind(&n.comments)
                .bind(seq)
                .execute(&mut *conn)
                .await?;
                "updated"
            }
        };
        write_event(&mut *conn, row.id, seq, kind, at, changes).await?;
        // Only what this undo wrote back: a skipped or unrestorable field
        // keeps its copy (it can still be undone later).
        let fields: Vec<String> = match next {
            None => vec!["_created".into()],
            Some(_) => changes.keys().cloned().collect(),
        };
        sqlx::query(
            "UPDATE execution_time_entry_changes SET reverted_by = $3
             WHERE execution_id = $1 AND time_entry_id = $2 AND reverted_by IS NULL
               AND field_key = ANY($4)",
        )
        .bind(exec)
        .bind(row.id)
        .bind(at.execution_ref)
        .bind(&fields)
        .execute(&mut *conn)
        .await?;
    }
    apply_deltas(&mut *conn, &deltas).await?;
    Ok(plan.rows.len())
}
