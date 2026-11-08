// SPDX-License-Identifier: GPL-2.0-or-later
//! describe (contract 02 §5) and the assignee function (§6.5).
//!
//! Two questions, neither an authorization ticket (every write re-checks):
//!
//! - static: which fields an issue of this project × tracker has, and which
//!   ops the caller could use on them (`GET /projects/{id}/trackers/{tid}/schema`);
//! - dynamic: what the caller can do to this issue right now
//!   (`GET /issues/{key}/actions`).
//!
//! Restricted custom fields the caller cannot read never appear (no name, no
//! type, no "N hidden fields"). For agents the answer is the delegator's
//! permissions ∩ the delegation, and anything outside the delegation is left
//! out rather than listed as `false` (§5.2).

use crate::auth::AuthContext;
use crate::db;
use crate::error::{ErrorCode, api};
use crate::perm::{self, Actor, ProjectPerms};
use crate::service::{App, read_tx, user_err};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use ox_core::workflow::{
    Baseline, Condition, FieldRule, HiddenField, Rule, Transition, allowed_statuses, field_rules,
};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use std::collections::BTreeMap;
use uuid::Uuid;

pub const COMMAND_SCHEMA_VERSION: u16 = 1;
pub const MAX_OPS: i32 = 10;
pub const MAX_NOTE_BYTES: i32 = 65_536;
pub const MAX_DESCRIPTION_BYTES: i32 = 262_144;
/// What the first slice cannot do, so agents do not find out by trying.
pub const UNSUPPORTED: &[&str] = &[
    "multi_issue_changeset",
    "per_item",
    "delete_issue",
    "move_issue_project",
    "relative_date",
    "version_delete",
];

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OptionView {
    pub id: Uuid,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldSchema {
    /// `subject`, `assigned_to_id`, … or `cf:<id>` for custom fields.
    pub key: String,
    /// Display name (custom fields).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// string | text | date | principal_ref | priority_ref | status_ref | version_ref, or a
    /// custom field type (int, float, date, string, text, list, user, bool, link).
    #[serde(rename = "type")]
    pub field_type: String,
    /// The field definition itself is required (workflow rules are per status:
    /// see the issue actions).
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Json>,
    /// Where the valid values come from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_source: Option<String>,
    /// List fields: every option, inactive ones marked (they can be kept but
    /// not newly chosen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<OptionView>>,
    /// create | update | transition: what the caller could do with the field.
    pub ops: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PrivateChoice {
    /// Used when `is_private` is not given (the tracker's default).
    pub default: bool,
    /// Values the caller may give explicitly.
    pub allowed_values: Vec<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Creation {
    pub allowed: bool,
    pub initial_status_id: Option<Uuid>,
    pub is_private: PrivateChoice,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Limits {
    pub max_ops: i32,
    pub max_note_bytes: i32,
    pub max_description_bytes: i32,
}

/// Static schema of an issue in a project × tracker (§5.1).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TrackerSchema {
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    /// Changes when field definitions change: a hint, not a ticket.
    pub schema_version: i64,
    pub workflow_version: i64,
    pub command_schema_version: u16,
    pub observed_at: DateTime<Utc>,
    pub fields: Vec<FieldSchema>,
    pub creation: Creation,
    pub limits: Limits,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldAction {
    pub key: String,
    /// Can be changed (for agents: can be changed in a proposal).
    pub editable: bool,
    /// Required in the current status (definition or workflow rule).
    pub required: bool,
    /// Why not editable: project_not_writable | not_editable | workflow_readonly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TransitionAction {
    pub to_status_id: Uuid,
    pub name: String,
    /// Readable fields required after moving to this status.
    pub then_required: Vec<String>,
    /// Readable fields read-only after moving to this status.
    pub then_readonly: Vec<String>,
    /// A condition the caller cannot see blocks this transition.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NoteActions {
    pub can_add: bool,
    pub can_set_private: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DuplicateActions {
    pub can_propose: bool,
}

/// What the caller can do to one issue now (§5.2). There is deliberately no
/// `can_apply`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IssueActions {
    pub issue_id: Uuid,
    pub key: String,
    pub lock_version: i32,
    pub schema_version: i64,
    pub workflow_version: i64,
    pub observed_at: DateTime<Utc>,
    pub fields: Vec<FieldAction>,
    pub transitions: Vec<TransitionAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<NoteActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<DuplicateActions>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct Assignee {
    pub id: Uuid,
    pub name: String,
    /// user | group
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AssigneeList {
    pub project_id: Uuid,
    pub assignees: Vec<Assignee>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DescribeScope {
    ProjectTracker,
    IssueActions,
    /// Agents only: which projects the work context reads and where it may
    /// write (contract 02 §5.3).
    Context,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    /// project_tracker: fields of an issue type; issue_actions: what you can do to one issue now.
    pub scope: DescribeScope,
    /// project_tracker: project id or key (e.g. "AUTH").
    pub project: Option<String>,
    /// project_tracker: tracker id or name (e.g. "Bug").
    pub tracker: Option<String>,
    /// issue_actions: issue key (e.g. "AUTH-12").
    pub issue_key: Option<String>,
}

/// REST path of the static schema.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectTrackerPath {
    pub id: String,
    pub tracker_id: String,
}

/// REST path of an issue's actions.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IssueKeyPath {
    pub key: String,
}

/// REST path of a project's assignees.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectPath {
    pub id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DescribeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_tracker: Option<TrackerSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_actions: Option<IssueActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<crate::agents::ContextDescription>,
}

// ---------------------------------------------------------------- shared pieces

/// Built-in fields: (key, type, definition-required, constraints, options source).
type Builtin = (
    &'static str,
    &'static str,
    bool,
    Option<Json>,
    Option<String>,
);

fn builtin_fields(project: Uuid) -> Vec<Builtin> {
    vec![
        (
            "subject",
            "string",
            true,
            Some(json!({"min_length": 1, "max_length": 255})),
            None,
        ),
        (
            "description",
            "text",
            false,
            Some(json!({"max_bytes": MAX_DESCRIPTION_BYTES})),
            None,
        ),
        (
            "assigned_to_id",
            "principal_ref",
            false,
            None,
            Some(format!("/api/v1/projects/{project}/assignees")),
        ),
        // Options inline (issue_priorities); a new issue gets the default.
        ("priority_id", "priority_ref", false, None, None),
        (
            "start_date",
            "date",
            false,
            Some(json!({"min": "1900-01-01", "max": "2199-12-31"})),
            None,
        ),
        (
            "due_date",
            "date",
            false,
            Some(json!({"min": "1900-01-01", "max": "2199-12-31", "not_before": "start_date"})),
            None,
        ),
        (
            "fixed_version_id",
            "version_ref",
            false,
            None,
            Some(format!("/api/v1/projects/{project}/versions?status=open")),
        ),
    ]
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct CfDef {
    pub id: i32,
    pub name: String,
    pub ftype: String,
    pub is_required: bool,
    pub visible: bool,
    pub roles: Vec<Uuid>,
}

impl CfDef {
    pub fn key(&self) -> String {
        format!("cf:{}", self.id)
    }
}

/// Custom fields that apply to `tracker` in `project` (D14), display order.
pub(crate) async fn applicable_fields(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
) -> Result<Vec<CfDef>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.name, d.ftype, d.is_required, d.visible,
                ARRAY(SELECT fr.role_id FROM field_roles fr WHERE fr.field_id = d.id ORDER BY fr.role_id) AS roles
         FROM field_defs d
         JOIN field_trackers ft ON ft.field_id = d.id AND ft.tracker_id = $1
         WHERE d.index_state <> 'dropping'
           AND (d.is_for_all OR EXISTS (SELECT 1 FROM field_projects fp
                                        WHERE fp.field_id = d.id AND fp.project_id = $2))
         ORDER BY d.position, d.name",
    )
    .bind(tracker)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?)
}

/// Same rule as `get` and filtering: visible fields for everyone; restricted
/// ones for admins and holders of a listed role. Agents: visible only (§3.5).
pub(crate) fn readable(cf: &CfDef, actor: &Actor, pp: &ProjectPerms) -> bool {
    if cf.visible {
        return true;
    }
    if actor.is_agent() {
        return false;
    }
    pp.admin || pp.roles.iter().any(|r| cf.roles.contains(&r.id))
}

async fn versions(conn: &mut PgConnection) -> Result<(i64, i64)> {
    Ok(
        sqlx::query_as("SELECT schema_version, workflow_version FROM org_settings")
            .fetch_one(&mut *conn)
            .await?,
    )
}

fn not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no {what}"))
}

/// A project the caller can see, by id or key. Unknown and invisible look
/// the same.
pub(crate) async fn visible_project(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
) -> Result<Uuid> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let text = project.trim();
    let id: Option<Uuid> = match Uuid::parse_str(text) {
        Ok(id) => Some(id),
        Err(_) if text.len() <= 10 && text.chars().all(|c| c.is_ascii_alphanumeric()) => {
            sqlx::query_scalar("SELECT id FROM projects WHERE key = $1")
                .bind(text.to_uppercase())
                .fetch_optional(&mut *conn)
                .await?
        }
        Err(_) => None,
    };
    match id {
        Some(id) if scope.project_ids().any(|p| p == id) => Ok(id),
        _ => Err(not_found("project")),
    }
}

