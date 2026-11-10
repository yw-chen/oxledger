// SPDX-License-Identifier: GPL-2.0-or-later
//! Triage (contract 05 §2.4, §3.4, §3.10.1): per-project settings, the
//! inbox, recent decisions and counts. Decisions are command ops
//! (`triage_decide`, in [`crate::commands`]); this module only reads, and
//! writes the settings.

use crate::auth::AuthContext;
use crate::citations::Reader;
use crate::commands;
use crate::cursor::{self, Position};
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, MANAGE_PROJECT, TRIAGE_ISSUES, VIEW_ISSUES};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

/// The pseudo field key of the triage state in `issue_field_origins` and
/// `execution_field_changes` (undo, §3.10.1).
pub const FIELD_KEY: &str = "_triage";

/// `issue_triage` (alias `t`) as one jsonb value: what undo compares and
/// restores.
pub const SNAPSHOT_SQL: &str = "jsonb_build_object(
    'state', t.state, 'snoozed_until', t.snoozed_until, 'snoozed_at', t.snoozed_at,
    'snoozed_by', t.snoozed_by, 'decided_at', t.decided_at, 'decided_by', t.decided_by,
    'self_triage', t.self_triage, 'same_sponsor', t.same_sponsor)";

/// The settings PUT's idempotency action.
pub const ACTION_SETTINGS: &str = "triage_settings_update";
/// `entry_rule` values (§2.4).
pub const ENTRY_RULES: [&str; 2] = ["creator_without_triage", "all_new"];
/// "Recent decisions" look back this far (§3.4).
pub const DECIDED_DAYS: i32 = 30;
pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 100;
/// Counts stop here and say `more` (§3.4: "99+").
pub const COUNT_CAP: i64 = 99;

/// The state shown (alias `tr` on `issue_triage`, `i` on `issues`):
/// `snoozed` while the snooze holds (§2.4: until `snoozed_until` or the
/// issue changes).
const STATE_SQL: &str = "CASE WHEN tr.state = 'pending' AND tr.snoozed_until > now()
                               AND i.updated_at <= tr.snoozed_at
                          THEN 'snoozed' ELSE tr.state END";
const SNOOZED_SQL: &str = "(tr.snoozed_until > now() AND i.updated_at <= tr.snoozed_at)";

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such project")
}

fn invalid(kind: &str, path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"kind": kind, "path": path}))
        .into()
}

// ---------------------------------------------------------------- issue state

/// `GET /issues/{key}`'s `triage` (§3.4).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct IssueTriage {
    /// `pending`, `snoozed`, `accepted`, `declined` or `duplicate`.
    pub state: String,
    /// While snoozed.
    pub snoozed_until: Option<DateTime<Utc>>,
    /// The caller may send a `triage_decide` for it now (`triage_issues`;
    /// for an agent also the delegation's `triage_decide`).
    pub can_decide: bool,
}

pub(crate) async fn issue_state(
    conn: &mut PgConnection,
    issue: Uuid,
    can_triage: bool,
) -> Result<Option<IssueTriage>> {
    let row: Option<(String, Option<DateTime<Utc>>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {STATE_SQL}, CASE WHEN {SNOOZED_SQL} THEN tr.snoozed_until END
             FROM issue_triage tr JOIN issues i ON i.id = tr.issue_id WHERE tr.issue_id = $1"
        )))
        .bind(issue)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.map(|(state, snoozed_until)| IssueTriage {
        can_decide: can_triage && matches!(state.as_str(), "pending" | "snoozed"),
        state,
        snoozed_until,
    }))
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, sqlx::FromRow)]
pub struct Named {
    pub id: Uuid,
    pub name: String,
}

/// A role that has `triage_issues` but no workflow transition from a
/// tracker's initial status to the decline status (warn only, §3.4).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageWarning {
    /// `decline_transition_missing`.
    pub kind: String,
    pub message: String,
    pub roles: Vec<Named>,
}

/// `GET /projects/{id}/triage-settings` and the PUT's result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageSettings {
    pub project_id: Uuid,
    pub enabled: bool,
    /// `creator_without_triage` (default) or `all_new`.
    pub entry_rule: String,
    /// The person on duty (display only).
    pub responsible: Option<Named>,
    /// The closed status decline and duplicate preset.
    pub decline_status: Option<Named>,
    pub lock_version: i32,
    /// `manage_project` in an active project.
    pub can_manage: bool,
    /// PUT with `enabled: true` only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<TriageWarning>,
}

