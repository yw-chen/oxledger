// SPDX-License-Identifier: GPL-2.0-or-later
//! Undo of agent writes, incident recovery, the agent activity page and the
//! home summary card (contract 02 §16.5, §16.7, §16.10; S6b.4).
//!
//! Who: only the delegator of the agent that wrote (B6), or the person who
//! ran an undo (to undo the undo). Each field is restored only when it still
//! holds what that execution wrote and that execution is still its origin,
//! the person may change it now and the old value is still valid (§16.7
//! L2331). Locks (L2368): the reverted execution's source projects ∪ the
//! target issues' projects FOR SHARE in UUID order (only to order against
//! the redaction transaction, B5: epochs are not compared), then the issues
//! FOR UPDATE, and only then the undo data is read. The undo is a new
//! `human_direct` execution with `reverts_execution_id`, its source snapshot
//! is the reverted one's (transitive), and it keeps before/after values so
//! it can itself be undone.

// Row tuples straight from sqlx.
#![allow(clippy::type_complexity)]

use crate::auth::{AuthContext, PrincipalKind};
use crate::commands;
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::service::App;
use crate::time_entries::UndoTimeEntry;
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

/// §16.7 (U18): undo data is kept 30 days.
pub const UNDO_DAYS: i64 = 30;
pub const ACTION_UNDO: &str = "undo";

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoField {
    pub issue_id: Uuid,
    pub key: String,
    pub field: String,
    pub before: Json,
    pub after: Json,
    pub current: Json,
    pub restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `version_not_open` (§17.13 L2889).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subreason: Option<String>,
    /// reason `rules_changed` (contract 05 §3.6.3): the error code the new
    /// form rules give (`required_field_missing`, `invalid_value`,
    /// `readonly_field`) and the rule, when its fields are readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoNote {
    pub issue_id: Uuid,
    pub key: String,
    pub note_id: Uuid,
    pub restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoRelation {
    pub relation_id: Uuid,
    pub source_key: String,
    pub target_key: String,
    pub restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct StatusOption {
    pub to_status_id: Uuid,
    pub name: String,
}

/// Not done in a batch: a person picks (§16.7: "轉回 X", "關閉為誤建").
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoChoice {
    pub issue_id: Uuid,
    pub key: String,
    /// `transition_back` or `close_as_mistaken`.
    pub kind: String,
    pub options: Vec<StatusOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoVersionField {
    pub version_id: Uuid,
    pub name: String,
    pub field: String,
    pub before: Json,
    pub after: Json,
    pub current: Json,
    pub restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A triage decision this execution made (contract 05 §3.10.1): undo puts
/// the issue back to its state before (pending; the A5 flags cleared).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoTriage {
    pub issue_id: Uuid,
    pub key: String,
    /// The triage state before the decision (`pending`).
    pub before: String,
    /// The state the decision wrote.
    pub after: String,
    /// The state now.
    pub current: String,
    pub restorable: bool,
    /// `expired`, `redacted`, `changed_since` (a later triage write) or
    /// `permission` (no `triage_issues` now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The undo of `create_version`: a soft delete (§17.13).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoVersionCreate {
    pub version_id: Uuid,
    pub name: String,
    pub restorable: bool,
    /// `changed_since` or `in_use` (B7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoPreview {
    pub execution_id: Uuid,
    /// `command` or `changeset`.
    pub execution_kind: String,
    pub created_at: DateTime<Utc>,
    pub expired: bool,
    pub fields: Vec<UndoField>,
    pub notes: Vec<UndoNote>,
    pub relations: Vec<UndoRelation>,
    pub choices: Vec<UndoChoice>,
    pub version_fields: Vec<UndoVersionField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_created: Option<UndoVersionCreate>,
    /// Triage decisions (contract 05 §3.10.1).
    #[serde(default)]
    pub triage: Vec<UndoTriage>,
    /// Time entries (contract 05 §3.10.2, T-M1-41).
    #[serde(default)]
    pub time_entries: Vec<UndoTimeEntry>,
}

impl UndoPreview {
    fn automatic(&self) -> bool {
        self.fields.iter().any(|f| f.restorable)
            || self.triage.iter().any(|t| t.restorable)
            || self.time_entries.iter().any(|t| t.restorable)
            || self.notes.iter().any(|n| n.restorable)
            || self.relations.iter().any(|r| r.restorable)
            || self.version_fields.iter().any(|f| f.restorable)
            || self.version_created.as_ref().is_some_and(|c| c.restorable)
    }

    fn reasons(&self) -> Json {
        let mut v: Vec<Json> = Vec::new();
        for f in &self.fields {
            if let Some(r) = &f.reason {
                let mut item =
                    json!({"key": f.key, "field": f.field, "reason": r, "subreason": f.subreason});
                if let Some(c) = &f.code {
                    item["code"] = json!(c);
                    item["rule_id"] = json!(f.rule_id);
                }
                v.push(item);
            }
        }
        for t in &self.triage {
            if let Some(r) = &t.reason {
                v.push(json!({"key": t.key, "field": "triage", "reason": r}));
            }
        }
        for t in &self.time_entries {
            if let Some(r) = &t.reason {
                v.push(json!({"time_entry_id": t.time_entry_id, "field": t.field, "reason": r}));
            }
        }
        for n in &self.notes {
            if let Some(r) = &n.reason {
                v.push(json!({"key": n.key, "note_id": n.note_id, "reason": r}));
            }
        }
        for r in &self.relations {
            if let Some(x) = &r.reason {
                v.push(json!({"relation_id": r.relation_id, "reason": x}));
            }
        }
        for f in &self.version_fields {
            if let Some(r) = &f.reason {
                v.push(json!({"version_id": f.version_id, "field": f.field, "reason": r}));
            }
        }
        if let Some(c) = &self.version_created
            && let Some(r) = &c.reason
        {
            v.push(json!({"version_id": c.version_id, "reason": r}));
        }
        Json::Array(v)
    }
}

/// What the person confirmed (§16.7 flow: preview → confirm).
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UndoInput {
    /// `"<issue_id>/<field>"` to leave as they are.
    #[serde(default)]
    pub skip_fields: Vec<String>,
    /// Note ids not to retract.
    #[serde(default)]
    pub skip_notes: Vec<Uuid>,
    /// Relation ids not to remove.
    #[serde(default)]
    pub skip_relations: Vec<Uuid>,
    /// Chosen transitions per issue (from `choices`).
    #[serde(default)]
    pub transitions: BTreeMap<Uuid, Uuid>,
    /// Issue ids whose triage decision stays (contract 05 §3.10.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_triage: Vec<Uuid>,
    /// `"<time_entry_id>/<field>"` (field `created` for a new entry) to
    /// leave as they are (contract 05 §3.10.2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_time_entries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct UndoResult {
    pub execution_id: Uuid,
    pub reverts_execution_id: Uuid,
    pub fields_restored: usize,
    pub notes_retracted: usize,
    pub relations_removed: usize,
    pub transitions: usize,
    /// Triage decisions put back (contract 05 §3.10.1).
    #[serde(default)]
    pub triage_restored: usize,
    /// Time entries deleted (created by the execution) or put back
    /// (contract 05 §3.10.2).
    #[serde(default)]
    pub time_entries_restored: usize,
    #[serde(default)]
    pub version_fields_restored: usize,
    #[serde(default)]
    pub version_deleted: bool,
    pub consistent_after: String,
}

fn not_applicable(reasons: Json) -> anyhow::Error {
    ApiError::new(
        ErrorCode::UndoNotApplicable,
        "nothing in this execution can be undone now",
    )
    .with_details(json!({"reasons": reasons}))
    .into()
}

fn person(caller: &AuthContext) -> Result<Uuid> {
    if caller.kind() != PrincipalKind::User {
        return Err(api(
            ErrorCode::HumanSessionRequired,
            "undo is done by a person in the web app",
        ));
    }
    caller
        .principal_id()
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "sign in"))
}

// ---------------------------------------------------------------- the execution

struct ExecInfo {
    kind: String,
    created_at: DateTime<Utc>,
    /// Who may undo it (B6).
    owner: Uuid,
    sources: Vec<(Uuid, i64)>,
}

async fn exec_info(conn: &mut PgConnection, id: Uuid) -> Result<Option<ExecInfo>> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM execution_refs WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(kind) = kind else { return Ok(None) };
    if kind == "command" {
        let r: Option<(String, Uuid, Option<Uuid>, Option<Uuid>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT origin, actor_id, on_behalf_of_id, reverts_execution_id, created_at
             FROM command_executions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((origin, actor, behalf, reverts, at)) = r else {
            return Ok(None);
        };
        let owner = match (origin.as_str(), behalf, reverts) {
            ("agent_direct", Some(b), _) => b,
            ("human_direct", _, Some(_)) => actor,
            _ => return Ok(None),
        };
        let sources = sqlx::query_as(
            "SELECT project_id, security_epoch FROM command_execution_source_projects
             WHERE execution_id = $1 ORDER BY project_id",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        return Ok(Some(ExecInfo {
            kind,
            created_at: at,
            owner,
            sources,
        }));
    }
    let r: Option<(Uuid, i32, Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT e.changeset_id, e.version, c.on_behalf_of_id, e.status, e.created_at
         FROM changeset_executions e JOIN changesets c ON c.id = e.changeset_id WHERE e.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((cs, version, owner, status, at)) = r else {
        return Ok(None);
    };
    if status != "succeeded" {
        return Ok(None);
    }
    let sources = sqlx::query_as(
        "SELECT project_id, security_epoch FROM changeset_version_source_projects
         WHERE changeset_id = $1 AND version = $2 ORDER BY project_id",
    )
    .bind(cs)
    .bind(version)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(ExecInfo {
        kind,
        created_at: at,
        owner,
        sources,
    }))
}

