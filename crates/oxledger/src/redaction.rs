// SPDX-License-Identifier: GPL-2.0-or-later
//! D12 redaction (contract 02 §7.5, §16.7, §17.13, §18.2; S7).
//!
//! One transaction per redaction, in the order §7.5 gives: the project row
//! `FOR UPDATE` and `security_epoch + 1` (every agent context that read the
//! project is quarantined, §3.8) → the issue / version and the note or event
//! `FOR UPDATE` → the manager check → the text replaced, a redaction record
//! that never holds the original → the derived copies ([`derived`]) → an
//! outbox row so the index follows. Any failure rolls all of it back.
//!
//! Propose, revise and undo take the same project rows `FOR SHARE` before
//! they read or copy text, so each one runs wholly before or wholly after a
//! redaction (T-S9, T-X22, T-D14).

use crate::auth::{self, AuthContext, SessionPolicy};
use crate::citations::Reader;
use crate::commands::{self, Executor};
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, MANAGE_PROJECT, VIEW_ISSUES};
use crate::service::App;
use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// What a redacted note body, event payload or derived value becomes.
pub const REDACTED: &str = "[已遮蔽]";
/// The reason codes (C3).
pub const REASON_CODES: &[&str] = &["personal_data", "confidential", "legal", "other"];
pub const MAX_REASON_CHARS: usize = 500;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RedactInput {
    /// One of `personal_data`, `confidential`, `legal`, `other`.
    pub reason_code: String,
    /// Why (1–500 characters). Never paste the original text here.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Redacted {
    /// False when it was already redacted (nothing written, epoch unchanged).
    pub changed: bool,
    pub redaction_id: Option<Uuid>,
    /// The project's epoch after this call.
    pub security_epoch: i64,
}

/// What to redact.
#[derive(Debug, Clone)]
pub enum Target {
    Note {
        key: String,
        note: Uuid,
    },
    /// The `event_id` that `recent_events` returns (never the internal seq).
    IssueEvent {
        key: String,
        event: Uuid,
    },
    VersionEvent {
        version: Uuid,
        seq: i64,
    },
    SettingEvent {
        project: String,
        event: Uuid,
    },
    /// A time entry's comments (contract 05 §2.5 D12; a deleted entry too,
    /// its text is still stored): the entry, every
    /// event of it that holds the comments, and its undo copies.
    TimeEntry {
        entry: Uuid,
    },
}

impl Target {
    fn action(&self) -> &'static str {
        match self {
            Target::Note { .. } => "redact_note",
            Target::IssueEvent { .. } => "redact_issue_event",
            Target::VersionEvent { .. } => "redact_version_event",
            Target::SettingEvent { .. } => "redact_setting_event",
            Target::TimeEntry { .. } => "redact_time_entry",
        }
    }
}

/// Rows the derived-copy rule touched (for tests and logs; no text).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Derived {
    pub contents: u64,
    pub labels: u64,
    pub field_changes: u64,
    pub version_changes: u64,
    /// `execution_time_entry_changes` rows (contract 05 §2.5 D12).
    pub time_changes: u64,
}

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

fn not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no such {what}"))
}

fn invalid(field: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg.to_string())
        .with_details(json!({"field": field}))
        .into()
}

fn check_input(input: &RedactInput) -> Result<()> {
    if !REASON_CODES.contains(&input.reason_code.as_str()) {
        return Err(invalid(
            "reason_code",
            "reason_code is one of personal_data, confidential, legal, other",
        ));
    }
    let r = input.reason.trim();
    let n = r.chars().count();
    if n == 0 || n > MAX_REASON_CHARS || r.chars().any(|c| c.is_control() && c != '\n') {
        return Err(invalid(
            "reason",
            "reason is 1–500 characters without control characters",
        ));
    }
    Ok(())
}

/// The located target: its project and the ids the transaction locks.
struct Located {
    project: Uuid,
    issue: Option<Uuid>,
    version: Option<Uuid>,
    row: Uuid,
    admin_only: bool,
}

async fn is_admin(conn: &mut PgConnection, caller: AuthContext) -> Result<bool> {
    let me = db::effective(&mut *conn, caller).await?;
    Ok(me.is_admin && me.agent.is_none())
}