/// `PUT /projects/{id}/triage-settings` (header Idempotency-Key).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriageSettingsPut {
    pub lock_version: i32,
    pub enabled: bool,
    /// `creator_without_triage` or `all_new`.
    pub entry_rule: String,
    /// An active user the project's issues may be assigned to; null clears.
    #[serde(default)]
    pub responsible_user_id: Option<Uuid>,
    /// A status of category done; required when `enabled`.
    #[serde(default)]
    pub decline_status_id: Option<Uuid>,
}

struct Access {
    can_manage: bool,
    pp: perm::ProjectPerms,
    actor: perm::Actor,
}

async fn access(conn: &mut PgConnection, caller: AuthContext, project: Uuid) -> Result<Access> {
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(not_found)?;
    if pp.status == "archived" || !pp.allowed_any(VIEW_ISSUES) {
        return Err(not_found());
    }
    Ok(Access {
        can_manage: pp.status == "active" && pp.allowed_any(MANAGE_PROJECT) && !actor.is_agent(),
        pp,
        actor,
    })
}

impl Access {
    /// `triage_issues` here, writable, and (agents) `triage_decide` delegated.
    fn can_decide(&self, project: Uuid) -> bool {
        self.pp.writable()
            && self.pp.allowed_any(TRIAGE_ISSUES)
            && self
                .actor
                .delegation_allows(project, perm::OP_TRIAGE_DECIDE, None)
    }
}

type SettingsRow = (
    bool,
    String,
    Option<Uuid>,
    Option<String>,
    Option<Uuid>,
    Option<String>,
    i32,
);

async fn settings_row(
    conn: &mut PgConnection,
    project: Uuid,
    lock: bool,
) -> Result<TriageSettings> {
    let r: SettingsRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.enabled, s.entry_rule, s.responsible_user_id, u.name, s.decline_status_id, st.name,
                s.lock_version
         FROM project_triage_settings s
         LEFT JOIN principals u ON u.id = s.responsible_user_id
         LEFT JOIN issue_statuses st ON st.id = s.decline_status_id
         WHERE s.project_id = $1{}",
        if lock { " FOR UPDATE OF s" } else { "" }
    )))
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    Ok(TriageSettings {
        project_id: project,
        enabled: r.0,
        entry_rule: r.1,
        responsible: r.2.zip(r.3).map(|(id, name)| Named { id, name }),
        decline_status: r.4.zip(r.5).map(|(id, name)| Named { id, name }),
        lock_version: r.6,
        can_manage: false,
        warnings: Vec::new(),
    })
}

fn snapshot(s: &TriageSettings) -> Json {
    json!({"enabled": s.enabled, "entry_rule": s.entry_rule,
           "responsible_user_id": s.responsible.as_ref().map(|n| n.id),
           "decline_status_id": s.decline_status.as_ref().map(|n| n.id),
           "lock_version": s.lock_version})
}

/// `GET /projects/{id}/triage-settings`: readable by whoever sees the project.
pub async fn settings(app: &App, caller: AuthContext, project: &str) -> Result<TriageSettings> {
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let mut s = settings_row(&mut tx, project, false).await?;
    s.can_manage = a.can_manage;
    if s.can_manage {
        s.warnings = warnings(&mut tx, project, &s).await?;
    }
    tx.commit().await?;
    Ok(s)
}