/// Issues this execution touched (from its immutable undo rows and events).
async fn touched_issues(conn: &mut PgConnection, id: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT issue_id FROM execution_field_changes WHERE execution_id = $1
         UNION SELECT issue_id FROM issue_events
               WHERE (command_execution_id = $1 OR execution_id = $1)
                 AND kind IN ('note_added', 'created', 'duplicate_added', 'status_changed',
                              'triage_changed')
         ORDER BY 1",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Visible keys of `ids` for `caller` (missing and invisible look the same).
async fn visible(
    conn: &mut PgConnection,
    caller: AuthContext,
    ids: &[Uuid],
) -> Result<BTreeMap<Uuid, String>> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    type Row = (Uuid, String, Uuid, Uuid, bool, Uuid, Option<Uuid>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = ANY($1) AND i.archived_at IS NULL",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            scope.can_see(&IssueFacts {
                project_id: r.2,
                tracker_id: r.3,
                is_private: r.4,
                author_id: r.5,
                assigned_to_id: r.6,
            })
        })
        .map(|r| (r.0, r.1))
        .collect())
}

/// §17.13 L2889: restoring a version that is no longer open.
fn subreason_of(e: &anyhow::Error) -> Option<&'static str> {
    match crate::error::classify(e).map(|a| a.code) {
        Some(ErrorCode::VersionNotOpen) => Some("version_not_open"),
        _ => None,
    }
}

/// Contract 05 §3.6.3: the error came from a form rule (L1).
fn form_rule_error(e: &anyhow::Error) -> Option<Json> {
    let a = crate::error::classify(e)?;
    let d = a.details.as_ref()?;
    let by_rule = d["reason"] == "form_rule" || d["kind"] == "rule_disallowed";
    by_rule.then(|| d.get("rule_id").cloned().unwrap_or(Json::Null))
}

fn reason_of(e: &anyhow::Error) -> &'static str {
    if form_rule_error(e).is_some() {
        return "rules_changed";
    }
    match crate::error::classify(e).map(|a| a.code) {
        Some(ErrorCode::ForbiddenOp | ErrorCode::ReadonlyField | ErrorCode::NotFound) => {
            "permission"
        }
        _ => "value_no_longer_valid",
    }
}

fn one_field_doc(issue: Uuid, lv: i32, field: &str, value: &Json) -> Json {
    json!({"command_schema_version": 1,
           "ops": [{"op_id": Uuid::now_v7(), "kind": "update_fields",
                    "target": {"type": "issue", "id": issue}, "set": {field: value}}],
           "preconditions": [{"target": {"type": "issue", "id": issue}, "lock_version": lv}]})
}

async fn transitions_to(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
    want: impl Fn(&str, Uuid) -> bool,
) -> Result<Vec<StatusOption>> {
    let Ok(a) = crate::describe::issue_actions_in(&mut *conn, caller, key).await else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for t in a.transitions.iter().filter(|t| !t.blocked) {
        let cat: String = sqlx::query_scalar("SELECT category FROM issue_statuses WHERE id = $1")
            .bind(t.to_status_id)
            .fetch_one(&mut *conn)
            .await?;
        if want(&cat, t.to_status_id) {
            out.push(StatusOption {
                to_status_id: t.to_status_id,
                name: t.name.clone(),
            });
        }
    }
    Ok(out)
}

async fn field_origin(conn: &mut PgConnection, issue: Uuid, field: &str) -> Result<Option<Uuid>> {
    let o: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT origin_execution_id FROM issue_field_origins WHERE issue_id = $1 AND field_key = $2",
    )
    .bind(issue)
    .bind(field)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(o.flatten())
}