/// (issue, project, tracker): visible to the caller, or any live issue for
/// an administrator (C9: archived projects included).
async fn find_issue(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
    admin: bool,
) -> Result<(Uuid, Uuid, Uuid)> {
    match describe::visible_issue(&mut *conn, caller, key).await {
        Ok(i) => Ok((i.id, i.project_id, i.tracker_id)),
        Err(e) if admin => {
            let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
                "SELECT i.id, i.project_id, i.tracker_id FROM issue_keys k JOIN issues i ON i.id = k.issue_id
                 WHERE k.key = $1 AND i.archived_at IS NULL",
            )
            .bind(key.trim().to_uppercase())
            .fetch_optional(&mut *conn)
            .await?;
            row.ok_or(e)
        }
        Err(e) => Err(e),
    }
}

async fn find_project(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
    admin: bool,
) -> Result<Uuid> {
    match describe::visible_project(&mut *conn, caller, project).await {
        Ok(p) => Ok(p),
        Err(_) if admin => {
            let text = project.trim();
            let id: Option<Uuid> = match Uuid::parse_str(text) {
                Ok(id) => {
                    sqlx::query_scalar("SELECT id FROM projects WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&mut *conn)
                        .await?
                }
                Err(_) => {
                    sqlx::query_scalar("SELECT id FROM projects WHERE key = $1")
                        .bind(text.to_uppercase())
                        .fetch_optional(&mut *conn)
                        .await?
                }
            };
            id.ok_or_else(|| not_found("project"))
        }
        Err(_) => Err(not_found("project")),
    }
}

/// A private note (or its event) the caller may not read looks missing.
async fn note_hidden(
    conn: &mut PgConnection,
    caller: AuthContext,
    admin: bool,
    project: Uuid,
    tracker: Uuid,
    note: Uuid,
) -> Result<bool> {
    if admin {
        return Ok(false);
    }
    let (p, a, b): (bool, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT is_private, author_id, created_on_behalf_of_id FROM issue_notes WHERE id = $1",
    )
    .bind(note)
    .fetch_one(&mut *conn)
    .await?;
    let mut reader = Reader::new(&mut *conn, caller).await?;
    Ok(!reader
        .note_visible(&mut *conn, project, tracker, p, a, b)
        .await?)
}

async fn locate(conn: &mut PgConnection, caller: AuthContext, t: &Target) -> Result<Located> {
    let admin = is_admin(&mut *conn, caller).await?;
    match t {
        Target::Note { key, note } => {
            let (issue, project, tracker) = find_issue(&mut *conn, caller, key, admin).await?;
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM issue_notes WHERE id = $1 AND issue_id = $2)",
            )
            .bind(note)
            .bind(issue)
            .fetch_one(&mut *conn)
            .await?;
            if !exists || note_hidden(&mut *conn, caller, admin, project, tracker, *note).await? {
                return Err(not_found("note"));
            }
            Ok(Located {
                project,
                issue: Some(issue),
                version: None,
                row: *note,
                admin_only: false,
            })
        }
        Target::IssueEvent { key, event } => {
            let (issue, project, tracker) = find_issue(&mut *conn, caller, key, admin).await?;
            let row: Option<Option<Uuid>> = sqlx::query_scalar(
                "SELECT note_id FROM issue_events WHERE id = $1 AND issue_id = $2",
            )
            .bind(event)
            .bind(issue)
            .fetch_optional(&mut *conn)
            .await?;
            let Some(note) = row else {
                return Err(not_found("event"));
            };
            // A private note's event is as hidden as the note.
            if let Some(note) = note
                && note_hidden(&mut *conn, caller, admin, project, tracker, note).await?
            {
                return Err(not_found("event"));
            }
            Ok(Located {
                project,
                issue: Some(issue),
                version: None,
                row: *event,
                admin_only: false,
            })
        }
        Target::VersionEvent { version, seq } => {
            let project: Option<Uuid> =
                sqlx::query_scalar("SELECT project_id FROM versions WHERE id = $1")
                    .bind(version)
                    .fetch_optional(&mut *conn)
                    .await?;
            let Some(project) = project else {
                return Err(not_found("version"));
            };
            find_project(&mut *conn, caller, &project.to_string(), admin)
                .await
                .map_err(|_| not_found("version"))?;
            let id: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM version_events WHERE version_id = $1 AND seq = $2",
            )
            .bind(version)
            .bind(seq)
            .fetch_optional(&mut *conn)
            .await?;
            Ok(Located {
                project,
                issue: None,
                version: Some(*version),
                row: id.ok_or_else(|| not_found("event"))?,
                admin_only: false,
            })
        }
        Target::TimeEntry { entry } => {
            let row: Option<(Uuid, Option<Uuid>, Uuid)> = sqlx::query_as(
                "SELECT project_id, issue_id, user_id FROM time_entries WHERE id = $1",
            )
            .bind(entry)
            .fetch_optional(&mut *conn)
            .await?;
            let Some((project, issue, user)) = row else {
                return Err(not_found("time entry"));
            };
            find_project(&mut *conn, caller, &project.to_string(), admin)
                .await
                .map_err(|_| not_found("time entry"))?;
            // Someone who cannot read the entry cannot tell it exists.
            if !admin {
                let me = caller.user_id().unwrap_or_default();
                let seen = match crate::time_entries::visibility(&mut *conn, project, me)
                    .await?
                    .as_deref()
                {
                    Some("all") => true,
                    Some("own") => user == me,
                    _ => false,
                };
                let issue_seen = match issue {
                    None => true,
                    Some(i) => {
                        let scope = db::load_scope(&mut *conn, caller).await?;
                        let f: Option<(Uuid, Uuid, bool, Uuid, Option<Uuid>)> = sqlx::query_as(
                            "SELECT project_id, tracker_id, is_private, author_id, assigned_to_id
                             FROM issues WHERE id = $1 AND archived_at IS NULL",
                        )
                        .bind(i)
                        .fetch_optional(&mut *conn)
                        .await?;
                        f.is_some_and(|f| {
                            scope.can_see(&ox_core::IssueFacts {
                                project_id: f.0,
                                tracker_id: f.1,
                                is_private: f.2,
                                author_id: f.3,
                                assigned_to_id: f.4,
                            })
                        })
                    }
                };
                if !seen || !issue_seen {
                    return Err(not_found("time entry"));
                }
            }
            Ok(Located {
                project,
                issue,
                version: None,
                row: *entry,
                admin_only: false,
            })
        }
        Target::SettingEvent { project, event } => {
            let project = find_project(&mut *conn, caller, project, admin).await?;
            // The same rule the list applies (T-PT17), on the stored payload.
            let admin_only: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {} FROM project_setting_events e WHERE e.id = $1 AND e.project_id = $2",
                crate::project_tabs::ADMIN_ONLY_SQL
            )))
            .bind(event)
            .bind(project)
            .fetch_optional(&mut *conn)
            .await?;
            let admin_only = admin_only.ok_or_else(|| not_found("event"))?;
            if admin_only && !admin {
                return Err(not_found("event"));
            }
            Ok(Located {
                project,
                issue: None,
                version: None,
                row: *event,
                admin_only,
            })
        }
    }
}