/// What the settings page warns about (none of it blocks saving):
/// - `responsible_invalid`: the person on duty can no longer be assigned here;
/// - `decline_status_invalid`: the decline status is no longer a done status;
/// - `decline_transition_missing`: roles with `triage_issues` and no workflow
///   transition from a tracker's initial status to the decline status.
async fn warnings(
    conn: &mut PgConnection,
    project: Uuid,
    s: &TriageSettings,
) -> Result<Vec<TriageWarning>> {
    let mut out = Vec::new();
    if let Some(r) = &s.responsible {
        let ok = describe::assignable(&mut *conn, project, None)
            .await?
            .iter()
            .any(|p| p.id == r.id && p.kind == "user");
        if !ok {
            out.push(TriageWarning {
                kind: "responsible_invalid".into(),
                message: format!("負責人「{}」已經不能在這個專案被指派，請改選", r.name),
                roles: Vec::new(),
            });
        }
    }
    let Some(st) = &s.decline_status else {
        return Ok(out);
    };
    let done: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM issue_statuses WHERE id = $1 AND category = 'done')",
    )
    .bind(st.id)
    .fetch_one(&mut *conn)
    .await?;
    if !done {
        out.push(TriageWarning {
            kind: "decline_status_invalid".into(),
            message: format!("拒絕狀態「{}」已經不是已完成類的狀態，請改選", st.name),
            roles: Vec::new(),
        });
    }
    if !s.enabled {
        return Ok(out);
    }
    let roles: Vec<Named> = sqlx::query_as(
        "SELECT DISTINCT r.id, r.name FROM roles r
         JOIN member_roles mr ON mr.role_id = r.id
         JOIN members m ON m.id = mr.member_id AND m.project_id = $1
         JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = 'triage_issues'
         WHERE EXISTS (
             SELECT 1 FROM project_trackers pt JOIN trackers t ON t.id = pt.tracker_id
             WHERE pt.project_id = $1 AND t.default_status_id IS DISTINCT FROM $2
               AND NOT EXISTS (SELECT 1 FROM workflow_transitions w
                               WHERE w.tracker_id = t.id AND w.role_id = r.id
                                 AND w.old_status_id = t.default_status_id
                                 AND w.new_status_id = $2))
         ORDER BY r.name",
    )
    .bind(project)
    .bind(st.id)
    .fetch_all(&mut *conn)
    .await?;
    if !roles.is_empty() {
        out.push(TriageWarning {
            kind: "decline_transition_missing".into(),
            message: format!(
                "這些角色有 triage_issues，但 workflow 沒有從初始狀態到「{}」的轉移：{}",
                st.name,
                roles
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            ),
            roles,
        });
    }
    Ok(out)
}

/// `PUT /projects/{id}/triage-settings` (§3.4): operation request →
/// executor → project `FOR SHARE` → settings `FOR UPDATE` → lock_version →
/// checks → UPDATE → `project_setting_events`.
pub async fn put_settings(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &TriageSettingsPut,
) -> Result<TriageSettings> {
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let h: [u8; 32] = Sha256::digest(
        json!({"project": project, "input": serde_json::to_value(input)?})
            .to_string()
            .as_bytes(),
    )
    .into();
    let fp = commands::fingerprint(ACTION_SETTINGS, &h);
    let Some(request_id) =
        commands::open_request(&mut tx, who.user_id, operation_id, ACTION_SETTINGS, &fp).await?
    else {
        // The same key and body: the response of that write, as it was
        // (its lock_version and warnings), not the settings now.
        let stored =
            commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_SETTINGS, &fp)
                .await?;
        tx.commit().await?;
        return Ok(serde_json::from_value(stored["settings"].clone())?);
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access(&mut tx, caller, project).await?;
    if !a.can_manage {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "changing the Triage settings needs manage_project in an active project",
        )
        .with_details(json!({"reason": "permission"}))
        .into());
    }
    let old = settings_row(&mut tx, project, true).await?;
    if old.lock_version != input.lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the Triage settings changed; here are the current ones",
        )
        .with_details(json!({
            "current_lock_version": old.lock_version,
            "current": snapshot(&old),
        }))
        .into());
    }
    if !ENTRY_RULES.contains(&input.entry_rule.as_str()) {
        return Err(invalid(
            "triage_entry_rule",
            "entry_rule",
            "entry_rule is creator_without_triage or all_new",
        ));
    }
    // Only what this request changes is checked: a value that went stale
    // since (a person no longer assignable, a status edited by hand) does
    // not block saving the rest, e.g. turning Triage off; it is a warning.
    let old_decline = old.decline_status.as_ref().map(|n| n.id);
    let old_responsible = old.responsible.as_ref().map(|n| n.id);
    if let Some(st) = input
        .decline_status_id
        .filter(|st| Some(*st) != old_decline)
    {
        let done: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM issue_statuses WHERE id = $1 AND category = 'done')",
        )
        .bind(st)
        .fetch_one(&mut *tx)
        .await?;
        if !done {
            return Err(invalid(
                "triage_decline_status_invalid",
                "decline_status_id",
                "the decline status must be a closed (done) status",
            ));
        }
    } else if input.enabled && input.decline_status_id.is_none() {
        return Err(invalid(
            "triage_decline_status_required",
            "decline_status_id",
            "turning Triage on needs a decline status",
        ));
    }
    if let Some(u) = input
        .responsible_user_id
        .filter(|u| Some(*u) != old_responsible)
    {
        let ok = describe::assignable(&mut tx, project, None)
            .await?
            .iter()
            .any(|p| p.id == u && p.kind == "user");
        if !ok {
            return Err(invalid(
                "triage_responsible_invalid",
                "responsible_user_id",
                "the person on duty must be an active user issues here may be assigned to",
            ));
        }
    }
    sqlx::query(
        "UPDATE project_triage_settings
         SET enabled = $2, entry_rule = $3, responsible_user_id = $4, decline_status_id = $5,
             lock_version = lock_version + 1, updated_by = $6, updated_at = now()
         WHERE project_id = $1",
    )
    .bind(project)
    .bind(input.enabled)
    .bind(&input.entry_rule)
    .bind(input.responsible_user_id)
    .bind(input.decline_status_id)
    .bind(who.user_id)
    .execute(&mut *tx)
    .await?;
    let mut new = settings_row(&mut tx, project, false).await?;
    sqlx::query(
        "INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
         VALUES ($1, 'triage_settings_updated', $2, $3, $4)",
    )
    .bind(project)
    .bind(who.user_id)
    .bind(request_id)
    .bind(json!({"before": snapshot(&old), "after": snapshot(&new)}))
    .execute(&mut *tx)
    .await?;
    new.can_manage = true;
    new.warnings = warnings(&mut tx, project, &new).await?;
    commands::complete_request(
        &mut tx,
        request_id,
        &json!({"project_id": project, "lock_version": new.lock_version,
                "settings": serde_json::to_value(&new)?}),
    )
    .await?;
    tx.commit().await?;
    Ok(new)
}