async fn load_perms(conn: &mut PgConnection, actor: &Actor, project: Uuid) -> Result<ProjectPerms> {
    perm::project_perms(&mut *conn, actor, project)
        .await?
        .ok_or_else(|| not_found("project"))
}

// ---------------------------------------------------------------- static

pub async fn tracker_schema(
    app: &App,
    caller: AuthContext,
    project: &str,
    tracker: &str,
) -> Result<TrackerSchema> {
    let mut tx = read_tx(app).await?;
    let r = tracker_schema_in(&mut tx, caller, project, tracker).await?;
    tx.commit().await?;
    Ok(r)
}

async fn tracker_schema_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
    tracker: &str,
) -> Result<TrackerSchema> {
    let project_id = visible_project(&mut *conn, caller, project).await?;
    let text = tracker.trim();
    // An id matches exactly; a name case-insensitively, which may match
    // several trackers ("Bug" and "BUG" can coexist): then ask (§5.1).
    let rows: Vec<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT t.id, t.default_status_id, t.private_by_default
         FROM trackers t JOIN project_trackers pt ON pt.tracker_id = t.id AND pt.project_id = $1
         WHERE t.id::text = $2 OR lower(t.name) = lower($2)
         ORDER BY t.id::text = $2 DESC, t.id",
    )
    .bind(project_id)
    .bind(text)
    .fetch_all(&mut *conn)
    .await?;
    let by_id = rows.first().filter(|r| r.0.to_string() == text);
    let (tracker_id, initial_status_id, private_by_default) = match (by_id, rows.as_slice()) {
        (Some(r), _) | (None, [r]) => *r,
        (None, []) => return Err(not_found("tracker in this project")),
        (None, _) => {
            return Err(api(
                ErrorCode::AmbiguousReference,
                format!("tracker name {text:?} matches several trackers; use the id"),
            ));
        }
    };
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = load_perms(&mut *conn, &actor, project_id).await?;
    let (schema_version, workflow_version) = versions(&mut *conn).await?;

    let can_create = pp.writable() && pp.allowed(perm::ADD_ISSUES, tracker_id);
    let can_update = pp.writable()
        && (pp.allowed(perm::EDIT_ISSUES, tracker_id)
            || pp.allowed(perm::EDIT_OWN_ISSUES, tracker_id));
    let ops_for = |key: &str| -> Vec<String> {
        let mut ops = Vec::new();
        if can_create && actor.delegation_allows(project_id, perm::OP_CREATE_ISSUE, Some(key)) {
            ops.push("create".to_string());
        }
        if can_update && actor.delegation_allows(project_id, perm::OP_UPDATE_FIELDS, Some(key)) {
            ops.push("update".to_string());
        }
        ops
    };

    let priorities: Vec<(Uuid, String, bool)> =
        sqlx::query_as("SELECT id, name, active FROM issue_priorities ORDER BY position, name")
            .fetch_all(&mut *conn)
            .await?;
    let mut fields: Vec<FieldSchema> = Vec::new();
    for (key, ty, required, constraints, options_source) in builtin_fields(project_id) {
        fields.push(FieldSchema {
            key: key.into(),
            name: None,
            field_type: ty.into(),
            required,
            constraints,
            options_source,
            options: (key == "priority_id").then(|| {
                priorities
                    .iter()
                    .map(|(id, label, active)| OptionView {
                        id: *id,
                        label: label.clone(),
                        active: *active,
                    })
                    .collect()
            }),
            ops: ops_for(key),
        });
        if key == "assigned_to_id" {
            let ops =
                if can_update && actor.delegation_allows(project_id, perm::OP_TRANSITION, None) {
                    vec!["transition".to_string()]
                } else {
                    Vec::new()
                };
            fields.push(FieldSchema {
                key: "status_id".into(),
                name: None,
                field_type: "status_ref".into(),
                required: false,
                constraints: None,
                options_source: Some("/api/v1/issues/{key}/actions".to_string()),
                options: None,
                ops,
            });
        }
    }
    let cfs = applicable_fields(&mut *conn, project_id, tracker_id).await?;
    let readable_cfs: Vec<&CfDef> = cfs.iter().filter(|c| readable(c, &actor, &pp)).collect();
    let list_ids: Vec<i32> = readable_cfs
        .iter()
        .filter(|c| c.ftype == "list")
        .map(|c| c.id)
        .collect();
    let options: Vec<(i32, Uuid, String, bool)> = sqlx::query_as(
        "SELECT field_id, id, label, active FROM field_options
         WHERE field_id = ANY($1) ORDER BY field_id, position, label",
    )
    .bind(&list_ids)
    .fetch_all(&mut *conn)
    .await?;
    for cf in readable_cfs {
        let key = cf.key();
        fields.push(FieldSchema {
            ops: ops_for(&key),
            key,
            name: Some(cf.name.clone()),
            field_type: cf.ftype.clone(),
            required: cf.is_required,
            constraints: None,
            options_source: (cf.ftype == "user")
                .then(|| format!("/api/v1/projects/{project_id}/assignees")),
            options: (cf.ftype == "list").then(|| {
                options
                    .iter()
                    .filter(|o| o.0 == cf.id)
                    .map(|o| OptionView {
                        id: o.1,
                        label: o.2.clone(),
                        active: o.3,
                    })
                    .collect()
            }),
        });
    }

    let mut allowed_values = vec![false];
    if pp.allowed(perm::SET_ISSUES_PRIVATE, tracker_id)
        || pp.allowed(perm::SET_OWN_ISSUES_PRIVATE, tracker_id)
    {
        // The creator is the author, so set_own_issues_private suffices.
        allowed_values.push(true);
    }
    let max_ops = match actor.delegation(project_id) {
        perm::Reach::Delegated(d) => d.max_ops.min(MAX_OPS),
        _ => MAX_OPS,
    };
    Ok(TrackerSchema {
        project_id,
        tracker_id,
        schema_version,
        workflow_version,
        command_schema_version: COMMAND_SCHEMA_VERSION,
        observed_at: Utc::now(),
        fields,
        creation: Creation {
            allowed: can_create && actor.delegation_allows(project_id, perm::OP_CREATE_ISSUE, None),
            initial_status_id,
            is_private: PrivateChoice {
                default: private_by_default,
                allowed_values,
            },
        },
        limits: Limits {
            max_ops,
            max_note_bytes: MAX_NOTE_BYTES,
            max_description_bytes: MAX_DESCRIPTION_BYTES,
        },
        unsupported: UNSUPPORTED.iter().map(|s| s.to_string()).collect(),
    })
}