fn request_hash(t: &Target, input: &RedactInput) -> [u8; 32] {
    let target = match t {
        Target::Note { key, note } => json!({"key": key.trim().to_uppercase(), "note": note}),
        Target::IssueEvent { key, event } => {
            json!({"key": key.trim().to_uppercase(), "event": event})
        }
        Target::VersionEvent { version, seq } => json!({"version": version, "seq": seq}),
        Target::SettingEvent { project, event } => {
            json!({"project": project.trim().to_uppercase(), "event": event})
        }
        Target::TimeEntry { entry } => json!({"time_entry": entry}),
    };
    let body =
        json!({"target": target, "reason_code": input.reason_code, "reason": input.reason.trim()});
    Sha256::digest(
        [
            b"oxledger/redact/v1\n".as_slice(),
            body.to_string().as_bytes(),
        ]
        .concat(),
    )
    .into()
}

/// Redacts one note or event (Idempotency-Key = `operation_id`).
pub async fn redact(
    app: &App,
    caller: AuthContext,
    policy: &SessionPolicy,
    operation_id: Uuid,
    target: &Target,
    input: &RedactInput,
) -> Result<Redacted> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    check_input(input)?;
    let mut tx = app.pool.begin().await?;
    let action = target.action();
    let fp = commands::fingerprint(action, &request_hash(target, input));
    let Some(request) =
        commands::open_request(&mut tx, who.user_id, operation_id, action, &fp).await?
    else {
        let first =
            commands::stored_result(&mut tx, who.user_id, operation_id, action, &fp).await?;
        tx.commit().await?;
        return Ok(serde_json::from_value(first["redacted"].clone())?);
    };
    let out = redact_locked(&mut tx, caller, policy, &who, target, input).await?;
    commands::complete_request(&mut tx, request, &json!({"redacted": out})).await?;
    tx.commit().await?;
    Ok(out)
}