/// The preview as `caller` (the four conditions per field), read where it
/// is called: in a read transaction, or under apply's locks.
async fn build_preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    info: &ExecInfo,
) -> Result<UndoPreview> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let expired = now - info.created_at > chrono::Duration::days(UNDO_DAYS);
    let issues = touched_issues(&mut *conn, id).await?;
    let keys = visible(&mut *conn, caller, &issues).await?;
    let mut current: BTreeMap<Uuid, (i32, Uuid, BTreeMap<String, Json>)> = BTreeMap::new();
    for i in keys.keys() {
        if let Some(c) = commands::current_values(&mut *conn, *i).await? {
            current.insert(*i, c);
        }
    }
    // Fields (status is a choice, not a field).
    type Change = (Uuid, String, Option<Json>, Option<Json>);
    let changes: Vec<Change> = sqlx::query_as(
        "SELECT issue_id, field_key, before_value, after_value FROM execution_field_changes
         WHERE execution_id = $1 ORDER BY issue_id, field_key",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let mut fields = Vec::new();
    let mut choices = Vec::new();
    let mut triage = Vec::new();
    for (issue, field, before, after) in changes {
        let Some(key) = keys.get(&issue).cloned() else {
            continue; // not readable now: not even listed
        };
        let before = before.unwrap_or(Json::Null);
        let after = after.unwrap_or(Json::Null);
        if field == crate::triage::FIELD_KEY {
            triage.push(
                triage_preview(&mut *conn, caller, id, issue, key, &before, &after, expired)
                    .await?,
            );
            continue;
        }
        let (lv, status, vals) = &current[&issue];
        let cur = vals.get(&field).cloned().unwrap_or(Json::Null);
        let from_here = field_origin(&mut *conn, issue, &field).await? == Some(id);
        let mut reason = if expired {
            Some("expired")
        } else if before == json!("[已遮蔽]") || after == json!("[已遮蔽]") {
            Some("redacted")
        } else if cur != after || !from_here {
            Some("changed_since")
        } else {
            None
        };
        if field == "status_id" {
            let back: Option<Uuid> = before.as_str().and_then(|s| s.parse().ok());
            let options = match (reason, back) {
                (None, Some(b)) if *status != b => {
                    transitions_to(&mut *conn, caller, &key, |_, s| s == b).await?
                }
                _ => Vec::new(),
            };
            if reason.is_none() && options.is_empty() {
                reason = Some("value_no_longer_valid");
            }
            choices.push(UndoChoice {
                issue_id: issue,
                key,
                kind: "transition_back".into(),
                options,
                reason: reason.map(String::from),
            });
            continue;
        }
        let mut subreason = None;
        let mut rule = None;
        if reason.is_none()
            && let Err(e) = commands::check_as(
                &mut *conn,
                caller,
                &one_field_doc(issue, *lv, &field, &before),
            )
            .await
        {
            reason = Some(reason_of(&e));
            subreason = subreason_of(&e).map(String::from);
            if let Some(id) = form_rule_error(&e) {
                let code = crate::error::classify(&e).map(|a| a.code.as_str().to_string());
                rule = Some((code, id.as_str().map(String::from)));
            }
        }
        let (code, rule_id) = rule.unwrap_or((None, None));
        fields.push(UndoField {
            subreason,
            code,
            rule_id,
            issue_id: issue,
            key,
            field,
            before,
            after,
            current: cur,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }

    // Notes this execution added.
    let notes_rows: Vec<(Uuid, Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
        sqlx::query_as(
            "SELECT n.issue_id, n.id, n.retracted_at, n.redacted_at FROM issue_events e
             JOIN issue_notes n ON n.id = e.note_id
             WHERE (e.command_execution_id = $1 OR e.execution_id = $1) AND e.kind = 'note_added'
             ORDER BY n.id",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
    let mut notes = Vec::new();
    for (issue, note, retracted, redacted) in notes_rows {
        let Some(key) = keys.get(&issue).cloned() else {
            continue;
        };
        let lv = current[&issue].0;
        let mut reason = if expired {
            Some("expired")
        } else if redacted.is_some() {
            Some("redacted")
        } else if retracted.is_some() {
            Some("changed_since")
        } else {
            None
        };
        // The person must be able to add notes there now.
        if reason.is_none() {
            let probe = json!({"command_schema_version": 1,
                "ops": [{"op_id": Uuid::now_v7(), "kind": "add_note",
                         "target": {"type": "issue", "id": issue}, "body": "probe"}],
                "preconditions": [{"target": {"type": "issue", "id": issue}, "lock_version": lv}]});
            if let Err(e) = commands::check_as(&mut *conn, caller, &probe).await {
                reason = Some(reason_of(&e));
            }
        }
        notes.push(UndoNote {
            issue_id: issue,
            key,
            note_id: note,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }

    // Duplicate relations it created (that very instance, T-D8).
    let rels: Vec<(Uuid, Option<(Uuid, Uuid)>)> = {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT relation_id FROM execution_relation_changes
             WHERE execution_id = $1 AND action = 'created' ORDER BY relation_id",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let mut v = Vec::new();
        for r in ids {
            let ends: Option<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT source_issue_id, target_issue_id FROM issue_duplicates WHERE id = $1",
            )
            .bind(r)
            .fetch_optional(&mut *conn)
            .await?;
            v.push((r, ends));
        }
        v
    };
    let mut relations = Vec::new();
    for (rel, ends) in rels {
        let (reason, sk, tk) = match ends {
            None => (Some("changed_since"), String::new(), String::new()),
            Some((s, t)) => {
                let k = visible(&mut *conn, caller, &[s, t]).await?;
                let (Some(sk), Some(tk)) = (k.get(&s).cloned(), k.get(&t).cloned()) else {
                    continue;
                };
                let actor = crate::perm::actor(&mut *conn, caller).await?;
                let (p, tr): (Uuid, Uuid) =
                    sqlx::query_as("SELECT project_id, tracker_id FROM issues WHERE id = $1")
                        .bind(s)
                        .fetch_one(&mut *conn)
                        .await?;
                let ok = crate::perm::project_perms(&mut *conn, &actor, p)
                    .await?
                    .is_some_and(|pp| pp.allowed(crate::perm::MANAGE_ISSUE_RELATIONS, tr));
                let r = if expired {
                    Some("expired")
                } else if !ok {
                    Some("permission")
                } else {
                    None
                };
                (r, sk, tk)
            }
        };
        if ends.is_none() && sk.is_empty() {
            relations.push(UndoRelation {
                relation_id: rel,
                source_key: sk,
                target_key: tk,
                restorable: false,
                reason: reason.map(String::from),
            });
            continue;
        }
        relations.push(UndoRelation {
            relation_id: rel,
            source_key: sk,
            target_key: tk,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }

    // Issues it created: never deleted; "close as created by mistake".
    let created: Vec<Uuid> = sqlx::query_scalar(
        "SELECT issue_id FROM issue_events
         WHERE (command_execution_id = $1 OR execution_id = $1) AND kind = 'created' ORDER BY issue_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    for issue in created {
        let Some(key) = keys.get(&issue).cloned() else {
            continue;
        };
        let options = if expired {
            Vec::new()
        } else {
            transitions_to(&mut *conn, caller, &key, |cat, _| cat == "done").await?
        };
        let reason = if expired {
            Some("expired".to_string())
        } else if options.is_empty() {
            Some("value_no_longer_valid".to_string())
        } else {
            None
        };
        choices.push(UndoChoice {
            issue_id: issue,
            key,
            kind: "close_as_mistaken".into(),
            options,
            reason,
        });
    }

    // Versions (§17.13): fields by version_field_origins, a creation by soft delete.
    let (version_fields, version_created) =
        version_preview(&mut *conn, caller, id, expired).await?;

    // Time entries (§3.10.2): their issues as the caller sees them.
    let entry_issues: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT te.issue_id FROM execution_time_entry_changes c
         JOIN time_entries te ON te.id = c.time_entry_id
         WHERE c.execution_id = $1 AND te.issue_id IS NOT NULL",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let seen: BTreeSet<Uuid> = visible(&mut *conn, caller, &entry_issues)
        .await?
        .into_keys()
        .collect();
    let time_entries =
        crate::time_entries::undo_preview(&mut *conn, caller, id, expired, &seen).await?;

    Ok(UndoPreview {
        execution_id: id,
        execution_kind: info.kind.clone(),
        created_at: info.created_at,
        expired,
        fields,
        notes,
        relations,
        choices,
        version_fields,
        version_created,
        triage,
        time_entries,
    })
}

/// One triage decision of the execution, as `caller` sees it now.
#[allow(clippy::too_many_arguments)]
async fn triage_preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    issue: Uuid,
    key: String,
    before: &Json,
    after: &Json,
    expired: bool,
) -> Result<UndoTriage> {
    let state = |v: &Json| v["state"].as_str().unwrap_or("[已遮蔽]").to_string();
    let cur: Option<(Json, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {}, t.project_id FROM issue_triage t WHERE t.issue_id = $1",
        crate::triage::SNAPSHOT_SQL
    )))
    .bind(issue)
    .fetch_optional(&mut *conn)
    .await?;
    let from_here = field_origin(&mut *conn, issue, crate::triage::FIELD_KEY).await? == Some(id);
    let reason = if expired {
        Some("expired")
    } else if *before == json!("[已遮蔽]") || *after == json!("[已遮蔽]") {
        Some("redacted")
    } else if cur.as_ref().map(|c| &c.0) != Some(after) || !from_here {
        Some("changed_since")
    } else {
        let project = cur.as_ref().map(|c| c.1).unwrap_or_default();
        let actor = crate::perm::actor(&mut *conn, caller).await?;
        let ok = crate::perm::project_perms(&mut *conn, &actor, project)
            .await?
            .is_some_and(|pp| pp.writable() && pp.allowed_any(crate::perm::TRIAGE_ISSUES));
        (!ok).then_some("permission")
    };
    Ok(UndoTriage {
        issue_id: issue,
        key,
        before: state(before),
        after: state(after),
        current: cur.map(|c| state(&c.0)).unwrap_or_else(|| "none".into()),
        restorable: reason.is_none(),
        reason: reason.map(String::from),
    })
}

fn version_doc(v: Uuid, lv: i32, set: &serde_json::Map<String, Json>) -> Json {
    json!({"command_schema_version": 1,
           "ops": [{"op_id": Uuid::now_v7(), "kind": "update_version",
                    "target": {"type": "version", "id": v}, "set": set}],
           "preconditions": [{"target": {"type": "version", "id": v}, "lock_version": lv}]})
}

async fn version_origin(conn: &mut PgConnection, v: Uuid, field: &str) -> Result<Option<Uuid>> {
    let o: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT origin_execution_id FROM version_field_origins WHERE version_id = $1 AND field_key = $2",
    )
    .bind(v)
    .bind(field)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(o.flatten())
}

async fn version_preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    expired: bool,
) -> Result<(Vec<UndoVersionField>, Option<UndoVersionCreate>)> {
    type Change = (Uuid, String, Option<Json>, Option<Json>);
    let rows: Vec<Change> = sqlx::query_as(
        "SELECT version_id, field_key, before_value, after_value FROM execution_version_changes
         WHERE execution_id = $1 ORDER BY version_id, field_key",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let scope = db::load_scope(&mut *conn, caller).await?;
    let mut fields = Vec::new();
    let mut created = None;
    for (v, field, before, after) in rows {
        let Some((project, lv, deleted, name, cur)) =
            crate::versions::undo_values(&mut *conn, v).await?
        else {
            continue;
        };
        if !scope.project_ids().any(|p| p == project) {
            continue; // not readable now
        }
        if field == "_created" {
            let changed = {
                let mut any = deleted;
                for f in ["name", "description", "effective_date", "status"] {
                    if let Some(o) = version_origin(&mut *conn, v, f).await?
                        && o != id
                    {
                        any = true;
                    }
                }
                any
            };
            let uses = crate::versions::visible_uses(&mut *conn, caller, project, v).await?;
            let manage = {
                let actor = crate::perm::actor(&mut *conn, caller).await?;
                crate::perm::project_perms(&mut *conn, &actor, project)
                    .await?
                    .is_some_and(|pp| pp.allowed_any(crate::perm::MANAGE_VERSIONS))
            };
            let reason = if expired {
                Some("expired")
            } else if changed {
                Some("changed_since")
            } else if !uses.is_empty() {
                Some("in_use")
            } else if !manage {
                Some("permission")
            } else {
                None
            };
            created = Some(UndoVersionCreate {
                version_id: v,
                name,
                restorable: reason.is_none(),
                reason: reason.map(String::from),
            });
            continue;
        }
        let before = before.unwrap_or(Json::Null);
        let after = after.unwrap_or(Json::Null);
        let current = cur.get(&field).cloned().unwrap_or(Json::Null);
        let from_here = version_origin(&mut *conn, v, &field).await? == Some(id);
        let mut reason = if expired {
            Some("expired")
        } else if before == json!("[已遮蔽]") || after == json!("[已遮蔽]") {
            Some("redacted")
        } else if deleted || current != after || !from_here {
            Some("changed_since")
        } else {
            None
        };
        if reason.is_none() {
            let mut set = serde_json::Map::new();
            set.insert(field.clone(), before.clone());
            if let Err(e) = commands::check_as(&mut *conn, caller, &version_doc(v, lv, &set)).await
            {
                reason = Some(reason_of(&e));
            }
        }
        fields.push(UndoVersionField {
            version_id: v,
            name,
            field,
            before,
            after,
            current,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }
    Ok((fields, created))
}

async fn owned_info(conn: &mut PgConnection, user: Uuid, id: Uuid) -> Result<ExecInfo> {
    let info = exec_info(&mut *conn, id)
        .await?
        .filter(|i| i.owner == user)
        .ok_or_else(|| api(ErrorCode::NotFound, "no such execution"))?;
    Ok(info)
}

/// `GET /executions/{id}/undo-preview`.
pub async fn preview(app: &App, caller: AuthContext, id: Uuid) -> Result<UndoPreview> {
    let user = person(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    db::effective(&mut tx, caller).await?;
    let info = owned_info(&mut tx, user, id).await?;
    let p = build_preview(&mut tx, caller, id, &info).await?;
    tx.commit().await?;
    Ok(p)
}

/// `POST /executions/{id}/undo` (Idempotency-Key): only what is restorable
/// now, re-checked under the locks; the subset must pass the full domain
/// checks or nothing is written (`undo_not_applicable`).
pub async fn apply(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &UndoInput,
) -> Result<UndoResult> {
    let user = person(&caller)?;
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    let payload: [u8; 32] = Sha256::digest(
        [
            id.as_bytes().as_slice(),
            serde_json::to_string(input)?.as_bytes(),
        ]
        .concat(),
    )
    .into();
    let fp = commands::fingerprint(ACTION_UNDO, &payload);
    let Some(request_id) =
        commands::open_request(&mut tx, user, operation_id, ACTION_UNDO, &fp).await?
    else {
        let r = commands::stored_result(&mut tx, user, operation_id, ACTION_UNDO, &fp).await?;
        tx.commit().await?;
        return Ok(serde_json::from_value(r)?);
    };
    db::effective(&mut tx, caller).await?;
    let info = owned_info(&mut tx, user, id).await?;

    // Locks: projects in UUID order (FOR SHARE, no epoch comparison: B5),
    // then the issues FOR UPDATE, then the undo data.
    let issues = touched_issues(&mut tx, id).await?;
    let versions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT version_id FROM execution_version_changes WHERE execution_id = $1 ORDER BY 1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let mut projects: BTreeSet<Uuid> = info.sources.iter().map(|s| s.0).collect();
    let ip: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM issues WHERE id = ANY($1)
         UNION SELECT project_id FROM versions WHERE id = ANY($2)",
    )
    .bind(&issues)
    .bind(&versions)
    .fetch_all(&mut *tx)
    .await?;
    projects.extend(ip);
    // Time entries (§3.10.2): their projects and issues join the locks.
    let entries = crate::time_entries::undo_entries(&mut tx, id).await?;
    let ep: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT project_id, issue_id FROM time_entries WHERE id = ANY($1)")
            .bind(&entries)
            .fetch_all(&mut *tx)
            .await?;
    projects.extend(ep.iter().map(|e| e.0));
    let mut lock_issues: BTreeSet<Uuid> = issues.iter().copied().collect();
    lock_issues.extend(ep.iter().filter_map(|e| e.1));
    let lock_issues: Vec<Uuid> = lock_issues.into_iter().collect();
    for p in &projects {
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(p)
            .execute(&mut *tx)
            .await?;
    }
    fault("undo.after_projects").await?;
    sqlx::query("SELECT 1 FROM versions WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&versions)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT 1 FROM issues WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&lock_issues)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT 1 FROM time_entries WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&entries)
        .execute(&mut *tx)
        .await?;
    let pv = build_preview(&mut tx, caller, id, &info).await?;
    if pv.expired {
        return Err(not_applicable(json!([{"reason": "expired"}])));
    }
    if !pv.version_fields.is_empty() || pv.version_created.is_some() {
        let out =
            apply_version(&mut tx, caller, &who, request_id, id, &info, &pv, &payload).await?;
        commands::complete_request(&mut tx, request_id, &serde_json::to_value(&out)?).await?;
        tx.commit().await?;
        return Ok(out);
    }

    // The document: restorable fields not skipped, chosen transitions.
    let mut sets: BTreeMap<Uuid, serde_json::Map<String, Json>> = BTreeMap::new();
    let mut restored: Vec<(Uuid, String)> = Vec::new();
    for f in pv.fields.iter().filter(|f| f.restorable) {
        if input
            .skip_fields
            .contains(&format!("{}/{}", f.issue_id, f.field))
        {
            continue;
        }
        sets.entry(f.issue_id)
            .or_default()
            .insert(f.field.clone(), f.before.clone());
        restored.push((f.issue_id, f.field.clone()));
    }
    let mut ops: Vec<Json> = sets
        .iter()
        .map(|(i, set)| {
            json!({"op_id": Uuid::now_v7(), "kind": "update_fields",
                   "target": {"type": "issue", "id": i}, "set": set})
        })
        .collect();
    let mut n_transitions = 0usize;
    let mut restored_triage: Vec<(Uuid, String)> = Vec::new();
    for (issue, to) in &input.transitions {
        let ok = pv.choices.iter().any(|c| {
            c.issue_id == *issue
                && c.reason.is_none()
                && c.options.iter().any(|o| o.to_status_id == *to)
        });
        if !ok {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                "that transition is not one of the offered choices",
            )
            .with_details(json!({"issue_id": issue, "to_status_id": to}))
            .into());
        }
        ops.push(json!({"op_id": Uuid::now_v7(), "kind": "transition",
                        "target": {"type": "issue", "id": issue}, "to_status_id": to}));
        n_transitions += 1;
    }
    let notes: Vec<&UndoNote> = pv
        .notes
        .iter()
        .filter(|n| n.restorable && !input.skip_notes.contains(&n.note_id))
        .collect();
    let rels: Vec<&UndoRelation> = pv
        .relations
        .iter()
        .filter(|r| r.restorable && !input.skip_relations.contains(&r.relation_id))
        .collect();
    let triage: Vec<&UndoTriage> = pv
        .triage
        .iter()
        .filter(|t| t.restorable && !input.skip_triage.contains(&t.issue_id))
        .collect();
    let time: Vec<&UndoTimeEntry> = pv
        .time_entries
        .iter()
        .filter(|t| {
            t.restorable
                && !input
                    .skip_time_entries
                    .contains(&format!("{}/{}", t.time_entry_id, t.field))
        })
        .collect();
    if ops.is_empty() && notes.is_empty() && rels.is_empty() && triage.is_empty() && time.is_empty()
    {
        return Err(not_applicable(pv.reasons()));
    }
    // The daily totals come next in the lock order (after the entries,
    // before the search token the execution takes).
    let time_plan = crate::time_entries::undo_plan(&mut tx, &time).await?;
    crate::time_entries::undo_lock(&mut tx, &time_plan).await?;

    let targets: BTreeSet<Uuid> = sets
        .keys()
        .chain(input.transitions.keys())
        .copied()
        .collect();
    let mut pre = Vec::new();
    for t in &targets {
        let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
            .bind(t)
            .fetch_one(&mut *tx)
            .await?;
        pre.push(json!({"target": {"type": "issue", "id": t}, "lock_version": lv}));
    }
    let (exec_id, consistent_after) = if ops.is_empty() {
        let token = commands::search_token(&mut tx).await?;
        commands::lock_executor(&mut tx, &who).await?;
        let h: [u8; 32] = Sha256::digest(payload).into();
        let e = commands::record_undo_execution(&mut tx, request_id, user, &h, &token, id).await?;
        (e, token)
    } else {
        let doc = json!({"command_schema_version": 1, "ops": ops, "preconditions": pre});
        let parsed = commands::parse(&mut tx, &doc, crate::describe::MAX_OPS as usize)
            .await
            .map_err(|_| not_applicable(json!([{"reason": "dependency"}])))?;
        match commands::evaluate_undo(&mut tx, caller, &who, &parsed, request_id, id).await {
            Ok(r) => (r.execution_id, r.search.consistent_after),
            // A cross-field dependency or a rule the subset breaks: the
            // whole undo is refused (§16.7 L2371).
            Err(e) if crate::error::classify(&e).is_some() => {
                let code = crate::error::classify(&e).map(|a| a.code.as_str().to_string());
                if let Some(rule_id) = form_rule_error(&e) {
                    return Err(not_applicable(
                        json!([{"reason": "rules_changed", "code": code, "rule_id": rule_id}]),
                    ));
                }
                return Err(not_applicable(
                    json!([{"reason": "dependency", "code": code}]),
                ));
            }
            Err(e) => return Err(e),
        }
    };

    // Retract notes and remove relations (events, lock_version + 1: reindex).
    let mut bumps: BTreeMap<Uuid, Vec<(&'static str, Json, Option<Uuid>)>> = BTreeMap::new();
    for n in &notes {
        sqlx::query(
            "UPDATE issue_notes SET retracted_at = now(), retracted_by_execution_id = $2 WHERE id = $1",
        )
        .bind(n.note_id)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
        bumps.entry(n.issue_id).or_default().push((
            "note_retracted",
            json!({"note_id": n.note_id}),
            Some(n.note_id),
        ));
    }
    for r in &rels {
        let (s, t): (Uuid, Uuid) = sqlx::query_as(
            "DELETE FROM issue_duplicates WHERE id = $1 RETURNING source_issue_id, target_issue_id",
        )
        .bind(r.relation_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO execution_relation_changes (execution_id, relation_id, action)
             VALUES ($1, $2, 'removed')",
        )
        .bind(exec_id)
        .bind(r.relation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE execution_relation_changes SET reverted_by = $3
             WHERE execution_id = $1 AND relation_id = $2",
        )
        .bind(id)
        .bind(r.relation_id)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
        bumps.entry(s).or_default().push((
            "duplicate_removed",
            json!({"role": "source", "other_issue_id": t, "relation_id": r.relation_id}),
            None,
        ));
        bumps.entry(t).or_default().push((
            "duplicate_removed",
            json!({"role": "target", "other_issue_id": s, "relation_id": r.relation_id}),
            None,
        ));
    }
    // Triage decisions back to the state before (§3.10.1; the A5 flags of
    // the decision stay in its own event). Kept as a change of this undo,
    // so it can be undone in turn.
    let mut triage_changes: Vec<(Uuid, Json, Json)> = Vec::new();
    for t in &triage {
        let before: Option<Json> = sqlx::query_scalar(
            "SELECT before_value FROM execution_field_changes
             WHERE execution_id = $1 AND issue_id = $2 AND field_key = $3",
        )
        .bind(id)
        .bind(t.issue_id)
        .bind(crate::triage::FIELD_KEY)
        .fetch_one(&mut *tx)
        .await?;
        let before = before.unwrap_or(Json::Null);
        let (cur, now): (Json, Json) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH old AS (SELECT {} AS v FROM issue_triage t WHERE t.issue_id = $1)
             UPDATE issue_triage t SET
                 state = $2->>'state',
                 snoozed_until = ($2->>'snoozed_until')::timestamptz,
                 snoozed_at = ($2->>'snoozed_at')::timestamptz,
                 snoozed_by = ($2->>'snoozed_by')::uuid,
                 decided_at = ($2->>'decided_at')::timestamptz,
                 decided_by = ($2->>'decided_by')::uuid,
                 self_triage = COALESCE(($2->>'self_triage')::boolean, false),
                 same_sponsor = COALESCE(($2->>'same_sponsor')::boolean, false)
             FROM old WHERE t.issue_id = $1
             RETURNING old.v, {}",
            crate::triage::SNAPSHOT_SQL,
            crate::triage::SNAPSHOT_SQL
        )))
        .bind(t.issue_id)
        .bind(&before)
        .fetch_one(&mut *tx)
        .await?;
        bumps.entry(t.issue_id).or_default().push((
            "triage_changed",
            json!({"old": cur["state"], "new": now["state"],
                   "self_triage": now["self_triage"], "same_sponsor": now["same_sponsor"],
                   "snoozed_until": now["snoozed_until"]}),
            None,
        ));
        triage_changes.push((t.issue_id, cur, now));
    }
    for (issue, evs) in bumps {
        let seq: i64 = sqlx::query_scalar("SELECT last_event_seq FROM issues WHERE id = $1")
            .bind(issue)
            .fetch_one(&mut *tx)
            .await?;
        for (n, (kind, payload, note)) in evs.iter().enumerate() {
            sqlx::query(
                "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                           operation_request_id, command_execution_id, note_id, payload)
                 VALUES ($1, $2, $3, 'human_direct', $4, 'user', $5, $6, $7, $8)",
            )
            .bind(issue)
            .bind(seq + 1 + n as i64)
            .bind(kind)
            .bind(user)
            .bind(request_id)
            .bind(exec_id)
            .bind(note)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE issues SET lock_version = lock_version + 1, last_event_seq = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(issue)
        .bind(seq + evs.len() as i64)
        .execute(&mut *tx)
        .await?;
    }

    for (issue, cur, now) in &triage_changes {
        let event: Uuid = sqlx::query_scalar(
            "SELECT id FROM issue_events WHERE issue_id = $1 AND command_execution_id = $2
               AND kind = 'triage_changed' ORDER BY seq DESC LIMIT 1",
        )
        .bind(issue)
        .bind(exec_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO issue_field_origins (issue_id, field_key, origin_execution_id, origin_event_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (issue_id, field_key) DO UPDATE
               SET origin_execution_id = EXCLUDED.origin_execution_id,
                   origin_event_id = EXCLUDED.origin_event_id",
        )
        .bind(issue)
        .bind(crate::triage::FIELD_KEY)
        .bind(exec_id)
        .bind(event)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO execution_field_changes (execution_id, issue_id, field_key, before_value, after_value)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(exec_id)
        .bind(issue)
        .bind(crate::triage::FIELD_KEY)
        .bind(cur)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        restored_triage.push((*issue, crate::triage::FIELD_KEY.to_string()));
    }

    // Time entries (§3.10.2): soft delete the created ones, put fields back.
    let time_restored = crate::time_entries::undo_write(
        &mut tx,
        &time_plan,
        id,
        &commands::Attribution::human(user, request_id, exec_id),
    )
    .await?;

    // What was reverted, and the transitive source snapshot.
    for (issue, field) in restored.iter().chain(&restored_triage) {
        sqlx::query(
            "UPDATE execution_field_changes SET reverted_by = $4
             WHERE execution_id = $1 AND issue_id = $2 AND field_key = $3",
        )
        .bind(id)
        .bind(issue)
        .bind(field)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
    }
    for (p, epoch) in &info.sources {
        sqlx::query(
            "INSERT INTO command_execution_source_projects (execution_id, project_id, security_epoch)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(exec_id)
        .bind(p)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
    }
    let out = UndoResult {
        execution_id: exec_id,
        reverts_execution_id: id,
        fields_restored: restored.len(),
        notes_retracted: notes.len(),
        relations_removed: rels.len(),
        transitions: n_transitions,
        triage_restored: restored_triage.len(),
        time_entries_restored: time_restored,
        version_fields_restored: 0,
        version_deleted: false,
        consistent_after,
    };
    commands::complete_request(&mut tx, request_id, &serde_json::to_value(&out)?).await?;
    tx.commit().await?;
    Ok(out)
}

/// The version half of an undo (§17.13, L2759): the version is locked by
/// `apply` (its project FOR SHARE first); fields through `update_version`,
/// a creation by soft delete. No agent credential, delegation or quota is
/// looked at (T-V35).
#[allow(clippy::too_many_arguments)]
async fn apply_version(
    tx: &mut PgConnection,
    caller: AuthContext,
    who: &commands::Executor,
    request_id: Uuid,
    id: Uuid,
    info: &ExecInfo,
    pv: &UndoPreview,
    payload: &[u8; 32],
) -> Result<UndoResult> {
    let restorable: Vec<&UndoVersionField> =
        pv.version_fields.iter().filter(|f| f.restorable).collect();
    let create = pv.version_created.as_ref().filter(|c| c.restorable);
    if restorable.is_empty() && create.is_none() {
        return Err(not_applicable(pv.reasons()));
    }
    let (exec_id, token) = if let Some(f) = restorable.first() {
        let v = f.version_id;
        let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
            .bind(v)
            .fetch_one(&mut *tx)
            .await?;
        let mut set = serde_json::Map::new();
        for f in &restorable {
            set.insert(f.field.clone(), f.before.clone());
        }
        let parsed = commands::parse(&mut *tx, &version_doc(v, lv, &set), 1).await?;
        match commands::evaluate_undo(&mut *tx, caller, who, &parsed, request_id, id).await {
            Ok(r) => (r.execution_id, r.search.consistent_after),
            Err(e) if crate::error::classify(&e).is_some() => {
                let code = crate::error::classify(&e).map(|a| a.code.as_str().to_string());
                if let Some(rule_id) = form_rule_error(&e) {
                    return Err(not_applicable(
                        json!([{"reason": "rules_changed", "code": code, "rule_id": rule_id}]),
                    ));
                }
                return Err(not_applicable(
                    json!([{"reason": "dependency", "code": code}]),
                ));
            }
            Err(e) => return Err(e),
        }
    } else {
        let token = commands::search_token(&mut *tx).await?;
        commands::lock_executor(&mut *tx, who).await?;
        let h: [u8; 32] = Sha256::digest(payload).into();
        let e = commands::record_undo_execution(&mut *tx, request_id, who.user_id, &h, &token, id)
            .await?;
        (e, token)
    };
    for f in &restorable {
        sqlx::query(
            "UPDATE execution_version_changes SET reverted_by = $4
             WHERE execution_id = $1 AND version_id = $2 AND field_key = $3",
        )
        .bind(id)
        .bind(f.version_id)
        .bind(&f.field)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(c) = create {
        let at = commands::Attribution::human(who.user_id, request_id, exec_id);
        crate::versions::undo_soft_delete(&mut *tx, c.version_id, &at).await?;
        sqlx::query(
            "UPDATE execution_version_changes SET reverted_by = $3
             WHERE execution_id = $1 AND version_id = $2 AND field_key = '_created'",
        )
        .bind(id)
        .bind(c.version_id)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
    }
    for (p, epoch) in &info.sources {
        sqlx::query(
            "INSERT INTO command_execution_source_projects (execution_id, project_id, security_epoch)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(exec_id)
        .bind(p)
        .bind(epoch)
        .execute(&mut *tx)
        .await?;
    }
    Ok(UndoResult {
        execution_id: exec_id,
        reverts_execution_id: id,
        fields_restored: 0,
        notes_retracted: 0,
        relations_removed: 0,
        transitions: 0,
        triage_restored: 0,
        time_entries_restored: 0,
        version_fields_restored: restorable.len(),
        version_deleted: create.is_some(),
        consistent_after: token,
    })
}

// ---------------------------------------------------------------- incident recovery

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct IncidentItem {
    pub execution_id: Uuid,
    pub at: DateTime<Utc>,
    pub keys: Vec<String>,
    /// Fields, notes or relations it can restore without a choice.
    pub automatic: bool,
    /// Transitions or "close as created by mistake" to do one by one.
    pub needs_choice: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct IncidentPreview {
    pub agent_principal_id: Uuid,
    pub since: DateTime<Utc>,
    pub frozen_at: DateTime<Utc>,
    /// Newest first: the SPA sends one undo per item, in this order.
    pub items: Vec<IncidentItem>,
}

/// The agent's executions on the caller's behalf since `since`, frozen now
/// (§16.7 L2372, T-D12): writes after this list are not in it.
async fn agent_executions(
    conn: &mut PgConnection,
    user: Uuid,
    agent: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(Uuid, DateTime<Utc>, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, created_at, 'direct' FROM command_executions
         WHERE actor_id = $1 AND on_behalf_of_id = $2 AND origin = 'agent_direct'
           AND created_at >= $3 AND created_at <= $4
         UNION ALL
         SELECT e.id, e.created_at, 'proposal' FROM changeset_executions e
         JOIN changesets c ON c.id = e.changeset_id
         WHERE c.proposer_id = $1 AND c.on_behalf_of_id = $2 AND e.status = 'succeeded'
           AND e.created_at >= $3 AND e.created_at <= $4
         ORDER BY 2 DESC, 1 DESC",
    )
    .bind(agent)
    .bind(user)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?)
}

/// `POST /agents/{id}/incident-recovery/preview`.
pub async fn incident_preview(
    app: &App,
    caller: AuthContext,
    agent: Uuid,
    since: DateTime<Utc>,
) -> Result<IncidentPreview> {
    let user = person(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    db::effective(&mut tx, caller).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut items = Vec::new();
    for (id, at, _) in agent_executions(&mut tx, user, agent, since, now).await? {
        let info = owned_info(&mut tx, user, id).await?;
        let p = build_preview(&mut tx, caller, id, &info).await?;
        let mut keys: Vec<String> = p
            .fields
            .iter()
            .map(|f| f.key.clone())
            .chain(p.notes.iter().map(|n| n.key.clone()))
            .chain(p.choices.iter().map(|c| c.key.clone()))
            .collect();
        keys.sort();
        keys.dedup();
        items.push(IncidentItem {
            execution_id: id,
            at,
            keys,
            automatic: p.automatic(),
            needs_choice: p.choices.iter().any(|c| c.reason.is_none()),
        });
    }
    tx.commit().await?;
    Ok(IncidentPreview {
        agent_principal_id: agent,
        since,
        frozen_at: now,
        items,
    })
}

// ---------------------------------------------------------------- activity and summary

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct ActivityDiff {
    pub key: String,
    pub field: String,
    pub before: Json,
    pub after: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct ActivityItem {
    /// `direct`, `proposal` (applied), `blocked` or `undo`.
    pub kind: String,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<Uuid>,
    pub keys: Vec<String>,
    pub diff: Vec<ActivityDiff>,
    pub undoable: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct Activity {
    pub items: Vec<ActivityItem>,
}

/// `GET /agents/{id}/activity`: direct writes, applied proposals, refusals
/// (B8: the audience blocks only) and undos, newest first, within the
/// caller's current visibility.
pub async fn activity(
    app: &App,
    caller: AuthContext,
    agent: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    issue: Option<&str>,
) -> Result<Activity> {
    let user = person(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    db::effective(&mut tx, caller).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let from = from.unwrap_or(now - chrono::Duration::days(UNDO_DAYS));
    let to = to.unwrap_or(now);
    let mut rows = agent_executions(&mut tx, user, agent, from, to).await?;
    let undos: Vec<(Uuid, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT u.id, u.created_at, 'undo' FROM command_executions u
         WHERE u.actor_id = $1 AND u.reverts_execution_id IS NOT NULL
           AND u.created_at >= $2 AND u.created_at <= $3",
    )
    .bind(user)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    rows.extend(undos);
    let blocked: Vec<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT l.at FROM agent_write_log l
         JOIN agent_principals ap ON ap.agent_principal_id = l.agent_principal_id
         WHERE l.agent_principal_id = $1 AND ap.delegator_user_id = $2 AND l.kind = 'audience_blocked'
           AND l.at >= $3 AND l.at <= $4",
    )
    .bind(agent)
    .bind(user)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    let mut items = Vec::new();
    for (id, at, kind) in rows {
        let issues = touched_issues(&mut tx, id).await?;
        let keys_map = visible(&mut tx, caller, &issues).await?;
        if keys_map.is_empty() {
            continue; // nothing of it is readable now
        }
        let mut keys: Vec<String> = keys_map.values().cloned().collect();
        keys.sort();
        if let Some(k) = issue
            && !keys.iter().any(|x| x == k)
        {
            continue;
        }
        let ch: Vec<(Uuid, String, Option<Json>, Option<Json>)> = sqlx::query_as(
            "SELECT issue_id, field_key, before_value, after_value FROM execution_field_changes
             WHERE execution_id = $1 ORDER BY issue_id, field_key",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        let diff = ch
            .into_iter()
            .filter_map(|(i, f, b, a)| {
                Some(ActivityDiff {
                    key: keys_map.get(&i)?.clone(),
                    field: f,
                    before: b.unwrap_or(Json::Null),
                    after: a.unwrap_or(Json::Null),
                })
            })
            .collect();
        let undoable = match exec_info(&mut tx, id).await? {
            Some(info) if info.owner == user => {
                build_preview(&mut tx, caller, id, &info).await?.automatic()
            }
            _ => false,
        };
        items.push(ActivityItem {
            kind,
            at,
            execution_id: Some(id),
            keys,
            diff,
            undoable,
        });
    }
    if issue.is_none() {
        for at in blocked {
            items.push(ActivityItem {
                kind: "blocked".into(),
                at,
                execution_id: None,
                keys: Vec::new(),
                diff: Vec::new(),
                undoable: false,
            });
        }
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.at));
    tx.commit().await?;
    Ok(Activity { items })
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct AgentToday {
    pub agent_principal_id: Uuid,
    pub label: String,
    /// Distinct issues changed today that the caller can see now.
    pub issues_changed: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct SuspendedContext {
    pub context_id: Uuid,
    pub agent_principal_id: Uuid,
    pub reason: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct AgentSummary {
    pub today: Vec<AgentToday>,
    pub suspended: Vec<SuspendedContext>,
}

/// `GET /home/agent-summary` (§16.10 L2426). Turning the card off is the
/// SPA's own setting (B9).
pub async fn summary(app: &App, caller: AuthContext) -> Result<AgentSummary> {
    let user = person(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    db::effective(&mut tx, caller).await?;
    let start: DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('day', now())")
        .fetch_one(&mut *tx)
        .await?;
    let agents: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT ap.agent_principal_id, p.name FROM agent_principals ap
         JOIN principals p ON p.id = ap.agent_principal_id
         WHERE ap.delegator_user_id = $1 ORDER BY p.name",
    )
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let mut today = Vec::new();
    for (agent, label) in agents {
        let mut issues: BTreeSet<Uuid> = BTreeSet::new();
        for (id, _, _) in agent_executions(&mut tx, user, agent, start, now).await? {
            issues.extend(touched_issues(&mut tx, id).await?);
        }
        let ids: Vec<Uuid> = issues.into_iter().collect();
        let n = visible(&mut tx, caller, &ids).await?.len();
        if n > 0 {
            today.push(AgentToday {
                agent_principal_id: agent,
                label,
                issues_changed: n,
            });
        }
    }
    let suspended: Vec<(Uuid, Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT c.id, c.agent_principal_id, w.suspension_reason, w.suspended_at
         FROM agent_context_write_controls w JOIN agent_work_contexts c ON c.id = w.context_id
         JOIN agent_principals ap ON ap.agent_principal_id = c.agent_principal_id
         WHERE ap.delegator_user_id = $1 AND w.suspended_at IS NOT NULL
           AND w.suspension_reason <> 'manual'
         ORDER BY w.suspended_at DESC",
    )
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(AgentSummary {
        today,
        suspended: suspended
            .into_iter()
            .map(|(c, a, r, at)| SuspendedContext {
                context_id: c,
                agent_principal_id: a,
                reason: r,
                at,
            })
            .collect(),
    })
}