// ---------------------------------------------------------------- dynamic

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct IssueCore {
    pub id: Uuid,
    pub key: String,
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    pub status_id: Uuid,
    pub author_id: Uuid,
    pub created_on_behalf_of_id: Option<Uuid>,
    pub assigned_to_id: Option<Uuid>,
    pub is_private: bool,
    pub lock_version: i32,
}

impl IssueCore {
    /// [`perm::is_own`] for this issue.
    pub fn is_own(&self, actor: &Actor) -> bool {
        perm::is_own(actor.user_id, self.author_id, self.created_on_behalf_of_id)
    }
}

/// PROJECT-123; anything else cannot exist.
pub(crate) fn well_formed_key(key: &str) -> bool {
    key.len() <= 32
        && key.split_once('-').is_some_and(|(p, n)| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_alphanumeric())
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
        })
}

/// The issue behind `key` if the caller can see it. Missing and invisible
/// give the same `not_found`, and the scope is loaded first either way so a
/// missing key is not measurably faster.
pub(crate) async fn visible_issue(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
) -> Result<IssueCore> {
    let key = key.trim().to_uppercase();
    let nf = || api(ErrorCode::NotFound, format!("no issue {key}"));
    let scope = db::load_scope(&mut *conn, caller).await?;
    if !well_formed_key(&key) {
        return Err(nf());
    }
    let row: Option<IssueCore> = sqlx::query_as(
        "SELECT i.id, ck.key, i.project_id, i.tracker_id, i.status_id, i.author_id,
                i.created_on_behalf_of_id, i.assigned_to_id, i.is_private, i.lock_version
         FROM issue_keys rk
         JOIN issues i ON i.id = rk.issue_id
         JOIN issue_keys ck ON ck.issue_id = i.id AND ck.is_current
         WHERE rk.key = $1 AND i.archived_at IS NULL",
    )
    .bind(&key)
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or_else(nf)?;
    let facts = IssueFacts {
        project_id: row.project_id,
        tracker_id: row.tracker_id,
        is_private: row.is_private,
        author_id: row.author_id,
        assigned_to_id: row.assigned_to_id,
    };
    if !scope.can_see(&facts) {
        return Err(nf());
    }
    Ok(row)
}