// ---------------------------------------------------------------- inbox

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageProject {
    pub id: Uuid,
    pub key: String,
    pub name: String,
}

/// An issue's author or a decider; `on_behalf_of` for agents.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PrincipalRef {
    pub id: Uuid,
    pub name: String,
    /// `user` or `agent`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Named>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageItem {
    pub key: String,
    pub id: Uuid,
    pub lock_version: i32,
    pub subject: String,
    pub tracker: Named,
    pub priority: Option<Named>,
    pub author: PrincipalRef,
    pub created_at: DateTime<Utc>,
    pub entered_at: DateTime<Utc>,
    /// While snoozed (only listed with `include_snoozed`).
    pub snoozed_until: Option<DateTime<Utc>>,
    /// The latest activity the caller can see.
    pub last_activity_at: DateTime<Utc>,
    /// `GET /triage` (every project) only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<TriageProject>,
    /// `view=decided`: `accepted`, `declined` or `duplicate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<PrincipalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
    /// A5: an agent decided an issue it created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_triage: Option<bool>,
    /// A5: an agent decided another agent's issue, both for the same person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_sponsor: Option<bool>,
}

/// The project's settings as the inbox shows them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct InboxSettings {
    pub enabled: bool,
    pub responsible: Option<Named>,
    pub decline_status: Option<Named>,
}

/// `GET /projects/{id}/triage`, `GET /triage` and MCP `triage_list`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageList {
    /// Project inbox only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<InboxSettings>,
    /// Project inbox only: the caller may decide here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_decide: Option<bool>,
    pub items: Vec<TriageItem>,
    pub next_cursor: Option<String>,
}

/// What a list asks for.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub include_snoozed: bool,
    /// `view=decided`.
    pub decided: bool,
    /// `only`, `exclude` or `all`.
    pub self_triage: Option<String>,
    /// `agent`, `user` or `all`.
    pub decided_by_kind: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    key: String,
    lock_version: i32,
    subject: String,
    tracker_id: Uuid,
    tracker_name: String,
    priority_id: Option<Uuid>,
    priority_name: Option<String>,
    author_id: Uuid,
    author_name: String,
    author_kind: String,
    behalf_id: Option<Uuid>,
    behalf_name: Option<String>,
    created_at: DateTime<Utc>,
    entered_at: DateTime<Utc>,
    snoozed_until: Option<DateTime<Utc>>,
    project_id: Uuid,
    project_key: String,
    project_name: String,
    state: String,
    decided_at: Option<DateTime<Utc>>,
    decider_id: Option<Uuid>,
    decider_name: Option<String>,
    decider_kind: Option<String>,
    decider_behalf_id: Option<Uuid>,
    decider_behalf_name: Option<String>,
    self_triage: bool,
    same_sponsor: bool,
}