async fn redact_locked(
    tx: &mut PgConnection,
    caller: AuthContext,
    policy: &SessionPolicy,
    who: &Executor,
    target: &Target,
    input: &RedactInput,
) -> Result<Redacted> {
    let at = locate(&mut *tx, caller, target).await?;
    commands::lock_executor(&mut *tx, who).await?;
    auth::require_step_up(&mut *tx, policy, &caller).await?;
    // §7.5 step 1: the project row first, held to the commit.
    let epoch: i64 =
        sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1 FOR UPDATE")
            .bind(at.project)
            .fetch_one(&mut *tx)
            .await?;
    may_redact(&mut *tx, caller, at.project).await?;
    if let Some(issue) = at.issue {
        sqlx::query("SELECT 1 FROM issues WHERE id = $1 FOR UPDATE")
            .bind(issue)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(version) = at.version {
        sqlx::query("SELECT 1 FROM versions WHERE id = $1 FOR UPDATE")
            .bind(version)
            .execute(&mut *tx)
            .await?;
    }
    let done: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(match target {
        Target::Note { .. } => {
            "SELECT redacted_at IS NOT NULL FROM issue_notes WHERE id = $1 FOR UPDATE"
        }
        Target::IssueEvent { .. } => {
            "SELECT redaction_id IS NOT NULL FROM issue_events WHERE id = $1 FOR UPDATE"
        }
        Target::VersionEvent { .. } => {
            "SELECT redaction_id IS NOT NULL FROM version_events WHERE id = $1 FOR UPDATE"
        }
        Target::SettingEvent { .. } => {
            "SELECT redaction_id IS NOT NULL FROM project_setting_events WHERE id = $1 FOR UPDATE"
        }
        Target::TimeEntry { .. } => {
            "SELECT redacted_at IS NOT NULL FROM time_entries WHERE id = $1 FOR UPDATE"
        }
    }))
    .bind(at.row)
    .fetch_one(&mut *tx)
    .await?;
    if done {
        // C4: already redacted; nothing new, epoch unchanged.
        return Ok(Redacted {
            changed: false,
            redaction_id: None,
            security_epoch: epoch,
        });
    }
    let epoch: i64 = sqlx::query_scalar(
        "UPDATE projects SET security_epoch = security_epoch + 1 WHERE id = $1 RETURNING security_epoch",
    )
    .bind(at.project)
    .fetch_one(&mut *tx)
    .await?;
    let reason = input.reason.trim();
    let id: Uuid = match target {
        Target::Note { .. } => {
            sqlx::query("UPDATE issue_notes SET body = $2, redacted_at = now() WHERE id = $1")
                .bind(at.row)
                .bind(REDACTED)
                .execute(&mut *tx)
                .await?;
            // Design 06 §3.13: who the note mentioned goes with it.
            sqlx::query(
                "DELETE FROM issue_event_mentions
                 WHERE event_id IN (SELECT id FROM issue_events WHERE note_id = $1)",
            )
            .bind(at.row)
            .execute(&mut *tx)
            .await?;
            sqlx::query_scalar(
                "INSERT INTO issue_event_redactions (note_id, redacted_by, reason_code, reason)
                 VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(at.row)
            .bind(who.user_id)
            .bind(&input.reason_code)
            .bind(reason)
            .fetch_one(&mut *tx)
            .await?
        }
        Target::IssueEvent { .. } => {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO issue_event_redactions (event_id, redacted_by, reason_code, reason)
                 VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(at.row)
            .bind(who.user_id)
            .bind(&input.reason_code)
            .bind(reason)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE issue_events SET payload = to_jsonb($2::text), redaction_id = $3 WHERE id = $1")
                .bind(at.row)
                .bind(REDACTED)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            // Design 05 §2.2: a redacted change cannot be filtered on (ever_in).
            sqlx::query("DELETE FROM issue_field_changes WHERE event_id = $1")
                .bind(at.row)
                .execute(&mut *tx)
                .await?;
            // Design 06 §3.13: and who the event mentioned.
            sqlx::query("DELETE FROM issue_event_mentions WHERE event_id = $1")
                .bind(at.row)
                .execute(&mut *tx)
                .await?;
            id
        }
        Target::VersionEvent { .. } => {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO version_event_redactions (event_id, redacted_by, reason_code, reason)
                 VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(at.row)
            .bind(who.user_id)
            .bind(&input.reason_code)
            .bind(reason)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE version_events SET payload = to_jsonb($2::text), redaction_id = $3 WHERE id = $1")
                .bind(at.row)
                .bind(REDACTED)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            id
        }
        Target::SettingEvent { .. } => {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO project_setting_event_redactions (event_id, redacted_by, reason_code, reason, admin_only)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(at.row)
            .bind(who.user_id)
            .bind(&input.reason_code)
            .bind(reason)
            .bind(at.admin_only)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE project_setting_events SET payload = to_jsonb($2::text), redaction_id = $3 WHERE id = $1")
                .bind(at.row)
                .bind(REDACTED)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            id
        }
        Target::TimeEntry { .. } => {
            redact_time_entry(&mut *tx, at.row, who.user_id, &input.reason_code, reason).await?
        }
    };
    fault("redact.before_derived").await?;
    derived(&mut *tx, at.project, who.user_id).await?;
    if let Some(issue) = at.issue {
        // C7: re-index without touching the issue row.
        sqlx::query("INSERT INTO outbox (topic, entity_id) VALUES ('issue', $1)")
            .bind(issue)
            .execute(&mut *tx)
            .await?;
    }
    Ok(Redacted {
        changed: true,
        redaction_id: Some(id),
        security_epoch: epoch,
    })
}

/// `manage_project` on a project that is not archived, or a system
/// administrator; administrator-only events need an administrator (C2, C9).
async fn may_redact(conn: &mut PgConnection, caller: AuthContext, project: Uuid) -> Result<()> {
    if is_admin(&mut *conn, caller).await? {
        return Ok(());
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(|| not_found("project"))?;
    if pp.status == "archived" || !pp.allowed_any(VIEW_ISSUES) {
        return Err(not_found("project"));
    }
    if !pp.allowed_any(MANAGE_PROJECT) {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "redaction needs manage_project".to_string(),
        )
        .with_details(json!({"reason": "permission"}))
        .into());
    }
    Ok(())
}