fn baseline(actor: &Actor, issue: &IssueCore) -> Baseline {
    Baseline {
        is_author: actor.user_id == Some(issue.author_id),
        is_assignee: issue
            .assigned_to_id
            .is_some_and(|a| actor.principal_ids.contains(&a)),
    }
}

/// Statuses the actor may move `issue` to, current first, with names. Empty
/// transitions (only the current status) when attributes are not editable or
/// the delegation does not include `transition`.
pub(crate) async fn allowed_transitions(
    conn: &mut PgConnection,
    actor: &Actor,
    pp: &ProjectPerms,
    issue: &IssueCore,
) -> Result<Vec<(Uuid, String)>> {
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM issue_statuses ORDER BY position, name")
            .fetch_all(&mut *conn)
            .await?;
    let editable = pp.attributes_editable(issue.tracker_id, issue.is_own(actor))
        && actor.delegation_allows(issue.project_id, perm::OP_TRANSITION, None);
    let roles = pp.workflow_roles();
    let ids = if editable && !roles.is_empty() {
        let rows: Vec<(Uuid, Uuid, Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT tracker_id, role_id, old_status_id, new_status_id, condition
             FROM workflow_transitions
             WHERE tracker_id = $1 AND old_status_id = $2 AND role_id = ANY($3)",
        )
        .bind(issue.tracker_id)
        .bind(issue.status_id)
        .bind(&roles)
        .fetch_all(&mut *conn)
        .await?;
        let transitions: Vec<Transition> = rows
            .into_iter()
            .filter_map(|(tracker_id, role_id, old_status_id, new_status_id, c)| {
                Some(Transition {
                    tracker_id,
                    role_id,
                    old_status_id,
                    new_status_id,
                    condition: Condition::parse(&c)?,
                })
            })
            .collect();
        let mut order = vec![issue.status_id];
        order.extend(
            statuses
                .iter()
                .map(|s| s.0)
                .filter(|s| *s != issue.status_id),
        );
        allowed_statuses(
            &transitions,
            &roles,
            issue.tracker_id,
            issue.status_id,
            baseline(actor, issue),
            &order,
        )
    } else {
        vec![issue.status_id]
    };
    Ok(ids
        .into_iter()
        .filter_map(|id| statuses.iter().find(|s| s.0 == id).cloned())
        .collect())
}