fn bad_arg(msg: &str) -> anyhow::Error {
    api(ErrorCode::InvalidArgument, msg)
}

/// The page of `projects` (all already allowed for the caller).
async fn page(
    pool: &sqlx::PgPool,
    conn: &mut PgConnection,
    caller: AuthContext,
    projects: &[Uuid],
    q: &ListQuery,
    with_project: bool,
) -> Result<(Vec<TriageItem>, Option<String>)> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
    let self_filter = q.self_triage.as_deref().unwrap_or("all");
    let kind_filter = q.decided_by_kind.as_deref().unwrap_or("all");
    if !matches!(self_filter, "only" | "exclude" | "all") {
        return Err(bad_arg("self_triage is only, exclude or all"));
    }
    if !matches!(kind_filter, "agent" | "user" | "all") {
        return Err(bad_arg("decided_by_kind is agent, user or all"));
    }
    let mut projects = projects.to_vec();
    projects.sort();
    let hash = cursor::query_hash(&json!({
        "q": "triage", "projects": projects, "decided": q.decided,
        "include_snoozed": q.include_snoozed, "self": self_filter, "kind": kind_filter,
        "limit": limit,
    }));
    let principal = caller.principal_id();
    let after = match q.cursor.as_deref().filter(|c| !c.is_empty()) {
        None => None,
        Some(c) => match cursor::decode(pool, c, principal, &hash).await {
            Ok(Position::Keyset { t, i }) => Some((t, i)),
            Ok(_) => return Err(cursor::mismatch()),
            Err(e) => return Err(e),
        },
    };
    let scope = db::load_scope(&mut *conn, caller).await?;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT i.id, k.key, i.lock_version, i.subject, i.tracker_id, t.name AS tracker_name,
                i.priority_id, pr.name AS priority_name,
                i.author_id, au.name AS author_name, au.kind AS author_kind,
                CASE WHEN au.kind = 'agent' THEN i.created_on_behalf_of_id END AS behalf_id,
                CASE WHEN au.kind = 'agent' THEN ob.name END AS behalf_name,
                i.created_at, tr.entered_at,
                CASE WHEN {SNOOZED_SQL} THEN tr.snoozed_until END AS snoozed_until,
                i.project_id, p.key AS project_key, p.name AS project_name,
                tr.state, tr.decided_at,
                tr.decided_by AS decider_id, dp.name AS decider_name, dp.kind AS decider_kind,
                ap.delegator_user_id AS decider_behalf_id, dob.name AS decider_behalf_name,
                tr.self_triage, tr.same_sponsor
         FROM issue_triage tr
         JOIN issues i ON i.id = tr.issue_id AND i.archived_at IS NULL
         JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         JOIN projects p ON p.id = i.project_id
         JOIN trackers t ON t.id = i.tracker_id
         LEFT JOIN issue_priorities pr ON pr.id = i.priority_id
         JOIN principals au ON au.id = i.author_id
         LEFT JOIN principals ob ON ob.id = i.created_on_behalf_of_id
         LEFT JOIN principals dp ON dp.id = tr.decided_by
         LEFT JOIN agent_principals ap ON ap.agent_principal_id = tr.decided_by
         LEFT JOIN principals dob ON dob.id = ap.delegator_user_id
         WHERE tr.project_id = ANY("
    ));
    qb.push_bind(projects.clone()).push(") AND ");
    db::push_pred(&mut qb, "i", &scope.predicate());
    if q.decided {
        qb.push(format!(
            " AND tr.state <> 'pending' AND tr.decided_at >= now() - make_interval(days => {DECIDED_DAYS})"
        ));
        match self_filter {
            "only" => {
                qb.push(" AND (tr.self_triage OR tr.same_sponsor)");
            }
            "exclude" => {
                qb.push(" AND NOT (tr.self_triage OR tr.same_sponsor)");
            }
            _ => {}
        }
        match kind_filter {
            "agent" => {
                qb.push(" AND dp.kind = 'agent'");
            }
            "user" => {
                qb.push(" AND dp.kind = 'user'");
            }
            _ => {}
        }
        if let Some((t, i)) = after {
            qb.push(" AND (tr.decided_at, i.id) < (")
                .push_bind(t)
                .push(", ")
                .push_bind(i)
                .push(")");
        }
        qb.push(" ORDER BY tr.decided_at DESC, i.id DESC");
    } else {
        // §2.4: a project with Triage off shows nothing (its pending rows stay).
        qb.push(
            " AND tr.state = 'pending'
              AND EXISTS (SELECT 1 FROM project_triage_settings s
                          WHERE s.project_id = tr.project_id AND s.enabled)",
        );
        if !q.include_snoozed {
            qb.push(format!(" AND NOT COALESCE({SNOOZED_SQL}, false)"));
        }
        if let Some((t, i)) = after {
            qb.push(" AND (tr.entered_at, i.id) > (")
                .push_bind(t)
                .push(", ")
                .push_bind(i)
                .push(")");
        }
        qb.push(" ORDER BY tr.entered_at, i.id");
    }
    qb.push(" LIMIT ").push_bind(limit + 1);
    let mut rows: Vec<Row> = qb.build_query_as().fetch_all(&mut *conn).await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next = match (more, rows.last()) {
        (true, Some(r)) => {
            let t = if q.decided {
                r.decided_at.unwrap_or(r.entered_at)
            } else {
                r.entered_at
            };
            Some(cursor::encode(pool, principal, &hash, Position::Keyset { t, i: r.id }).await?)
        }
        _ => None,
    };
    let mut reader = Reader::new(&mut *conn, caller).await?;
    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let last = crate::issue_view::last_activity_at(
            &mut *conn,
            &mut reader,
            caller,
            (r.id, r.project_id, r.tracker_id),
            r.created_at,
        )
        .await?;
        let decided = q.decided;
        items.push(TriageItem {
            key: r.key,
            id: r.id,
            lock_version: r.lock_version,
            subject: r.subject,
            tracker: Named {
                id: r.tracker_id,
                name: r.tracker_name,
            },
            priority: r
                .priority_id
                .zip(r.priority_name)
                .map(|(id, name)| Named { id, name }),
            author: PrincipalRef {
                id: r.author_id,
                name: r.author_name,
                kind: r.author_kind,
                on_behalf_of: r
                    .behalf_id
                    .zip(r.behalf_name)
                    .map(|(id, name)| Named { id, name }),
            },
            created_at: r.created_at,
            entered_at: r.entered_at,
            snoozed_until: r.snoozed_until,
            last_activity_at: last,
            project: with_project.then_some(TriageProject {
                id: r.project_id,
                key: r.project_key,
                name: r.project_name,
            }),
            decision: decided.then(|| r.state.clone()),
            decided_by: if decided {
                r.decider_id
                    .zip(r.decider_name)
                    .map(|(id, name)| PrincipalRef {
                        id,
                        name,
                        kind: r.decider_kind.clone().unwrap_or_default(),
                        on_behalf_of: r
                            .decider_behalf_id
                            .zip(r.decider_behalf_name.clone())
                            .map(|(id, name)| Named { id, name }),
                    })
            } else {
                None
            },
            decided_at: if decided { r.decided_at } else { None },
            self_triage: decided.then_some(r.self_triage),
            same_sponsor: decided.then_some(r.same_sponsor),
        });
    }
    Ok((items, next))
}