/// §7.5 / §16.7 / §17.13 derived copies of project `project`, in the
/// caller's transaction (which holds the project row `FOR UPDATE`):
///
/// * every proposal content of a changeset whose destination is the project
///   or any of whose versions has the project in its source snapshot, all
///   versions; and those changesets' group labels;
/// * `execution_field_changes` of executions that wrote an issue of the
///   project or whose source snapshot holds it (a changeset execution uses
///   its version's snapshot; an undo inherits the reverted one's);
/// * `execution_version_changes` likewise for versions of the project.
pub async fn derived(conn: &mut PgConnection, project: Uuid, actor: Uuid) -> Result<Derived> {
    // Design 05 §3.13.7: built-in AI messages drawn from the project lose
    // their content in the same transaction (by project, erring wide).
    crate::ai::chat::redact_project(&mut *conn, project).await?;
    let changesets ="SELECT id FROM changesets WHERE context_project_id = $1
                      UNION SELECT changeset_id FROM changeset_version_source_projects WHERE project_id = $1";
    let contents = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE proposal_contents pc
         SET body = NULL, baseline = NULL, state = 'redacted', redacted_at = now(), redacted_by = $2
         FROM changeset_versions v
         WHERE v.content_id = pc.id AND pc.state = 'available' AND v.changeset_id IN ({changesets})"
    )))
    .bind(project)
    .bind(actor)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let labels = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE changeset_groups SET label = NULL, label_state = 'redacted'
         WHERE label_state = 'available'
           AND id IN (SELECT group_id FROM changesets WHERE group_id IS NOT NULL AND id IN ({changesets}))"
    )))
    .bind(project)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let sources = "SELECT execution_id FROM command_execution_source_projects WHERE project_id = $1
                   UNION SELECT e.id FROM changeset_executions e
                         JOIN changeset_version_source_projects s
                           ON s.changeset_id = e.changeset_id AND s.version = e.version
                         WHERE s.project_id = $1";
    let marker = "to_jsonb($2::text)";
    let field_changes = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE execution_field_changes f SET before_value = {marker}, after_value = {marker}
         WHERE (f.before_value IS DISTINCT FROM {marker} OR f.after_value IS DISTINCT FROM {marker})
           AND (f.issue_id IN (SELECT id FROM issues WHERE project_id = $1)
                OR f.execution_id IN ({sources}))"
    )))
    .bind(project)
    .bind(REDACTED)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let version_changes = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE execution_version_changes f SET before_value = {marker}, after_value = {marker}
         WHERE (f.before_value IS DISTINCT FROM {marker} OR f.after_value IS DISTINCT FROM {marker})
           AND (f.version_id IN (SELECT id FROM versions WHERE project_id = $1)
                OR f.execution_id IN ({sources}))"
    )))
    .bind(project)
    .bind(REDACTED)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    // Contract 05 §2.5 D12: the time entry undo copies likewise (whole
    // values; a `_created` / `_deleted` snapshot becomes the marker too).
    let time_changes = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE execution_time_entry_changes f
         SET before_value = CASE WHEN f.before_value IS NULL THEN NULL ELSE {marker} END,
             after_value = CASE WHEN f.after_value IS NULL THEN NULL ELSE {marker} END
         WHERE (f.before_value IS DISTINCT FROM {marker} AND f.before_value IS NOT NULL
                OR f.after_value IS DISTINCT FROM {marker} AND f.after_value IS NOT NULL)
           AND (f.time_entry_id IN (SELECT id FROM time_entries WHERE project_id = $1)
                OR f.execution_id IN ({sources}))"
    )))
    .bind(project)
    .bind(REDACTED)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(Derived {
        contents,
        labels,
        field_changes,
        version_changes,
        time_changes,
    })
}

