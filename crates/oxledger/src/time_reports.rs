// SPDX-License-Identifier: GPL-2.0-or-later
//! Time tracking reads and settings (contract 05 §3.5.1–§3.5.3): one entry,
//! the list, the summary report, the weekly timesheet, activities, the
//! timelog settings, and MCP `time_list` (§3.5.4 A1).
//!
//! Every read applies the same visibility (§3.5.4): a live entry in an
//! `active`/`closed` project with the time module, where one of the
//! viewer's roles with `view_time_entries` is `all` (or `own` and the entry
//! is the viewer's), and — when the entry has an issue — the issue is
//! visible to the viewer. `time_visibility()` (0023) computes the first
//! part in SQL; the issue part is the caller's issue predicate.

use crate::auth::AuthContext;
use crate::commands;
use crate::cursor::{self, Position};
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{
    self, EDIT_OWN_TIME_ENTRIES as EDIT_OWN, EDIT_TIME_ENTRIES as EDIT, MANAGE_PROJECT,
};
use crate::service::{App, read_tx};
use crate::time_entries::{available_activities, fmt_hours, has, module_enabled, visibility};
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use ox_core::visibility::Scope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use std::collections::BTreeMap;
use uuid::Uuid;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;
const MAX_REPORT_DAYS: i64 = 366;
const GROUPS: &[&str] = &[
    "user",
    "activity",
    "issue",
    "tracker",
    "fixed_version",
    "day",
    "week",
    "month",
];

fn invalid_arg(path: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no time entry")
}

