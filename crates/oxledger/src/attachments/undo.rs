// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 R-A7 (U18): undoing a person's `delete_attachment` within 30
//! days puts the file back; undoing that undo deletes it again. The rows
//! are `execution_attachment_changes`; who may do it is who may delete the
//! attachment now (R-A7, R-W14), checked again under the container's lock.

use super::ops::{Holder, holder_event, record_change, removal_check, set_deleted};
use crate::auth::AuthContext;
use crate::commands::{self, Executor};
use crate::error::{ErrorCode, classify};
use crate::perm;
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// One attachment an execution deleted or put back, as its undo would
/// treat it.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoAttachment {
    pub attachment_id: Uuid,
    pub filename: String,
    /// `deleted` (undo: put it back) or `restored` (undo: delete it again).
    pub change: String,
    /// Where it hangs: the issue's key, or the wiki page's title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki_title: Option<String>,
    pub restorable: bool,
    /// `expired`, `changed_since` (put back or deleted again since),
    /// `redacted`, `content_gone` (the file was removed),
    /// `container_gone` (the issue, note or page is gone or out of reach),
    /// `project_not_writable` or `permission`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Whether execution `id` deleted or put back an attachment (its undo is
/// this module's).
pub(crate) async fn has_changes(conn: &mut PgConnection, id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM execution_attachment_changes WHERE execution_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

#[derive(sqlx::FromRow)]
struct ChangeRow {
    attachment_id: Uuid,
    change: String,
    reverted: bool,
    filename: String,
    project_id: Uuid,
    issue_id: Option<Uuid>,
    note_id: Option<Uuid>,
    wiki_page_id: Option<Uuid>,
    author_id: Uuid,
    deleted: bool,
    deleted_by: Option<Uuid>,
    redacted: bool,
    present: bool,
    /// No later execution changed this attachment (R-W9's rule).
    latest: bool,
}

async fn changes_of(conn: &mut PgConnection, id: Uuid) -> Result<Vec<ChangeRow>> {
    Ok(sqlx::query_as(
        "SELECT c.attachment_id, c.change, c.reverted_by IS NOT NULL AS reverted, a.filename,
                a.project_id, a.issue_id, a.note_id, a.wiki_page_id, a.author_id,
                a.deleted_at IS NOT NULL AS deleted, a.deleted_by_execution_id AS deleted_by,
                a.redacted_at IS NOT NULL AS redacted, b.state = 'present' AS present,
                NOT EXISTS (SELECT 1 FROM execution_attachment_changes l
                            WHERE l.attachment_id = c.attachment_id
                              AND l.execution_id > c.execution_id) AS latest
         FROM execution_attachment_changes c
         JOIN attachments a ON a.id = c.attachment_id
         JOIN attachment_blobs b ON b.id = a.blob_id
         WHERE c.execution_id = $1 ORDER BY c.attachment_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Where the attachment hangs and whether `caller` may change it now,
/// without locks (the preview's view; `apply` checks again under them).
/// `None`: the container is out of the caller's sight (left out).
async fn where_and_may(
    conn: &mut PgConnection,
    caller: AuthContext,
    c: &ChangeRow,
) -> Result<Option<(Option<String>, Option<String>, Option<&'static str>)>> {
    let me = caller.user_id();
    let author = me == Some(c.author_id);
    if let Some(page) = c.wiki_page_id {
        let Some(a) = crate::wiki::access_of(&mut *conn, caller, c.project_id).await? else {
            return Ok(None);
        };
        let (title, gone, protected): (String, bool, bool) = sqlx::query_as(
            "SELECT title, deleted_at IS NOT NULL, protected FROM wiki_pages WHERE id = $1",
        )
        .bind(page)
        .fetch_one(&mut *conn)
        .await?;
        // The order of removal_check: permission, a closed project, then
        // protection.
        let reason = if gone {
            Some("container_gone")
        } else if !(a.has(perm::DELETE_WIKI_PAGES) || (author && a.has(perm::EDIT_WIKI_PAGES))) {
            Some("permission")
        } else if !a.writable() {
            Some("project_not_writable")
        } else if protected && !a.has(perm::PROTECT_WIKI_PAGES) {
            Some("permission")
        } else {
            None
        };
        return Ok(Some((None, Some(title), reason)));
    }
    let Some(issue) = c.issue_id else {
        return Ok(None);
    };
    type Row = (String, Uuid, bool, Uuid, Option<Uuid>, Option<Uuid>, bool);
    let (key, tracker, private, i_author, assignee, obo, archived): Row = sqlx::query_as(
        "SELECT k.key, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                i.created_on_behalf_of_id, i.archived_at IS NOT NULL
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = $1",
    )
    .bind(issue)
    .fetch_one(&mut *conn)
    .await?;
    let scope = crate::db::load_scope(&mut *conn, caller).await?;
    let facts = IssueFacts {
        project_id: c.project_id,
        tracker_id: tracker,
        is_private: private,
        author_id: i_author,
        assigned_to_id: assignee,
    };
    if !scope.can_see(&facts) {
        return Ok(None);
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    let Some(pp) = perm::project_perms(&mut *conn, &actor, c.project_id).await? else {
        return Ok(None);
    };
    // A note: retracted is gone; one the caller may no longer read is left
    // out (R-A5), as removal_check refuses it.
    let note_gone = match c.note_id {
        Some(n) => {
            let (retracted, private, n_author, n_obo): (bool, bool, Uuid, Option<Uuid>) =
                sqlx::query_as(
                    "SELECT retracted_at IS NOT NULL, is_private, author_id, created_on_behalf_of_id
                     FROM issue_notes WHERE id = $1",
                )
                .bind(n)
                .fetch_one(&mut *conn)
                .await?;
            let mut reader = crate::citations::Reader::new(&mut *conn, caller).await?;
            if !reader
                .note_visible(&mut *conn, c.project_id, tracker, private, n_author, n_obo)
                .await?
            {
                return Ok(None);
            }
            retracted
        }
        None => false,
    };
    let add = if c.note_id.is_some() {
        perm::ADD_ISSUE_NOTES
    } else {
        perm::ADD_ISSUES
    };
    let own = perm::is_own(me, i_author, obo);
    let reason = if archived || note_gone {
        Some("container_gone")
    } else if !pp.writable() {
        Some("project_not_writable")
    } else if !(pp.attributes_editable(tracker, own) || (author && pp.allowed(add, tracker))) {
        Some("permission")
    } else {
        None
    };
    Ok(Some((Some(key), None, reason)))
}

/// The state checks of one row (no permission): `None` when it can be
/// undone as far as the row goes.
fn state_reason(c: &ChangeRow, id: Uuid, expired: bool) -> Option<&'static str> {
    if expired {
        return Some("expired");
    }
    let back = c.change == "deleted";
    let as_left = if back {
        c.deleted && c.deleted_by == Some(id)
    } else {
        !c.deleted
    };
    if c.reverted || !c.latest || !as_left {
        return Some("changed_since");
    }
    if back && c.redacted {
        return Some("redacted");
    }
    if back && !c.present {
        return Some("content_gone");
    }
    None
}

/// The undo preview of execution `id`'s attachment rows, as `caller` sees
/// them now (attachments whose issue or wiki they cannot see are left out).
pub(crate) async fn preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    expired: bool,
) -> Result<Vec<UndoAttachment>> {
    let mut out = Vec::new();
    for c in changes_of(&mut *conn, id).await? {
        let Some((issue_key, wiki_title, may)) = where_and_may(&mut *conn, caller, &c).await?
        else {
            continue;
        };
        let reason = state_reason(&c, id, expired).or(may);
        out.push(UndoAttachment {
            attachment_id: c.attachment_id,
            filename: c.filename.clone(),
            change: c.change.clone(),
            issue_key,
            wiki_title,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }
    Ok(out)
}

/// A refusal of [`removal_check`] as an undo reason; other errors stay.
fn reason_of(e: anyhow::Error) -> Result<&'static str> {
    let Some(x) = classify(&e) else {
        return Err(e);
    };
    let reason = x
        .details
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    match x.code {
        ErrorCode::NotFound => Ok("container_gone"),
        ErrorCode::ForbiddenOp if reason == "project_not_writable" => Ok("project_not_writable"),
        ErrorCode::ForbiddenOp => Ok("permission"),
        _ => Err(e),
    }
}

/// `POST /executions/{id}/undo` for an execution with attachment rows:
/// each row still undoable (not in `skip`) is checked again under its
/// container's lock and reverted; one new execution. Returns how many,
/// the execution, its token and the reasons of the rest.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    request: Uuid,
    id: Uuid,
    executed_at: DateTime<Utc>,
    skip: &[Uuid],
    payload: &[u8; 32],
) -> Result<(usize, Uuid, String, Json)> {
    // One undo of these rows at a time.
    sqlx::query(
        "SELECT 1 FROM execution_attachment_changes WHERE execution_id = $1
         ORDER BY attachment_id FOR UPDATE",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let expired = now - executed_at > chrono::Duration::days(crate::undo::UNDO_DAYS);
    let mut reasons: Vec<Json> = Vec::new();
    let mut todo: Vec<(Uuid, String, Holder, String)> = Vec::new();
    for w in preview(&mut *conn, caller, id, expired).await? {
        if skip.contains(&w.attachment_id) {
            continue;
        }
        let mut reason = w.reason.clone();
        if reason.is_none() {
            let back = w.change == "deleted";
            match removal_check(&mut *conn, caller, who.user_id, w.attachment_id, back).await {
                Ok((holder, filename)) => {
                    // The file stays while it comes back (the GC purges
                    // a generation under FOR UPDATE).
                    sqlx::query(
                        "SELECT 1 FROM attachment_blobs WHERE id =
                           (SELECT blob_id FROM attachments WHERE id = $1) FOR SHARE",
                    )
                    .bind(w.attachment_id)
                    .execute(&mut *conn)
                    .await?;
                    // Under the locks: the row as the preview saw it.
                    let row = changes_of(&mut *conn, id)
                        .await?
                        .into_iter()
                        .find(|c| c.attachment_id == w.attachment_id);
                    match row.and_then(|c| state_reason(&c, id, expired)) {
                        None => {
                            todo.push((w.attachment_id, w.change.clone(), holder, filename));
                            continue;
                        }
                        Some(r) => reason = Some(r.into()),
                    }
                }
                Err(e) => {
                    let r = reason_of(e)?;
                    // The row changed while waiting for the container's
                    // lock (a delete or undo committed): say so.
                    let now = changes_of(&mut *conn, id)
                        .await?
                        .into_iter()
                        .find(|c| c.attachment_id == w.attachment_id)
                        .and_then(|c| state_reason(&c, id, expired));
                    reason = Some(
                        if r == "container_gone" {
                            now.unwrap_or(r)
                        } else {
                            r
                        }
                        .into(),
                    );
                }
            }
        }
        if let Some(r) = reason {
            reasons.push(json!({"attachment_id": w.attachment_id, "field": w.change, "reason": r}));
        }
    }
    let reasons = Json::Array(reasons);
    if todo.is_empty() {
        return Ok((0, Uuid::nil(), String::new(), reasons));
    }
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    commands::check_deadline(&mut *conn, deadline).await?;
    let token = commands::search_token(&mut *conn).await?;
    let h: [u8; 32] = Sha256::digest(payload).into();
    let exec =
        commands::record_undo_execution(&mut *conn, request, who.user_id, &h, &token, id).await?;
    for (att, change, holder, filename) in &todo {
        let (kind, now_change) = if change == "deleted" {
            set_deleted(&mut *conn, *att, None).await?;
            ("attachment_added", "restored")
        } else {
            set_deleted(&mut *conn, *att, Some(exec)).await?;
            ("attachment_removed", "deleted")
        };
        record_change(&mut *conn, exec, *att, now_change).await?;
        sqlx::query(
            "UPDATE execution_attachment_changes SET reverted_by = $3
             WHERE execution_id = $1 AND attachment_id = $2",
        )
        .bind(id)
        .bind(att)
        .bind(exec)
        .execute(&mut *conn)
        .await?;
        holder_event(
            &mut *conn,
            holder,
            kind,
            *att,
            filename,
            who.user_id,
            request,
            exec,
            None,
        )
        .await?;
    }
    Ok((todo.len(), exec, token, reasons))
}