/// Replaces a time entry's comments with the marker, in the entry and in
/// every event that holds them (one redaction record per such event; the
/// first is returned). The undo copies follow in [`derived`].
async fn redact_time_entry(
    conn: &mut PgConnection,
    entry: Uuid,
    by: Uuid,
    reason_code: &str,
    reason: &str,
) -> Result<Uuid> {
    sqlx::query("UPDATE time_entries SET comments = $2, redacted_at = now() WHERE id = $1")
        .bind(entry)
        .bind(REDACTED)
        .execute(&mut *conn)
        .await?;
    let events: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT id, payload FROM time_entry_events WHERE time_entry_id = $1 ORDER BY seq FOR UPDATE",
    )
    .bind(entry)
    .fetch_all(&mut *conn)
    .await?;
    let marker = json!(REDACTED);
    let mut first = None;
    for (event, mut payload) in events {
        let mut touched = false;
        if let Some(ch) = payload.get_mut("changes").and_then(|c| c.as_object_mut()) {
            if let Some(c) = ch.get_mut("comments").and_then(|c| c.as_object_mut()) {
                for side in ["old", "new"] {
                    if c.get(side).is_some_and(|v| !v.is_null() && *v != marker) {
                        c.insert(side.into(), marker.clone());
                        touched = true;
                    }
                }
            }
            for snap in ["_deleted", "_restored"] {
                for side in ["old", "new"] {
                    if let Some(s) = ch
                        .get_mut(snap)
                        .and_then(|c| c.get_mut(side))
                        .and_then(|v| v.as_object_mut())
                        && s.get("comments").is_some_and(|v| *v != marker)
                    {
                        s.insert("comments".into(), marker.clone());
                        touched = true;
                    }
                }
            }
        }
        if !touched {
            continue;
        }
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO time_entry_event_redactions (event_id, redacted_by, reason_code, reason)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (event_id) DO UPDATE SET redacted_at = now()
             RETURNING id",
        )
        .bind(event)
        .bind(by)
        .bind(reason_code)
        .bind(reason)
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query("UPDATE time_entry_events SET payload = $2, redaction_id = $3 WHERE id = $1")
            .bind(event)
            .bind(&payload)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        first.get_or_insert(id);
    }
    // Every entry has its `created` event; a blank comment leaves nothing
    // to replace there, so record the redaction on that event anyway.
    match first {
        Some(id) => Ok(id),
        None => Ok(sqlx::query_scalar(
            "INSERT INTO time_entry_event_redactions (event_id, redacted_by, reason_code, reason)
             SELECT id, $2, $3, $4 FROM time_entry_events WHERE time_entry_id = $1
             ORDER BY seq LIMIT 1
             ON CONFLICT (event_id) DO UPDATE SET redacted_at = now()
             RETURNING id",
        )
        .bind(entry)
        .bind(by)
        .bind(reason_code)
        .bind(reason)
        .fetch_one(&mut *conn)
        .await?),
    }
}