/// Workflow field rules for (tracker, status) and the actor's workflow roles.
pub(crate) async fn rules_by_status(
    conn: &mut PgConnection,
    tracker: Uuid,
    roles: &[Uuid],
) -> Result<BTreeMap<Uuid, Vec<FieldRule>>> {
    let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT old_status_id, role_id, field_key, rule FROM workflow_field_rules
         WHERE tracker_id = $1 AND role_id = ANY($2)",
    )
    .bind(tracker)
    .bind(roles)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: BTreeMap<Uuid, Vec<FieldRule>> = BTreeMap::new();
    for (status, role_id, field_key, rule) in rows {
        if let Some(rule) = Rule::parse(&rule) {
            out.entry(status).or_default().push(FieldRule {
                role_id,
                field_key,
                rule,
            });
        }
    }
    Ok(out)
}

pub async fn issue_actions(app: &App, caller: AuthContext, key: &str) -> Result<IssueActions> {
    let mut tx = read_tx(app).await?;
    let r = issue_actions_in(&mut tx, caller, key).await?;
    tx.commit().await?;
    Ok(r)
}

pub(crate) async fn issue_actions_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
) -> Result<IssueActions> {
    let issue = visible_issue(&mut *conn, caller, key).await?;
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = load_perms(&mut *conn, &actor, issue.project_id).await?;
    let (schema_version, workflow_version) = versions(&mut *conn).await?;
    let p = issue.project_id;
    let t = issue.tracker_id;
    let attr_editable = pp.attributes_editable(t, issue.is_own(&actor));
    let roles = pp.workflow_roles();
    let rules = rules_by_status(&mut *conn, t, &roles).await?;
    let cfs = applicable_fields(&mut *conn, p, t).await?;
    let hidden: Vec<HiddenField> = cfs
        .iter()
        .filter(|c| !c.visible)
        .map(|c| HiddenField {
            field_key: c.key(),
            roles: c.roles.clone(),
        })
        .collect();
    let rules_at = |status: Uuid| {
        field_rules(
            rules.get(&status).map(Vec::as_slice).unwrap_or(&[]),
            &roles,
            &hidden,
        )
    };
    let current = rules_at(issue.status_id);

    // (key, definition-required, readable)
    let mut keys: Vec<(String, bool, bool)> = builtin_fields(p)
        .into_iter()
        .map(|(k, _, req, _, _)| (k.to_string(), req, true))
        .collect();
    keys.extend(
        cfs.iter()
            .map(|c| (c.key(), c.is_required, readable(c, &actor, &pp))),
    );

    let mut fields = Vec::new();
    if actor.delegation_allows(p, perm::OP_UPDATE_FIELDS, None) {
        for (key, def_required, can_read) in &keys {
            if !can_read || !actor.delegation_allows(p, perm::OP_UPDATE_FIELDS, Some(key)) {
                continue;
            }
            let rule = current.get(key).copied();
            let editable = attr_editable && rule != Some(Rule::Readonly);
            let reason = if editable {
                None
            } else if !pp.writable() {
                Some("project_not_writable")
            } else if !attr_editable {
                Some("not_editable")
            } else {
                Some("workflow_readonly")
            };
            fields.push(FieldAction {
                key: key.clone(),
                editable,
                required: *def_required || rule == Some(Rule::Required),
                reason: reason.map(String::from),
            });
        }
    }

    let mut transitions = Vec::new();
    for (to, name) in allowed_transitions(&mut *conn, &actor, &pp, &issue).await? {
        if to == issue.status_id {
            continue;
        }
        let after = rules_at(to);
        let mut then_required = Vec::new();
        let mut then_readonly = Vec::new();
        let mut blocked = false;
        for (key, _, can_read) in &keys {
            match (after.get(key), can_read) {
                (Some(Rule::Required), true) => then_required.push(key.clone()),
                (Some(Rule::Readonly), true) => then_readonly.push(key.clone()),
                (Some(Rule::Required), false) => blocked = true,
                _ => {}
            }
        }
        transitions.push(TransitionAction {
            to_status_id: to,
            name,
            then_required,
            then_readonly,
            blocked,
        });
    }

    let notes = actor
        .delegation_allows(p, perm::OP_ADD_NOTE, None)
        .then(|| {
            let can_add = pp.writable() && pp.allowed(perm::ADD_ISSUE_NOTES, t);
            NoteActions {
                can_add,
                can_set_private: can_add && pp.allowed(perm::SET_NOTES_PRIVATE, t),
            }
        });
    let duplicate_of = actor
        .delegation_allows(p, perm::OP_DUPLICATE_OF, None)
        .then(|| DuplicateActions {
            can_propose: attr_editable && pp.allowed(perm::MANAGE_ISSUE_RELATIONS, t),
        });

    Ok(IssueActions {
        issue_id: issue.id,
        key: issue.key,
        lock_version: issue.lock_version,
        schema_version,
        workflow_version,
        observed_at: Utc::now(),
        fields,
        transitions,
        notes,
        duplicate_of,
    })
}