// ---------------------------------------------------------------- views

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Named {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectRef {
    pub id: Uuid,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IssueRef {
    pub id: Uuid,
    pub key: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuthorRef {
    pub id: Uuid,
    pub name: String,
    /// `user` or `agent`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Named>,
}

/// A time entry (§3.5.1). `can_edit`/`can_delete` only on the single read.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimeEntryView {
    pub id: Uuid,
    pub project: ProjectRef,
    pub issue: Option<IssueRef>,
    pub user: Named,
    pub author: AuthorRef,
    pub activity: Named,
    pub spent_on: NaiveDate,
    /// Decimal string (`"1.50"`).
    pub hours: String,
    /// `[已遮蔽]` once redacted (D12).
    pub comments: String,
    pub redacted: bool,
    pub lock_version: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_edit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_delete: Option<bool>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    project_id: Uuid,
    project_key: String,
    issue_id: Option<Uuid>,
    issue_key: Option<String>,
    subject: Option<String>,
    user_id: Uuid,
    user_name: String,
    author_id: Uuid,
    author_name: String,
    author_kind: String,
    obo_id: Option<Uuid>,
    obo_name: Option<String>,
    activity_id: Uuid,
    activity_name: String,
    spent_on: NaiveDate,
    cents: i64,
    comments: String,
    redacted: bool,
    lock_version: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Row {
    fn view(self) -> TimeEntryView {
        TimeEntryView {
            id: self.id,
            project: ProjectRef {
                id: self.project_id,
                key: self.project_key,
            },
            issue: match (self.issue_id, self.issue_key, self.subject) {
                (Some(id), Some(key), Some(subject)) => Some(IssueRef { id, key, subject }),
                _ => None,
            },
            user: Named {
                id: self.user_id,
                name: self.user_name,
            },
            author: AuthorRef {
                id: self.author_id,
                name: self.author_name,
                kind: self.author_kind,
                on_behalf_of: self
                    .obo_id
                    .zip(self.obo_name)
                    .map(|(id, name)| Named { id, name }),
            },
            activity: Named {
                id: self.activity_id,
                name: self.activity_name,
            },
            spent_on: self.spent_on,
            hours: fmt_hours(self.cents),
            comments: self.comments,
            redacted: self.redacted,
            lock_version: self.lock_version,
            can_edit: None,
            can_delete: None,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

const SELECT: &str = "SELECT te.id, te.project_id, p.key AS project_key, te.issue_id, ik.key AS issue_key,
        i.subject, te.user_id, u.name AS user_name, te.author_id, a.name AS author_name,
        a.kind AS author_kind, te.created_on_behalf_of_id AS obo_id, ob.name AS obo_name,
        te.activity_id, act.name AS activity_name, te.spent_on, (te.hours * 100)::bigint AS cents,
        te.comments, te.redacted_at IS NOT NULL AS redacted, te.lock_version, te.created_at, te.updated_at";

const FROM: &str = " FROM time_entries te
     JOIN projects p ON p.id = te.project_id
     LEFT JOIN issues i ON i.id = te.issue_id
     LEFT JOIN issue_keys ik ON ik.issue_id = i.id AND ik.is_current
     JOIN principals u ON u.id = te.user_id
     JOIN principals a ON a.id = te.author_id
     LEFT JOIN principals ob ON ob.id = te.created_on_behalf_of_id
     JOIN time_entry_activities act ON act.id = te.activity_id";

// ---------------------------------------------------------------- the viewer

/// Whose time reading this is.
pub(crate) struct Viewer {
    /// The user, or an agent's delegator.
    pub me: Uuid,
    pub scope: Scope,
    /// Agents: the delegated projects with `read_own_time` (§3.5.4 A1).
    pub agent_projects: Option<Vec<Uuid>>,
}

pub(crate) async fn viewer(conn: &mut PgConnection, caller: AuthContext) -> Result<Viewer> {
    let eff = db::effective(&mut *conn, caller).await?;
    let me = eff
        .user_id
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "sign in to read time"))?;
    let scope = db::load_scope(&mut *conn, caller).await?;
    let agent_projects = match &eff.agent {
        None => None,
        Some(a) => {
            let ps: Vec<Uuid> = sqlx::query_scalar(
                "SELECT project_id FROM delegations
                 WHERE context_id = $1 AND agent_principal_id = $3 AND read_own_time AND revoked_at IS NULL
                   AND expires_at > now() AND project_id = ANY($2) ORDER BY project_id",
            )
            .bind(a.context_id)
            .bind(&a.projects)
            .bind(a.agent_principal_id)
            .fetch_all(&mut *conn)
            .await?;
            if ps.is_empty() {
                return Err(crate::time_entries::forbidden(
                    "time_read_not_delegated",
                    "the delegation does not let this agent read time (read_own_time is off)",
                ));
            }
            Some(ps)
        }
    };
    Ok(Viewer {
        me,
        scope,
        agent_projects,
    })
}

/// The visibility conditions on `te` (and `i`, the issue, LEFT JOINed).
fn push_visible(qb: &mut QueryBuilder<Postgres>, v: &Viewer) {
    let projects: Vec<Uuid> = v.scope.project_ids().collect();
    qb.push(" te.deleted_at IS NULL AND te.project_id = ANY(")
        .push_bind(projects)
        .push(") AND (CASE time_visibility(te.project_id, ")
        .push_bind(v.me)
        .push(") WHEN 'all' THEN true WHEN 'own' THEN te.user_id = ")
        .push_bind(v.me)
        .push(" ELSE false END) AND (te.issue_id IS NULL OR (i.archived_at IS NULL AND ");
    db::push_pred(qb, "i", &v.scope.predicate());
    qb.push("))");
    if let Some(ps) = &v.agent_projects {
        qb.push(" AND te.user_id = ")
            .push_bind(v.me)
            .push(" AND te.project_id = ANY(")
            .push_bind(ps.clone())
            .push(")");
    }
}

/// `spent_hours` of one issue the caller (a person) can already see (§3.5.4):
/// the visible entries only. `None` when the caller cannot read time there.
pub(crate) async fn issue_spent(
    conn: &mut PgConnection,
    me: Uuid,
    project: Uuid,
    issue: Uuid,
) -> Result<Option<String>> {
    let cents: Option<i64> = sqlx::query_scalar(
        "SELECT CASE time_visibility($1, $2)
                  WHEN 'all' THEN (SELECT (COALESCE(sum(hours), 0) * 100)::bigint FROM time_entries
                                   WHERE issue_id = $3 AND deleted_at IS NULL)
                  WHEN 'own' THEN (SELECT (COALESCE(sum(hours), 0) * 100)::bigint FROM time_entries
                                   WHERE issue_id = $3 AND deleted_at IS NULL AND user_id = $2)
                END",
    )
    .bind(project)
    .bind(me)
    .bind(issue)
    .fetch_one(&mut *conn)
    .await?;
    Ok(cents.map(fmt_hours))
}

// ---------------------------------------------------------------- one entry

pub async fn get(app: &App, caller: AuthContext, id: &str) -> Result<TimeEntryView> {
    let id: Uuid = id.parse().map_err(|_| not_found())?;
    let mut tx = read_tx(app).await?;
    let v = viewer(&mut tx, caller).await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    qb.push(FROM)
        .push(" WHERE te.id = ")
        .push_bind(id)
        .push(" AND");
    push_visible(&mut qb, &v);
    let row: Row = qb
        .build_query_as()
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(not_found)?;
    let (can_edit, can_delete) = abilities(&mut tx, caller, &row).await?;
    let mut view = row.view();
    view.can_edit = Some(can_edit);
    view.can_delete = Some(can_delete);
    tx.commit().await?;
    Ok(view)
}

/// Whether the caller may change and delete a visible entry (§3.5.4).
async fn abilities(
    conn: &mut PgConnection,
    caller: AuthContext,
    row: &Row,
) -> Result<(bool, bool)> {
    let actor = perm::actor(&mut *conn, caller).await?;
    let Some(pp) = perm::project_perms(&mut *conn, &actor, row.project_id).await? else {
        return Ok((false, false));
    };
    let me = actor.user_id;
    let writable = pp.status == "active" && module_enabled(&mut *conn, row.project_id).await?;
    let mut edit = writable && (has(&pp, EDIT) || (me == Some(row.user_id) && has(&pp, EDIT_OWN)));
    let delete = edit && !actor.is_agent();
    if actor.is_agent() {
        edit = edit
            && me == Some(row.user_id)
            && actor.delegation_allows(row.project_id, perm::OP_UPDATE_TIME_ENTRY, None);
    }
    Ok((edit, delete))
}

// ---------------------------------------------------------------- the list

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub project: Option<String>,
    pub issue: Option<String>,
    /// A user id or `me`.
    pub user: Option<String>,
    pub activity: Option<Uuid>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    /// A tracker id, or `none` (entries without an issue): the issue's
    /// current tracker, as `summary` groups it (P3 gap G44).
    pub tracker: Option<String>,
    /// A version id, or `none` (no issue, or an issue without a milestone):
    /// the issue's current fixed_version, as `summary` groups it (G44).
    pub fixed_version: Option<String>,
}

/// A `tracker` / `fixed_version` filter (G44).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum IdOrNone {
    Id(Uuid),
    None,
}

fn id_or_none(path: &str, v: Option<&str>) -> Result<Option<IdOrNone>> {
    match v.map(str::trim) {
        None => Ok(None),
        Some("none") => Ok(Some(IdOrNone::None)),
        Some(s) => s
            .parse::<Uuid>()
            .map(|id| Some(IdOrNone::Id(id)))
            .map_err(|_| invalid_arg(path, &format!("{path} is an id or none"))),
    }
}

/// `column` (on the LEFT JOINed issue `i`) is `f`'s id, or NULL for `none`.
fn push_id_or_none(qb: &mut QueryBuilder<Postgres>, column: &str, f: Option<IdOrNone>) {
    match f {
        None => {}
        Some(IdOrNone::None) => {
            qb.push(format!(" AND {column} IS NULL"));
        }
        Some(IdOrNone::Id(id)) => {
            qb.push(format!(" AND {column} = ")).push_bind(id);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimeEntryPage {
    pub items: Vec<TimeEntryView>,
    pub next_cursor: Option<String>,
}

fn date_arg(path: &str, s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .filter(|_| s.len() == 10)
        .ok_or_else(|| invalid_arg(path, "a date is YYYY-MM-DD"))
}

/// Resolves the shared filters (project, issue, user) to ids.
struct Filters {
    project: Option<Uuid>,
    issue: Option<Uuid>,
    user: Option<Uuid>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

#[allow(clippy::too_many_arguments)]
async fn filters(
    conn: &mut PgConnection,
    caller: AuthContext,
    v: &Viewer,
    project: Option<&str>,
    issue: Option<&str>,
    user: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Filters> {
    let project = match project {
        Some(p) => Some(describe::visible_project(&mut *conn, caller, p).await?),
        None => None,
    };
    let issue = match issue {
        None => None,
        Some(k) => {
            let id: Option<Uuid> = match Uuid::parse_str(k.trim()) {
                Ok(id) => Some(id),
                Err(_) => {
                    sqlx::query_scalar("SELECT issue_id FROM issue_keys WHERE key = $1")
                        .bind(k.trim().to_uppercase())
                        .fetch_optional(&mut *conn)
                        .await?
                }
            };
            // An issue the viewer cannot see filters to nothing (no oracle).
            Some(id.unwrap_or_else(Uuid::nil))
        }
    };
    let user = match user {
        None => None,
        Some("me") => Some(v.me),
        Some(u) => Some(
            u.parse()
                .map_err(|_| invalid_arg("user", "user is a user id or me"))?,
        ),
    };
    let from = from.map(|s| date_arg("from", s)).transpose()?;
    let to = to.map(|s| date_arg("to", s)).transpose()?;
    Ok(Filters {
        project,
        issue,
        user,
        from,
        to,
    })
}

fn push_filters(qb: &mut QueryBuilder<Postgres>, f: &Filters) {
    if let Some(p) = f.project {
        qb.push(" AND te.project_id = ").push_bind(p);
    }
    if let Some(i) = f.issue {
        qb.push(" AND te.issue_id = ").push_bind(i);
    }
    if let Some(u) = f.user {
        qb.push(" AND te.user_id = ").push_bind(u);
    }
    if let Some(d) = f.from {
        qb.push(" AND te.spent_on >= ").push_bind(d);
    }
    if let Some(d) = f.to {
        qb.push(" AND te.spent_on <= ").push_bind(d);
    }
}

pub async fn list(app: &App, caller: AuthContext, q: &ListQuery) -> Result<TimeEntryPage> {
    let mut tx = read_tx(app).await?;
    let v = viewer(&mut tx, caller).await?;
    let out = list_in(&mut tx, app, caller, &v, q).await?;
    tx.commit().await?;
    Ok(out)
}

async fn list_in(
    conn: &mut PgConnection,
    app: &App,
    caller: AuthContext,
    v: &Viewer,
    q: &ListQuery,
) -> Result<TimeEntryPage> {
    let f = filters(
        &mut *conn,
        caller,
        v,
        q.project.as_deref(),
        q.issue.as_deref(),
        q.user.as_deref(),
        q.from.as_deref(),
        q.to.as_deref(),
    )
    .await?;
    let tracker = id_or_none("tracker", q.tracker.as_deref())?;
    let fixed_version = id_or_none("fixed_version", q.fixed_version.as_deref())?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let qhash = cursor::query_hash(&json!({
        "kind": "time_entries", "project": f.project, "issue": f.issue, "user": f.user,
        "activity": q.activity, "from": f.from, "to": f.to,
        "tracker": tracker, "fixed_version": fixed_version,
    }));
    let after = match &q.cursor {
        None => None,
        Some(c) => match cursor::decode(&app.pool, c, caller.principal_id(), &qhash).await? {
            Position::Sort { k, i } if k.len() == 2 => {
                let d = k[0]
                    .as_deref()
                    .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
                let t = k[1]
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Utc));
                match (d, t) {
                    (Some(d), Some(t)) => Some((d, t, i)),
                    _ => return Err(cursor::mismatch()),
                }
            }
            _ => return Err(cursor::mismatch()),
        },
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    qb.push(FROM).push(" WHERE");
    push_visible(&mut qb, v);
    push_filters(&mut qb, &f);
    if let Some(a) = q.activity {
        qb.push(" AND te.activity_id = ").push_bind(a);
    }
    // G44: the summary's tracker / fixed_version groups (`i` is NULL for an
    // entry without an issue, so `none` includes those, as the group does).
    push_id_or_none(&mut qb, "i.tracker_id", tracker);
    push_id_or_none(&mut qb, "i.fixed_version_id", fixed_version);
    if let Some((d, t, i)) = after {
        qb.push(" AND (te.spent_on, te.created_at, te.id) < (")
            .push_bind(d)
            .push(", ")
            .push_bind(t)
            .push(", ")
            .push_bind(i)
            .push(")");
    }
    qb.push(" ORDER BY te.spent_on DESC, te.created_at DESC, te.id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);
    let mut rows: Vec<Row> = qb.build_query_as().fetch_all(&mut *conn).await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = match (more, rows.last()) {
        (true, Some(r)) => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                Position::Sort {
                    k: vec![
                        Some(r.spent_on.to_string()),
                        Some(r.created_at.to_rfc3339()),
                    ],
                    i: r.id,
                },
            )
            .await?,
        ),
        _ => None,
    };
    Ok(TimeEntryPage {
        items: rows.into_iter().map(Row::view).collect(),
        next_cursor,
    })
}

/// MCP `time_list` (§3.5.4 A1): an agent reads its delegator's own time,
/// in delegated projects with `read_own_time`, on issues it may read.
pub async fn list_for_tool(app: &App, caller: AuthContext, q: &ListQuery) -> Result<TimeEntryPage> {
    list(app, caller, q).await
}

/// MCP `time_list` (contract 05 §4.1, §3.5.4 A1(c)).
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeListParams {
    /// Project key or id (one of your delegated projects).
    #[serde(default)]
    pub project: Option<String>,
    /// Issue key such as "AUTH-12".
    #[serde(default)]
    pub issue: Option<String>,
    /// First day, YYYY-MM-DD.
    pub from: String,
    /// Last day, YYYY-MM-DD (inclusive).
    pub to: String,
    /// 1–200, default 50.
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

pub async fn agent_list(
    app: &App,
    caller: AuthContext,
    p: TimeListParams,
) -> Result<TimeEntryPage> {
    list_for_tool(
        app,
        caller,
        &ListQuery {
            project: p.project,
            issue: p.issue,
            from: Some(p.from),
            to: Some(p.to),
            cursor: p.cursor,
            limit: p.limit,
            ..Default::default()
        },
    )
    .await
}

/// Events of one visible entry (oldest first).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimeEntryEvent {
    pub seq: i64,
    pub at: DateTime<Utc>,
    /// `created`, `updated`, `deleted`, `restored`.
    pub kind: String,
    pub origin: String,
    pub actor: Named,
    pub actor_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Named>,
    /// `{field: {"old", "new"}}`; redacted text shows `[已遮蔽]`.
    pub changes: Json,
}

pub async fn events(app: &App, caller: AuthContext, id: &str) -> Result<Vec<TimeEntryEvent>> {
    let id: Uuid = id.parse().map_err(|_| not_found())?;
    let mut tx = read_tx(app).await?;
    let v = viewer(&mut tx, caller).await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT 1 FROM time_entries te LEFT JOIN issues i ON i.id = te.issue_id WHERE te.id = ",
    );
    qb.push_bind(id).push(" AND");
    push_visible(&mut qb, &v);
    let seen: Option<i32> = qb.build_query_scalar().fetch_optional(&mut *tx).await?;
    seen.ok_or_else(not_found)?;
    type Ev = (
        i64,
        DateTime<Utc>,
        String,
        String,
        Uuid,
        String,
        String,
        Option<Uuid>,
        Option<String>,
        Json,
    );
    let rows: Vec<Ev> = sqlx::query_as(
        "SELECT e.seq, e.at, e.kind, e.origin, e.actor_id, a.name, e.actor_kind, e.on_behalf_of_id, ob.name,
                e.payload -> 'changes'
         FROM time_entry_events e JOIN principals a ON a.id = e.actor_id
         LEFT JOIN principals ob ON ob.id = e.on_behalf_of_id
         WHERE e.time_entry_id = $1 ORDER BY e.seq",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|r| TimeEntryEvent {
            seq: r.0,
            at: r.1,
            kind: r.2,
            origin: r.3,
            actor: Named { id: r.4, name: r.5 },
            actor_kind: r.6,
            on_behalf_of: r.7.zip(r.8).map(|(id, name)| Named { id, name }),
            changes: r.9,
        })
        .collect())
}

// ---------------------------------------------------------------- summary

#[derive(Debug, Clone, Default)]
pub struct SummaryQuery {
    pub project: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub group_by: Option<String>,
    pub user: Option<String>,
    pub issue: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SummaryRow {
    /// The group's id or value (`null`: no issue / no tracker / no version).
    pub key: Option<String>,
    pub label: String,
    pub hours: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub group_by: String,
    pub rows: Vec<SummaryRow>,
    pub total: String,
    /// Always `only_visible`: the totals count only time the viewer may read.
    pub scope_note: String,
}

pub async fn summary(app: &App, caller: AuthContext, q: &SummaryQuery) -> Result<Summary> {
    let mut tx = read_tx(app).await?;
    let v = viewer(&mut tx, caller).await?;
    if v.agent_projects.is_some() {
        // §3.5.4 A1: agents never read totals or reports.
        return Err(crate::time_entries::forbidden(
            "time_read_not_delegated",
            "agents do not read time reports",
        ));
    }
    let from = q
        .from
        .as_deref()
        .ok_or_else(|| invalid_arg("from", "from is required"))?;
    let to =
        q.to.as_deref()
            .ok_or_else(|| invalid_arg("to", "to is required"))?;
    let group_by = q.group_by.clone().unwrap_or_else(|| "user".into());
    if !GROUPS.contains(&group_by.as_str()) {
        return Err(
            ApiError::new(ErrorCode::InvalidArgument, "unknown group_by")
                .with_details(json!({"path": "group_by", "allowed": GROUPS}))
                .into(),
        );
    }
    let f = filters(
        &mut tx,
        caller,
        &v,
        q.project.as_deref(),
        q.issue.as_deref(),
        q.user.as_deref(),
        Some(from),
        Some(to),
    )
    .await?;
    let (from, to) = (f.from.expect("given"), f.to.expect("given"));
    if to < from || (to - from).num_days() + 1 > MAX_REPORT_DAYS {
        return Err(invalid_arg("to", "a report spans 1–366 days (from ≤ to)"));
    }
    // R20: a visible project without view_time_entries for the viewer.
    let any: bool = match f.project {
        Some(p) => visibility(&mut tx, p, v.me).await?.is_some(),
        None => {
            let ps: Vec<Uuid> = v.scope.project_ids().collect();
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM unnest($1::uuid[]) AS p(id)
                                WHERE time_visibility(p.id, $2) IS NOT NULL)",
            )
            .bind(&ps)
            .bind(v.me)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    if !any {
        return Err(crate::time_entries::forbidden(
            "time_entries_not_visible",
            "you may not see time entries here (view_time_entries)",
        ));
    }
    let (key, label, order) = match group_by.as_str() {
        "user" => ("te.user_id::text", "u.name", "hours DESC, label"),
        "activity" => ("te.activity_id::text", "act.name", "hours DESC, label"),
        "issue" => (
            "te.issue_id::text",
            "COALESCE(ik.key || ' ' || i.subject, '')",
            "hours DESC, label",
        ),
        "tracker" => (
            "i.tracker_id::text",
            "COALESCE((SELECT name FROM trackers WHERE id = i.tracker_id), '')",
            "hours DESC, label",
        ),
        "fixed_version" => (
            "i.fixed_version_id::text",
            "COALESCE((SELECT name FROM versions WHERE id = i.fixed_version_id), '')",
            "hours DESC, label",
        ),
        "day" => ("te.spent_on::text", "te.spent_on::text", "key"),
        "week" => (
            "to_char(te.spent_on, 'IYYY-\"W\"IW')",
            "to_char(te.spent_on, 'IYYY-\"W\"IW')",
            "key",
        ),
        _ => (
            "to_char(te.spent_on, 'YYYY-MM')",
            "to_char(te.spent_on, 'YYYY-MM')",
            "key",
        ),
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT key, label, hours FROM (SELECT {key} AS key, {label} AS label,
                (sum(te.hours) * 100)::bigint AS hours"
    ));
    qb.push(FROM).push(" WHERE");
    push_visible(&mut qb, &v);
    push_filters(&mut qb, &f);
    qb.push(format!(" GROUP BY 1, 2) g ORDER BY {order}"));
    let rows: Vec<(Option<String>, String, i64)> = qb.build_query_as().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let total: i64 = rows.iter().map(|r| r.2).sum();
    Ok(Summary {
        from,
        to,
        group_by,
        rows: rows
            .into_iter()
            .map(|(key, label, h)| SummaryRow {
                key,
                label,
                hours: fmt_hours(h),
            })
            .collect(),
        total: fmt_hours(total),
        scope_note: "only_visible".into(),
    })
}

// ---------------------------------------------------------------- timesheet

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimesheetCell {
    pub date: NaiveDate,
    pub hours: Option<String>,
    pub entry_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimesheetRow {
    pub project: ProjectRef,
    pub issue: Option<IssueRef>,
    pub activity: Named,
    pub cells: Vec<TimesheetCell>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Timesheet {
    pub week: String,
    /// Seven days in `week_start` order.
    pub days: Vec<NaiveDate>,
    pub max_hours_per_day: String,
    pub rows: Vec<TimesheetRow>,
    pub day_totals: Vec<String>,
    pub week_total: String,
}

fn parse_week(s: &str) -> Option<(i32, u32)> {
    let (y, w) = s.split_once("-W")?;
    let y: i32 = y.parse().ok()?;
    let w: u32 = w.parse().ok()?;
    NaiveDate::from_isoywd_opt(y, w, Weekday::Mon).map(|_| (y, w))
}

/// `GET /me/timesheet?week=2026-W46` (§3.5.2): only the caller's own time,
/// so `time_entries_visibility` does not apply; issues still must be
/// visible.
pub async fn timesheet(app: &App, caller: AuthContext, week: Option<&str>) -> Result<Timesheet> {
    let mut tx = read_tx(app).await?;
    let v = viewer(&mut tx, caller).await?;
    let (today, week_start, max_cents): (NaiveDate, i16, i64) = sqlx::query_as(
        "SELECT (now() AT TIME ZONE timezone)::date, week_start,
                (timelog_max_hours_per_day * 100)::bigint FROM org_settings",
    )
    .fetch_one(&mut *tx)
    .await?;
    let (y, w) = match week {
        Some(s) => parse_week(s).ok_or_else(|| invalid_arg("week", "week is YYYY-Www (ISO)"))?,
        None => {
            let iw = today.iso_week();
            (iw.year(), iw.week())
        }
    };
    let monday = NaiveDate::from_isoywd_opt(y, w, Weekday::Mon).expect("checked");
    // week_start: 1 = Monday … 7 = Sunday; the ISO week's days, reordered.
    let start = monday + Duration::days(i64::from(week_start) - 1);
    let start = if start > monday + Duration::days(6) {
        start - Duration::days(7)
    } else {
        start
    };
    let mut days: Vec<NaiveDate> = (0..7).map(|i| monday + Duration::days(i)).collect();
    days.sort_by_key(|d| (*d - start).num_days().rem_euclid(7));
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    qb.push(FROM)
        .push(" WHERE te.deleted_at IS NULL AND te.user_id = ")
        .push_bind(v.me)
        .push(" AND te.spent_on BETWEEN ")
        .push_bind(monday)
        .push(" AND ")
        .push_bind(monday + Duration::days(6))
        .push(" AND p.status IN ('active', 'closed') AND te.project_id = ANY(")
        .push_bind(v.scope.project_ids().collect::<Vec<_>>())
        // A project with time tracking turned off shows no time (§3.5.1).
        .push(
            ") AND EXISTS (SELECT 1 FROM enabled_modules em WHERE em.project_id = te.project_id
               AND em.module = 'time_tracking')",
        )
        .push(" AND (te.issue_id IS NULL OR (i.archived_at IS NULL AND ");
    db::push_pred(&mut qb, "i", &v.scope.predicate());
    qb.push(")) ORDER BY p.key, ik.key NULLS FIRST, act.position, te.created_at");
    let rows: Vec<Row> = qb.build_query_as().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    type Line = (String, Option<String>, Uuid);
    type Cells = (Row, Vec<(NaiveDate, i64, Uuid)>);
    let mut grouped: BTreeMap<Line, Cells> = BTreeMap::new();
    for r in rows {
        let k = (r.project_key.clone(), r.issue_key.clone(), r.activity_id);
        let cell = (r.spent_on, r.cents, r.id);
        grouped
            .entry(k)
            .or_insert_with(|| (r, Vec::new()))
            .1
            .push(cell);
    }
    let mut totals = vec![0i64; 7];
    let mut out = Vec::new();
    for (_, (r, cells)) in grouped {
        let mut row_cells = Vec::new();
        for (i, d) in days.iter().enumerate() {
            let here: Vec<&(NaiveDate, i64, Uuid)> = cells.iter().filter(|c| c.0 == *d).collect();
            let sum: i64 = here.iter().map(|c| c.1).sum();
            totals[i] += sum;
            row_cells.push(TimesheetCell {
                date: *d,
                hours: (!here.is_empty()).then(|| fmt_hours(sum)),
                entry_ids: here.iter().map(|c| c.2).collect(),
            });
        }
        let base = r.view();
        out.push(TimesheetRow {
            project: base.project,
            issue: base.issue,
            activity: base.activity,
            cells: row_cells,
        });
    }
    Ok(Timesheet {
        week: format!("{y}-W{w:02}"),
        days,
        max_hours_per_day: fmt_hours(max_cents),
        rows: out,
        week_total: fmt_hours(totals.iter().sum()),
        day_totals: totals.into_iter().map(fmt_hours).collect(),
    })
}

// ---------------------------------------------------------------- activities

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Activity {
    pub id: Uuid,
    pub name: String,
    pub position: i32,
    pub active: bool,
    pub is_default: bool,
    /// Entries use it (it can then only be deactivated).
    pub in_use: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectActivities {
    /// Usable now in the project (site active and not turned off here).
    pub items: Vec<Named>,
    pub default_id: Option<Uuid>,
}

/// `GET /time-activities[?project=]`: with a project, what it may use now;
/// without, the site list (the admin page).
pub async fn activities(app: &App, caller: AuthContext, project: Option<&str>) -> Result<Json> {
    let mut tx = read_tx(app).await?;
    let out = match project {
        Some(p) => {
            let p = describe::visible_project(&mut tx, caller, p).await?;
            let avail = available_activities(&mut tx, p).await?;
            serde_json::to_value(ProjectActivities {
                default_id: avail.iter().find(|a| a.2).map(|a| a.0),
                items: avail
                    .into_iter()
                    .map(|(id, name, _)| Named { id, name })
                    .collect(),
            })?
        }
        None => serde_json::to_value(site_activities(&mut tx).await?)?,
    };
    tx.commit().await?;
    Ok(out)
}

async fn site_activities(conn: &mut PgConnection) -> Result<Vec<Activity>> {
    let rows: Vec<(Uuid, String, i32, bool, bool, bool)> = sqlx::query_as(
        "SELECT a.id, a.name, a.position, a.active, a.is_default,
                EXISTS (SELECT 1 FROM time_entries te WHERE te.activity_id = a.id)
         FROM time_entry_activities a ORDER BY a.position, a.name_key",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Activity {
            id: r.0,
            name: r.1,
            position: r.2,
            active: r.3,
            is_default: r.4,
            in_use: r.5,
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivityInput {
    /// Omitted = a new activity.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    pub position: i32,
    pub active: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivitiesPut {
    pub items: Vec<ActivityInput>,
}

fn invalid_value(path: &str, kind: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

/// `PUT /admin/time-activities` (§3.5.3): the whole list. An activity left
/// out is deleted, unless entries use it (`activity_in_use`: deactivate it
/// instead). At most one default, and it must be active.
pub async fn put_site_activities(
    app: &App,
    caller: AuthContext,
    input: &ActivitiesPut,
) -> Result<Vec<Activity>> {
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    let admin: bool = sqlx::query_scalar("SELECT is_admin FROM principals WHERE id = $1")
        .bind(who.user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !admin {
        return Err(crate::time_entries::forbidden(
            "permission",
            "only administrators change the activities",
        ));
    }
    sqlx::query("LOCK TABLE time_entry_activities IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if input.items.iter().filter(|a| a.is_default).count() > 1 {
        return Err(invalid_value(
            "items",
            "default_count",
            "at most one default activity",
        ));
    }
    if input.items.iter().any(|a| a.is_default && !a.active) {
        return Err(invalid_value(
            "items",
            "default_inactive",
            "the default activity must be active",
        ));
    }
    let mut keys: Vec<String> = Vec::new();
    for (i, a) in input.items.iter().enumerate() {
        let (name, key): (String, String) =
            sqlx::query_as("SELECT normalize(btrim($1), NFC), casefold(normalize(btrim($1), NFC))")
                .bind(&a.name)
                .fetch_one(&mut *tx)
                .await?;
        if name.is_empty() || name.chars().count() > 30 {
            return Err(invalid_value(
                &format!("items[{i}].name"),
                "name_invalid",
                "an activity name is 1–30 characters",
            ));
        }
        if keys.contains(&key) {
            return Err(invalid_value(
                &format!("items[{i}].name"),
                "name_taken",
                "activity names are unique",
            ));
        }
        keys.push(key);
    }
    let kept: Vec<Uuid> = input.items.iter().filter_map(|a| a.id).collect();
    let in_use: Option<String> = sqlx::query_scalar(
        "SELECT a.name FROM time_entry_activities a
         WHERE a.id <> ALL($1) AND EXISTS (SELECT 1 FROM time_entries te WHERE te.activity_id = a.id)
         ORDER BY a.position LIMIT 1",
    )
    .bind(&kept)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(n) = in_use {
        return Err(invalid_value(
            "items",
            "activity_in_use",
            &format!("{n} has time entries; deactivate it instead of removing it"),
        ));
    }
    sqlx::query("DELETE FROM project_time_activities WHERE activity_id <> ALL($1)")
        .bind(&kept)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM time_entry_activities WHERE id <> ALL($1)")
        .bind(&kept)
        .execute(&mut *tx)
        .await?;
    // Clear defaults and keys first so swaps do not trip the unique indexes.
    sqlx::query("UPDATE time_entry_activities SET is_default = false, name_key = id::text")
        .execute(&mut *tx)
        .await?;
    for (a, key) in input.items.iter().zip(&keys) {
        let found = match a.id {
            Some(id) => sqlx::query_scalar::<_, Uuid>(
                "UPDATE time_entry_activities SET name = normalize(btrim($2), NFC), name_key = $3,
                        position = $4, active = $5, is_default = $6
                 WHERE id = $1 RETURNING id",
            )
            .bind(id)
            .bind(&a.name)
            .bind(key)
            .bind(a.position)
            .bind(a.active)
            .bind(a.is_default)
            .fetch_optional(&mut *tx)
            .await?
            .is_some(),
            None => {
                sqlx::query(
                    "INSERT INTO time_entry_activities (name, name_key, position, active, is_default)
                     VALUES (normalize(btrim($1), NFC), $2, $3, $4, $5)",
                )
                .bind(&a.name)
                .bind(key)
                .bind(a.position)
                .bind(a.active)
                .bind(a.is_default)
                .execute(&mut *tx)
                .await?;
                true
            }
        };
        if !found {
            return Err(invalid_value("items", "unknown_id", "no such activity"));
        }
    }
    let out = site_activities(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActivityOverride {
    pub activity_id: Uuid,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectTimeSettings {
    pub project_id: Uuid,
    pub lock_version: i32,
    /// Whether the time tracking module is on.
    pub module_enabled: bool,
    pub overrides: Vec<ActivityOverride>,
    /// The site list, for the settings page.
    pub activities: Vec<Activity>,
    pub can_manage: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectTimePut {
    pub lock_version: i32,
    pub overrides: Vec<ActivityOverride>,
    /// Turns the time tracking module on or off (omitted = unchanged). This
    /// changes the project's security epoch: agent work contexts that read
    /// from the project must be restarted.
    #[serde(default)]
    pub module_enabled: Option<bool>,
}

async fn manage_access(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<bool> {
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project).await?;
    Ok(pp.is_some_and(|pp| {
        pp.status == "active" && !actor.is_agent() && (pp.admin || has(&pp, MANAGE_PROJECT))
    }))
}

async fn project_settings_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<ProjectTimeSettings> {
    let lock_version: i32 =
        sqlx::query_scalar("SELECT lock_version FROM project_time_settings WHERE project_id = $1")
            .bind(project)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(0);
    let overrides: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT activity_id, active FROM project_time_activities WHERE project_id = $1 ORDER BY activity_id",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ProjectTimeSettings {
        project_id: project,
        lock_version,
        module_enabled: module_enabled(&mut *conn, project).await?,
        overrides: overrides
            .into_iter()
            .map(|(activity_id, active)| ActivityOverride {
                activity_id,
                active,
            })
            .collect(),
        activities: site_activities(&mut *conn).await?,
        can_manage: manage_access(&mut *conn, caller, project).await?,
    })
}

pub async fn project_settings(
    app: &App,
    caller: AuthContext,
    project: &str,
) -> Result<ProjectTimeSettings> {
    let mut tx = read_tx(app).await?;
    let p = describe::visible_project(&mut tx, caller, project).await?;
    let out = project_settings_in(&mut tx, caller, p).await?;
    tx.commit().await?;
    Ok(out)
}

/// `PUT /projects/{id}/time-activities` (§3.5.3, `manage_project`).
pub async fn put_project_settings(
    app: &App,
    caller: AuthContext,
    project: &str,
    input: &ProjectTimePut,
) -> Result<ProjectTimeSettings> {
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    let p = describe::visible_project(&mut tx, caller, project).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(p)
        .execute(&mut *tx)
        .await?;
    if !manage_access(&mut tx, caller, p).await? {
        return Err(crate::time_entries::forbidden(
            "permission",
            "changing time settings needs manage_project in an active project",
        ));
    }
    let lv: i32 = sqlx::query_scalar(
        "SELECT lock_version FROM project_time_settings WHERE project_id = $1 FOR UPDATE",
    )
    .bind(p)
    .fetch_one(&mut *tx)
    .await?;
    if lv != input.lock_version {
        let current = project_settings_in(&mut tx, caller, p).await?;
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the time settings changed; here are the current ones",
        )
        .with_details(json!({"current_lock_version": lv, "current": current}))
        .into());
    }
    sqlx::query("DELETE FROM project_time_activities WHERE project_id = $1")
        .bind(p)
        .execute(&mut *tx)
        .await?;
    for (i, o) in input.overrides.iter().enumerate() {
        let r = sqlx::query(
            "INSERT INTO project_time_activities (project_id, activity_id, active)
             SELECT $1, id, $3 FROM time_entry_activities WHERE id = $2
             ON CONFLICT (project_id, activity_id) DO UPDATE SET active = EXCLUDED.active",
        )
        .bind(p)
        .bind(o.activity_id)
        .bind(o.active)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(invalid_value(
                &format!("overrides[{i}].activity_id"),
                "unknown_id",
                "no such activity",
            ));
        }
    }
    match input.module_enabled {
        Some(true) => {
            sqlx::query(
                "INSERT INTO enabled_modules (project_id, module) VALUES ($1, 'time_tracking')
                 ON CONFLICT DO NOTHING",
            )
            .bind(p)
            .execute(&mut *tx)
            .await?;
        }
        Some(false) => {
            sqlx::query(
                "DELETE FROM enabled_modules WHERE project_id = $1 AND module = 'time_tracking'",
            )
            .bind(p)
            .execute(&mut *tx)
            .await?;
        }
        None => {}
    }
    sqlx::query(
        "UPDATE project_time_settings SET lock_version = lock_version + 1, updated_by = $2, updated_at = now()
         WHERE project_id = $1",
    )
    .bind(p)
    .bind(who.user_id)
    .execute(&mut *tx)
    .await?;
    let out = project_settings_in(&mut tx, caller, p).await?;
    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimelogSettings {
    pub lock_version: i32,
    /// `"0"` = no limit.
    pub max_hours_per_day: String,
    pub accept_0_hours: bool,
    pub accept_future_dates: bool,
    pub accept_closed_issues: bool,
    pub require_issue: bool,
    pub require_comments: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelogSettingsPut {
    pub lock_version: i32,
    /// Decimal string 0–24 (`"0"` = no limit).
    pub max_hours_per_day: String,
    pub accept_0_hours: bool,
    pub accept_future_dates: bool,
    pub accept_closed_issues: bool,
    pub require_issue: bool,
    pub require_comments: bool,
}

async fn timelog_in(conn: &mut PgConnection) -> Result<TimelogSettings> {
    let r = crate::time_entries::rules(&mut *conn).await?;
    Ok(TimelogSettings {
        lock_version: r.lock_version,
        max_hours_per_day: fmt_hours(r.max_cents),
        accept_0_hours: r.accept_0_hours,
        accept_future_dates: r.accept_future_dates,
        accept_closed_issues: r.accept_closed_issues,
        require_issue: r.require_issue,
        require_comments: r.require_comments,
    })
}

pub async fn timelog_settings(app: &App, caller: AuthContext) -> Result<TimelogSettings> {
    commands::executor(&caller)?;
    let mut tx = read_tx(app).await?;
    let out = timelog_in(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}

/// `PUT /org/timelog-settings` (administrators).
pub async fn put_timelog_settings(
    app: &App,
    caller: AuthContext,
    input: &TimelogSettingsPut,
) -> Result<TimelogSettings> {
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    let admin: bool = sqlx::query_scalar("SELECT is_admin FROM principals WHERE id = $1")
        .bind(who.user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !admin {
        return Err(crate::time_entries::forbidden(
            "permission",
            "only administrators change the timelog settings",
        ));
    }
    let lv: i32 = sqlx::query_scalar("SELECT timelog_lock_version FROM org_settings FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    if lv != input.lock_version {
        let current = timelog_in(&mut tx).await?;
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the timelog settings changed; here are the current ones",
        )
        .with_details(json!({"current_lock_version": lv, "current": current}))
        .into());
    }
    let cents = crate::time_entries::parse_hours(&json!(input.max_hours_per_day))
        .filter(|c| *c <= 2400)
        .ok_or_else(|| {
            invalid_value(
                "max_hours_per_day",
                "hours_invalid",
                "max_hours_per_day is a decimal string from 0 to 24",
            )
        })?;
    sqlx::query(
        "UPDATE org_settings SET timelog_max_hours_per_day = $1::numeric, timelog_accept_0_hours = $2,
                timelog_accept_future_dates = $3, timelog_accept_closed_issues = $4,
                timelog_require_issue = $5, timelog_require_comments = $6,
                timelog_lock_version = timelog_lock_version + 1",
    )
    .bind(format!("{}.{:02}", cents / 100, cents % 100))
    .bind(input.accept_0_hours)
    .bind(input.accept_future_dates)
    .bind(input.accept_closed_issues)
    .bind(input.require_issue)
    .bind(input.require_comments)
    .execute(&mut *tx)
    .await?;
    let out = timelog_in(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}

/// `GET /projects/{id}/time-users`: whom the caller may log time for here
/// (themself, and with `log_time_for_other_users` the active members who
/// may log time).
pub async fn time_users(app: &App, caller: AuthContext, project: &str) -> Result<Vec<Named>> {
    let mut tx = read_tx(app).await?;
    let p = describe::visible_project(&mut tx, caller, project).await?;
    let actor = perm::actor(&mut tx, caller).await?;
    let pp = perm::project_perms(&mut tx, &actor, p)
        .await?
        .ok_or_else(|| api(ErrorCode::NotFound, "no project"))?;
    let me = actor
        .user_id
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "sign in first"))?;
    let others = has(&pp, perm::LOG_TIME_FOR_OTHER_USERS) && !actor.is_agent();
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT u.id, u.name FROM principals u
         WHERE u.kind = 'user' AND u.status = 'active'
           AND (u.id = $2 OR ($3 AND EXISTS (
                SELECT 1 FROM members m
                JOIN member_roles mr ON mr.member_id = m.id
                JOIN role_permissions rp ON rp.role_id = mr.role_id AND rp.permission = 'log_time'
                WHERE m.project_id = $1
                  AND (m.principal_id = u.id
                       OR m.principal_id IN (SELECT group_id FROM group_members WHERE user_id = u.id)))))
         ORDER BY u.name, u.id",
    )
    .bind(p)
    .bind(me)
    .bind(others)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| Named { id, name })
        .collect())
}