/// `GET /projects/{id}/triage` (§3.4): readable by whoever sees the
/// project; only issues the caller can see.
pub async fn project_inbox(
    app: &App,
    caller: AuthContext,
    project: &str,
    q: &ListQuery,
) -> Result<TriageList> {
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let s = settings_row(&mut tx, project, false).await?;
    let (items, next_cursor) = page(&app.pool, &mut tx, caller, &[project], q, false).await?;
    tx.commit().await?;
    Ok(TriageList {
        settings: Some(InboxSettings {
            enabled: s.enabled,
            responsible: s.responsible,
            decline_status: s.decline_status,
        }),
        can_decide: Some(a.can_decide(project)),
        items,
        next_cursor,
    })
}

/// Visible projects where the caller has `triage_issues` (agents: and the
/// delegation's `triage_decide`), and Triage is on.
async fn triage_projects(conn: &mut PgConnection, caller: AuthContext) -> Result<Vec<Uuid>> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let visible: Vec<Uuid> = scope.project_ids().collect();
    let enabled: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM project_triage_settings
         WHERE enabled AND project_id = ANY($1) ORDER BY project_id",
    )
    .bind(&visible)
    .fetch_all(&mut *conn)
    .await?;
    let actor = perm::actor(&mut *conn, caller).await?;
    let mut out = Vec::new();
    for p in enabled {
        let Some(pp) = perm::project_perms(&mut *conn, &actor, p).await? else {
            continue;
        };
        if pp.status != "archived"
            && pp.allowed_any(TRIAGE_ISSUES)
            && actor.delegation_allows(p, perm::OP_TRIAGE_DECIDE, None)
        {
            out.push(p);
        }
    }
    Ok(out)
}