// ---------------------------------------------------------------- assignees (§6.5)

/// Who an issue in `project` may be assigned to: active users holding an
/// `assignable` role there (directly or through a group, as Redmine's
/// inherited group memberships do), active groups with such a role when
/// `org_settings.issue_group_assignment` is on, and the issue's author when
/// active (Redmine `Issue#assignable_users`). Never agents.
pub async fn assignable(
    conn: &mut PgConnection,
    project: Uuid,
    author: Option<Uuid>,
) -> Result<Vec<Assignee>> {
    Ok(sqlx::query_as(
        "WITH assignable_members AS (
             SELECT m.principal_id FROM members m
             JOIN member_roles mr ON mr.member_id = m.id
             JOIN roles r ON r.id = mr.role_id AND r.assignable
             WHERE m.project_id = $1
         )
         SELECT p.id, p.name, p.kind FROM principals p
         WHERE p.status = 'active' AND (
             (p.kind = 'user' AND (
                 p.id IN (SELECT principal_id FROM assignable_members)
                 OR EXISTS (SELECT 1 FROM group_members gm
                            JOIN principals g ON g.id = gm.group_id AND g.status = 'active'
                            WHERE gm.user_id = p.id
                              AND gm.group_id IN (SELECT principal_id FROM assignable_members))
                 OR p.id = $2))
             OR (p.kind = 'group'
                 AND (SELECT issue_group_assignment FROM org_settings)
                 AND p.id IN (SELECT principal_id FROM assignable_members)))
         ORDER BY p.kind DESC, p.name, p.id",
    )
    .bind(project)
    .bind(author)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn assignees(app: &App, caller: AuthContext, project: &str) -> Result<AssigneeList> {
    let mut tx = read_tx(app).await?;
    let project_id = visible_project(&mut tx, caller, project).await?;
    let assignees = assignable(&mut tx, project_id, None).await?;
    tx.commit().await?;
    Ok(AssigneeList {
        project_id,
        assignees,
    })
}

// ---------------------------------------------------------------- MCP

pub async fn describe(app: &App, caller: AuthContext, p: DescribeParams) -> Result<DescribeResult> {
    match p.scope {
        DescribeScope::ProjectTracker => {
            let (Some(project), Some(tracker), None) = (p.project, p.tracker, &p.issue_key) else {
                return Err(user_err(
                    "scope project_tracker takes `project` and `tracker` (and no `issue_key`)",
                ));
            };
            Ok(DescribeResult {
                project_tracker: Some(tracker_schema(app, caller, &project, &tracker).await?),
                issue_actions: None,
                context: None,
            })
        }
        DescribeScope::IssueActions => {
            let (None, None, Some(key)) = (&p.project, &p.tracker, p.issue_key) else {
                return Err(user_err(
                    "scope issue_actions takes `issue_key` (and no `project` or `tracker`)",
                ));
            };
            Ok(DescribeResult {
                project_tracker: None,
                issue_actions: Some(issue_actions(app, caller, &key).await?),
                context: None,
            })
        }
        DescribeScope::Context => {
            let (None, None, None) = (&p.project, &p.tracker, &p.issue_key) else {
                return Err(user_err(
                    "scope context takes no `project`, `tracker` or `issue_key`",
                ));
            };
            Ok(DescribeResult {
                project_tracker: None,
                issue_actions: None,
                context: Some(crate::agents::describe_context(app, caller).await?),
            })
        }
    }
}