/// `GET /triage` (§3.4): every project where the caller triages, merged.
pub async fn my_inbox(app: &App, caller: AuthContext, q: &ListQuery) -> Result<TriageList> {
    if q.decided {
        return Err(bad_arg(
            "view=decided is per project: /projects/{id}/triage",
        ));
    }
    let mut tx = read_tx(app).await?;
    let projects = triage_projects(&mut tx, caller).await?;
    let (items, next_cursor) = page(&app.pool, &mut tx, caller, &projects, q, true).await?;
    tx.commit().await?;
    Ok(TriageList {
        settings: None,
        can_decide: None,
        items,
        next_cursor,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageCount {
    pub project: TriageProject,
    /// Pending issues the caller can see (not snoozed), at most 99.
    pub count: i64,
    /// More than 99 ("99+").
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TriageCounts {
    pub projects: Vec<TriageCount>,
}

/// `GET /triage/counts` (§3.4, R28): per project of `GET /triage`.
pub async fn counts(app: &App, caller: AuthContext) -> Result<TriageCounts> {
    let mut tx = read_tx(app).await?;
    let projects = triage_projects(&mut tx, caller).await?;
    let scope = db::load_scope(&mut tx, caller).await?;
    let mut out = Vec::new();
    for p in projects {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
            "SELECT count(*) FROM (SELECT 1 FROM issue_triage tr
               JOIN issues i ON i.id = tr.issue_id AND i.archived_at IS NULL
               WHERE tr.state = 'pending' AND NOT COALESCE({SNOOZED_SQL}, false) AND tr.project_id = "
        ));
        qb.push_bind(p).push(" AND ");
        db::push_pred(&mut qb, "i", &scope.predicate());
        qb.push(" LIMIT ").push_bind(COUNT_CAP + 1).push(") c");
        let n: i64 = qb.build_query_scalar().fetch_one(&mut *tx).await?;
        let (key, name): (String, String) =
            sqlx::query_as("SELECT key, name FROM projects WHERE id = $1")
                .bind(p)
                .fetch_one(&mut *tx)
                .await?;
        out.push(TriageCount {
            project: TriageProject { id: p, key, name },
            count: n.min(COUNT_CAP),
            more: n > COUNT_CAP,
        });
    }
    out.sort_by(|a, b| a.project.key.cmp(&b.project.key));
    tx.commit().await?;
    Ok(TriageCounts { projects: out })
}

/// MCP `triage_list` (§4.1).
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriageListParams {
    /// Project key or id; without it, every delegated project where you may triage.
    #[serde(default)]
    pub project: Option<String>,
    /// Also list snoozed issues (default false).
    #[serde(default)]
    pub include_snoozed: Option<bool>,
    /// 1–100, default 50.
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

pub async fn agent_list(app: &App, caller: AuthContext, p: TriageListParams) -> Result<TriageList> {
    let q = ListQuery {
        include_snoozed: p.include_snoozed.unwrap_or(false),
        cursor: p.cursor,
        limit: p.limit,
        ..Default::default()
    };
    match p
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(project) => project_inbox(app, caller, project, &q).await,
        None => my_inbox(app, caller, &q).await,
    }
}
