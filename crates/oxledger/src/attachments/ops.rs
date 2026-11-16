// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 §3.11, R-A5, R-A7, R-W14: attaching files (`attach`, and the
//! `attachments` param of `create_issue`, `add_note` and `wiki_put`) and
//! deleting them (`delete_attachment`). People only (R-A9).

use super::{disabled, invalid_value, normalize_filename};
use crate::auth::AuthContext;
use crate::commands::{
    self, CommandResult, Effect, Executor, ItemResult, SearchReceipt, stored_effect,
};
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, ADD_ISSUE_NOTES, ADD_ISSUES, DELETE_WIKI_PAGES, EDIT_WIKI_PAGES};
use anyhow::Result;
use ox_core::IssueFacts;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::BTreeSet;
use uuid::Uuid;

/// The attachment op kinds (design 06 §3.11).
pub const KINDS: [&str; 2] = ["attach", "delete_attachment"];

const NOT_FOUND: &str = "no such attachment";

fn bad(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidRequest, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn bad_argument(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn forbidden(reason: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

pub(crate) fn attachment_not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, NOT_FOUND)
}

/// R-A9: what an agent gets for an `attachments` param (before parsing).
pub(crate) fn people_only(path: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, "only people attach files")
        .with_details(json!({"reason": "attachments_people_only", "path": path}))
        .into()
}

// ---------------------------------------------------------------- input

/// One file to attach: an upload of the caller's in the same project, not
/// used yet and not expired (R-A1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentInput {
    pub upload_id: Uuid,
    /// Another name than the upload's (the same rules as an upload's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Where `attach` puts the files: exactly one of `{"issue": key}`,
/// `{"note": note_id}` and `{"project", "page"|"page_id"}`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachTarget {
    /// An issue key (`AUTH-12`): the issue's own attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// A note id: read as the note is (private notes included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Uuid>,
    /// A project id or key, with `page` or `page_id`: a wiki page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachOp {
    pub op_id: Uuid,
    pub target: AttachTarget,
    /// 1–10 files.
    pub attachments: Vec<AttachmentInput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRef {
    pub attachment: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteAttachmentOp {
    pub op_id: Uuid,
    /// `{"attachment": id}`.
    pub target: AttachmentRef,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentOp {
    Attach(AttachOp),
    DeleteAttachment(DeleteAttachmentOp),
}

impl AttachmentOp {
    fn op_id(&self) -> Uuid {
        match self {
            AttachmentOp::Attach(o) => o.op_id,
            AttachmentOp::DeleteAttachment(o) => o.op_id,
        }
    }
}

/// An attachment document: exactly one `attach` or `delete_attachment`
/// (people only; no preconditions: attachments are not fields).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentCommandDoc {
    /// Always 1.
    pub command_schema_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Exactly one attachment op.
    pub ops: Vec<AttachmentOp>,
    /// Always empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Json>,
}

/// Whether `raw` is an attachment document (any op kind is one).
pub(crate) fn is_attachment_doc(raw: &Json) -> bool {
    raw.get("ops").and_then(Json::as_array).is_some_and(|ops| {
        ops.iter().any(|o| {
            o.get("kind")
                .and_then(Json::as_str)
                .is_some_and(|k| KINDS.contains(&k))
        })
    })
}

/// The `attachments` list of one op: at most 10 (`attachment_limit`), each
/// upload once, names checked as an upload's.
pub(crate) fn check_inputs(path: &str, list: &[AttachmentInput]) -> Result<()> {
    if list.len() > super::MAX_PER_DOCUMENT {
        return Err(limit(path));
    }
    let mut seen = BTreeSet::new();
    for (i, a) in list.iter().enumerate() {
        if !seen.insert(a.upload_id) {
            return Err(bad(
                &format!("{path}[{i}].upload_id"),
                "the same upload is named twice",
            ));
        }
        if let Some(f) = &a.filename {
            normalize_filename(f, &format!("{path}[{i}].filename"))?;
        }
    }
    Ok(())
}

/// R-A2: more than 10 files in one document.
pub(crate) fn limit(path: &str) -> anyhow::Error {
    invalid_value(
        path,
        "attachment_limit",
        format!(
            "a command document attaches at most {} files",
            super::MAX_PER_DOCUMENT
        ),
    )
}

fn parse(raw: &Json) -> Result<AttachmentCommandDoc> {
    let n = raw
        .get("ops")
        .and_then(Json::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if n != 1 {
        return Err(bad("ops", "an attachment document has exactly one op"));
    }
    let doc: AttachmentCommandDoc = serde_json::from_value(raw.clone())
        .map_err(|e| bad("", format!("not a valid attachment command document: {e}")))?;
    if doc.command_schema_version != 1 {
        return Err(bad(
            "command_schema_version",
            "command_schema_version must be 1",
        ));
    }
    if doc.title.as_ref().is_some_and(|t| t.chars().count() > 255) {
        return Err(bad("title", "title is at most 255 characters"));
    }
    if !doc.preconditions.is_empty() {
        return Err(bad_argument(
            "preconditions",
            "attachment documents take no preconditions",
        ));
    }
    if let AttachmentOp::Attach(o) = &doc.ops[0] {
        let t = &o.target;
        let page = t.page.is_some() || t.page_id.is_some();
        let named = [t.issue.is_some(), t.note.is_some(), t.project.is_some()]
            .iter()
            .filter(|x| **x)
            .count();
        if named != 1
            || (t.project.is_some() && t.page.is_some() == t.page_id.is_some())
            || (t.project.is_none() && page)
        {
            return Err(bad(
                "ops[0].target",
                "target is one of {issue}, {note} and {project, page or page_id}",
            ));
        }
        if o.attachments.is_empty() {
            return Err(bad(
                "ops[0].attachments",
                "attach names at least one upload",
            ));
        }
        check_inputs("ops[0].attachments", &o.attachments)?;
    }
    Ok(doc)
}

// ---------------------------------------------------------------- taking uploads

/// An upload taken for a new attachment (its row locked).
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub id: Uuid,
    pub upload: Uuid,
    pub blob: Uuid,
    pub filename: String,
    pub size: i64,
    pub content_type: String,
}

/// R-A1, T-M2-53: each upload must be the caller's, in `project`, unused,
/// not expired, and its content still present; anything else is the same
/// `invalid_value` (`upload_unknown`). Locks the uploads (FOR UPDATE) and
/// their blobs (FOR SHARE, against the GC and redaction).
pub(crate) async fn take(
    conn: &mut PgConnection,
    uploader: Uuid,
    project: Uuid,
    list: &[AttachmentInput],
    path: &str,
) -> Result<Vec<Planned>> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    if !super::enabled() {
        return Err(disabled());
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, a) in list.iter().enumerate() {
        let row: Option<(Uuid, String, i64, String)> = sqlx::query_as(
            "SELECT u.blob_id, u.filename, u.size, b.sniffed_type
             FROM uploads u JOIN attachment_blobs b ON b.id = u.blob_id
             WHERE u.id = $1 AND u.uploader_id = $2 AND u.project_id = $3
               AND u.consumed_at IS NULL AND u.expires_at > now() AND b.state = 'present'
             FOR UPDATE OF u FOR SHARE OF b",
        )
        .bind(a.upload_id)
        .bind(uploader)
        .bind(project)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((blob, name, size, content_type)) = row else {
            return Err(invalid_value(
                &format!("{path}[{i}].upload_id"),
                "upload_unknown",
                "no such upload of yours in this project (uploads go after 24 hours or once attached)",
            ));
        };
        let filename = match &a.filename {
            Some(f) => normalize_filename(f, &format!("{path}[{i}].filename"))?,
            None => name,
        };
        out.push(Planned {
            id: Uuid::now_v7(),
            upload: a.upload_id,
            blob,
            filename,
            size,
            content_type,
        });
    }
    Ok(out)
}

/// What the attachments hang on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Container {
    Issue { issue: Uuid, note: Option<Uuid> },
    Wiki { page: Uuid, version: i32 },
}

/// R-A7: marks the uploads used and writes the attachment rows (the
/// caller writes the events).
pub(crate) async fn insert(
    conn: &mut PgConnection,
    project: Uuid,
    container: Container,
    planned: &[Planned],
    author: Uuid,
    execution: Uuid,
) -> Result<()> {
    if planned.is_empty() {
        return Ok(());
    }
    let uploads: Vec<Uuid> = planned.iter().map(|p| p.upload).collect();
    sqlx::query("UPDATE uploads SET consumed_at = now() WHERE id = ANY($1)")
        .bind(&uploads)
        .execute(&mut *conn)
        .await?;
    let (issue, note, page, version) = match container {
        Container::Issue { issue, note } => (Some(issue), note, None, None),
        Container::Wiki { page, version } => (None, None, Some(page), Some(version)),
    };
    for p in planned {
        sqlx::query(
            "INSERT INTO attachments (id, project_id, issue_id, note_id, wiki_page_id, wiki_version,
                                      blob_id, filename, size, content_type, author_id,
                                      created_execution_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(p.id)
        .bind(project)
        .bind(issue)
        .bind(note)
        .bind(page)
        .bind(version)
        .bind(p.blob)
        .bind(&p.filename)
        .bind(p.size)
        .bind(&p.content_type)
        .bind(author)
        .bind(execution)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The payload of `attachment_added` / `attachment_removed` (D12 range).
pub(crate) fn event_payload(id: Uuid, filename: &str) -> Json {
    json!({"attachment_id": id, "filename": filename})
}

// ---------------------------------------------------------------- run (POST /commands)

/// An issue the caller sees, locked for the write.
struct IssueRow {
    id: Uuid,
    key: String,
    project: Uuid,
    tracker: Uuid,
    own: bool,
    lock_version: i32,
}

/// `{"issue": key}` (or the issue of a note): visible (else `not_found`),
/// then locked FOR UPDATE.
async fn issue_for_write(
    conn: &mut PgConnection,
    caller: AuthContext,
    me: Uuid,
    id: Option<Uuid>,
    key: Option<&str>,
) -> Result<Option<IssueRow>> {
    let scope = crate::db::load_scope(&mut *conn, caller).await?;
    type Row = (
        Uuid,
        String,
        Uuid,
        Uuid,
        bool,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        i32,
    );
    let key = key.map(|k| k.trim().to_uppercase());
    if key
        .as_deref()
        .is_some_and(|k| !crate::describe::well_formed_key(k))
    {
        return Ok(None);
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT i.id, ck.key, i.project_id, i.tracker_id, i.is_private, i.author_id,
                i.assigned_to_id, i.created_on_behalf_of_id, i.lock_version
         FROM issues i JOIN issue_keys ck ON ck.issue_id = i.id AND ck.is_current
         WHERE i.archived_at IS NULL
           AND (i.id = $1 OR i.id = (SELECT issue_id FROM issue_keys WHERE key = $2))
         FOR UPDATE OF i",
    )
    .bind(id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, key, project, tracker, private, author, assignee, obo, lv)) = row else {
        return Ok(None);
    };
    let facts = IssueFacts {
        project_id: project,
        tracker_id: tracker,
        is_private: private,
        author_id: author,
        assigned_to_id: assignee,
    };
    if !scope.can_see(&facts) {
        return Ok(None);
    }
    Ok(Some(IssueRow {
        id,
        key,
        project,
        tracker,
        own: perm::is_own(Some(me), author, obo),
        lock_version: lv,
    }))
}

/// Writes events on an issue after its last one (`attachment_added`,
/// `attachment_removed`); the issue's lock_version stays (attachments are
/// not fields), its last event and `updated_at` move.
#[allow(clippy::too_many_arguments)]
async fn issue_events(
    conn: &mut PgConnection,
    issue: Uuid,
    kind: &str,
    rows: &[(Uuid, String, Option<Uuid>)],
    who: Uuid,
    request: Uuid,
    exec: Uuid,
    op_id: Uuid,
) -> Result<()> {
    for (id, filename, note) in rows {
        sqlx::query(
            "WITH s AS (UPDATE issues SET last_event_seq = last_event_seq + 1, updated_at = now()
                        WHERE id = $1 RETURNING last_event_seq)
             INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                       operation_request_id, command_execution_id, op_id, note_id,
                                       payload)
             SELECT $1, s.last_event_seq, $2, 'human_direct', $3, 'user', $4, $5, $6, $7, $8 FROM s",
        )
        .bind(issue)
        .bind(kind)
        .bind(who)
        .bind(request)
        .bind(exec)
        .bind(op_id)
        .bind(note)
        .bind(event_payload(*id, filename))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The note of `{"note": id}`: on a visible issue, readable by the caller,
/// not retracted; `None` otherwise.
async fn note_for_write(
    conn: &mut PgConnection,
    caller: AuthContext,
    me: Uuid,
    note: Uuid,
) -> Result<Option<(IssueRow, bool)>> {
    let row: Option<(Uuid, bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT issue_id, is_private, author_id, created_on_behalf_of_id FROM issue_notes
         WHERE id = $1 AND retracted_at IS NULL",
    )
    .bind(note)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((issue, _, _, _)) = row else {
        return Ok(None);
    };
    let Some(i) = issue_for_write(&mut *conn, caller, me, Some(issue), None).await? else {
        return Ok(None);
    };
    // Again under the issue's lock: an undo retracting the note (it locks
    // the issue too) may have committed in between.
    let row: Option<(bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT is_private, author_id, created_on_behalf_of_id FROM issue_notes
         WHERE id = $1 AND issue_id = $2 AND retracted_at IS NULL",
    )
    .bind(note)
    .bind(issue)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((private, author, obo)) = row else {
        return Ok(None);
    };
    let mut reader = crate::citations::Reader::new(&mut *conn, caller).await?;
    if !reader
        .note_visible(&mut *conn, i.project, i.tracker, private, author, obo)
        .await?
    {
        return Ok(None);
    }
    Ok(Some((i, perm::is_own(Some(me), author, obo))))
}

struct Done {
    effects: Vec<Effect>,
    item: ItemResult,
}

fn item(op_id: Uuid, issue: Option<Uuid>, page: Option<Uuid>, ids: Vec<Uuid>) -> ItemResult {
    ItemResult {
        op_id,
        status: "applied".into(),
        issue_id: issue,
        version_id: None,
        time_entry_id: None,
        wiki_page_id: page,
        attachment_ids: ids,
    }
}

fn issue_effect(i: &IssueRow, kind: &str) -> Effect {
    Effect {
        issue_id: i.id,
        key: i.key.clone(),
        lock_version: i.lock_version,
        changed: true,
        created: false,
        events: vec![kind.to_string()],
    }
}

fn no_permission(msg: &str) -> anyhow::Error {
    forbidden("permission", msg.to_string())
}

/// A closed project takes no writes (the same reason as a wiki's); the
/// permission checks come after.
fn writable(pp: &perm::ProjectPerms) -> Result<()> {
    if pp.allowed_any(perm::VIEW_ISSUES) && !pp.writable() {
        return Err(not_writable());
    }
    Ok(())
}

fn not_writable() -> anyhow::Error {
    forbidden(
        "project_not_writable",
        "the project is closed: it takes no writes",
    )
}

/// The execution of a person's attachment document.
struct Exec {
    exec: Uuid,
    token: String,
}

async fn begin(
    conn: &mut PgConnection,
    who: &Executor,
    request: Uuid,
    hash: &[u8; 32],
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Exec> {
    commands::check_deadline(&mut *conn, deadline).await?;
    let token = commands::search_token(&mut *conn).await?;
    let exec =
        commands::record_human_execution(&mut *conn, request, who.user_id, hash, &token).await?;
    Ok(Exec { exec, token })
}

/// `POST /commands` with an attachment document (from `commands::run`):
/// parsed, the operation request opened (a used key replays), then run.
pub(crate) async fn run(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    raw: &Json,
) -> Result<CommandResult> {
    let doc = parse(raw)?;
    let hash: [u8; 32] = Sha256::digest(commands::canonical_json(raw).as_bytes()).into();
    let fp = commands::fingerprint(commands::ACTION_COMMAND, &hash);
    let Some(request) = commands::open_request(
        &mut *conn,
        who.user_id,
        operation_id,
        commands::ACTION_COMMAND,
        &fp,
    )
    .await?
    else {
        return replay(&mut *conn, caller, who.user_id, operation_id, &fp).await;
    };
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    let op = &doc.ops[0];
    let (done, x) = match op {
        AttachmentOp::Attach(o) => {
            attach(&mut *conn, caller, who, o, request, &hash, deadline).await?
        }
        AttachmentOp::DeleteAttachment(o) => {
            delete(&mut *conn, caller, who, o, request, &hash, deadline).await?
        }
    };
    let _ = op.op_id();
    let r = CommandResult {
        status: "succeeded".into(),
        execution_id: x.exec,
        effects: done.effects,
        items: vec![done.item],
        search: SearchReceipt {
            consistent_after: x.token,
        },
        time_entries: Vec::new(),
        wiki: Vec::new(),
    };
    let stored = json!({
        "execution_id": r.execution_id,
        "consistent_after": r.search.consistent_after,
        "effects": r.effects.iter().map(stored_effect).collect::<Vec<_>>(),
        "items": r.items,
    });
    commands::complete_request(&mut *conn, request, &stored).await?;
    Ok(r)
}

/// A resent attachment document: issue results through the issue replay
/// (visibility re-checked); wiki items while the caller reads that wiki.
async fn replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    user: Uuid,
    operation_id: Uuid,
    fp: &[u8; 32],
) -> Result<CommandResult> {
    let mut r = commands::replay(
        &mut *conn,
        caller,
        user,
        operation_id,
        commands::ACTION_COMMAND,
        fp,
    )
    .await?;
    let stored =
        commands::stored_result(&mut *conn, user, operation_id, commands::ACTION_COMMAND, fp)
            .await?;
    for i in serde_json::from_value::<Vec<ItemResult>>(stored["items"].clone()).unwrap_or_default()
    {
        let Some(page) = i.wiki_page_id else {
            continue;
        };
        let project: Option<Uuid> =
            sqlx::query_scalar("SELECT project_id FROM wiki_pages WHERE id = $1")
                .bind(page)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(p) = project
            && crate::wiki::access_of(&mut *conn, caller, p)
                .await?
                .is_some()
        {
            r.items.push(i);
        }
    }
    Ok(r)
}

async fn attach(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    o: &AttachOp,
    request: Uuid,
    hash: &[u8; 32],
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(Done, Exec)> {
    let me = who.user_id;
    let path = "ops[0].attachments";
    let t = &o.target;
    if let Some(project) = &t.project {
        // R-W14: a live page of a wiki the caller reads; edit_wiki_pages
        // (and protect_wiki_pages on a protected page).
        let pid = crate::wiki::project_id(&mut *conn, project)
            .await?
            .ok_or_else(crate::wiki::not_found)?;
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(pid)
            .execute(&mut *conn)
            .await?;
        let a = crate::wiki::access_of(&mut *conn, caller, pid)
            .await?
            .ok_or_else(crate::wiki::not_found)?;
        let page =
            crate::wiki::page_for_attachments(&mut *conn, &a, t.page.as_deref(), t.page_id).await?;
        if !a.has(EDIT_WIKI_PAGES) {
            return Err(no_permission(
                "attaching to a wiki page needs edit_wiki_pages in this project",
            ));
        }
        if !a.writable() {
            return Err(not_writable());
        }
        page.require_unprotected(&a)?;
        let planned = take(&mut *conn, me, pid, &o.attachments, path).await?;
        let x = begin(&mut *conn, who, request, hash, deadline).await?;
        insert(
            &mut *conn,
            pid,
            Container::Wiki {
                page: page.id,
                version: page.version,
            },
            &planned,
            me,
            x.exec,
        )
        .await?;
        let prov = crate::wiki::Prov::human(me, request, x.exec);
        for p in &planned {
            crate::wiki::attachment_event(
                &mut *conn,
                page.id,
                "attachment_added",
                event_payload(p.id, &p.filename),
                &prov,
            )
            .await?;
        }
        let ids = planned.iter().map(|p| p.id).collect();
        return Ok((
            Done {
                effects: Vec::new(),
                item: item(o.op_id, None, Some(page.id), ids),
            },
            x,
        ));
    }
    // An issue, or a note of one.
    let (issue, note, may) = if let Some(note) = t.note {
        let (i, own_note) = note_for_write(&mut *conn, caller, me, note)
            .await?
            .ok_or_else(|| api(ErrorCode::NotFound, "no such note"))?;
        let pp = issue_perms(&mut *conn, caller, i.project).await?;
        writable(&pp)?;
        let may = pp.attributes_editable(i.tracker, i.own)
            || (own_note && pp.allowed(ADD_ISSUE_NOTES, i.tracker));
        (i, Some(note), may)
    } else {
        let key = t.issue.as_deref().unwrap_or_default();
        let i = issue_for_write(&mut *conn, caller, me, None, Some(key))
            .await?
            .ok_or_else(|| commands::issue_not_found(key))?;
        let pp = issue_perms(&mut *conn, caller, i.project).await?;
        writable(&pp)?;
        let may = pp.attributes_editable(i.tracker, i.own);
        (i, None, may)
    };
    if !may {
        return Err(no_permission(if note.is_some() {
            "attaching to a note needs edit_issues, or being its author with add_issue_notes"
        } else {
            "attaching to an issue needs edit_issues (or edit_own_issues on your own issue)"
        }));
    }
    let planned = take(&mut *conn, me, issue.project, &o.attachments, path).await?;
    let x = begin(&mut *conn, who, request, hash, deadline).await?;
    insert(
        &mut *conn,
        issue.project,
        Container::Issue {
            issue: issue.id,
            note,
        },
        &planned,
        me,
        x.exec,
    )
    .await?;
    let rows: Vec<_> = planned
        .iter()
        .map(|p| (p.id, p.filename.clone(), note))
        .collect();
    issue_events(
        &mut *conn,
        issue.id,
        "attachment_added",
        &rows,
        me,
        request,
        x.exec,
        o.op_id,
    )
    .await?;
    let ids = planned.iter().map(|p| p.id).collect();
    Ok((
        Done {
            effects: vec![issue_effect(&issue, "attachment_added")],
            item: item(o.op_id, Some(issue.id), None, ids),
        },
        x,
    ))
}

async fn issue_perms(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<perm::ProjectPerms> {
    let actor = perm::actor(&mut *conn, caller).await?;
    perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(attachment_not_found)
}

/// One attachment row.
#[derive(sqlx::FromRow)]
struct Row {
    project_id: Uuid,
    issue_id: Option<Uuid>,
    note_id: Option<Uuid>,
    wiki_page_id: Option<Uuid>,
    filename: String,
    author_id: Uuid,
}

async fn delete(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    o: &DeleteAttachmentOp,
    request: Uuid,
    hash: &[u8; 32],
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(Done, Exec)> {
    let me = who.user_id;
    let id = o.target.attachment;
    let row: Option<Row> = sqlx::query_as(
        "SELECT project_id, issue_id, note_id, wiki_page_id, filename, author_id
         FROM attachments WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Err(attachment_not_found());
    };
    let author = r.author_id == me;
    if let Some(page) = r.wiki_page_id {
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(r.project_id)
            .execute(&mut *conn)
            .await?;
        let a = crate::wiki::access_of(&mut *conn, caller, r.project_id)
            .await?
            .ok_or_else(attachment_not_found)?;
        // A gone page is this attachment's not_found; a lock timeout or
        // deadlock stays itself (retried, or temporarily_unavailable).
        let page = crate::wiki::page_for_attachments(&mut *conn, &a, None, Some(page))
            .await
            .map_err(|e| {
                if crate::error::classify(&e).is_some_and(|x| x.code == ErrorCode::NotFound) {
                    attachment_not_found()
                } else {
                    e
                }
            })?;
        // R-W14: delete_wiki_pages, or its author with edit_wiki_pages.
        if !(a.has(DELETE_WIKI_PAGES) || (author && a.has(EDIT_WIKI_PAGES))) {
            return Err(no_permission(
                "deleting a wiki attachment needs delete_wiki_pages, or being its author with edit_wiki_pages",
            ));
        }
        if !a.writable() {
            return Err(not_writable());
        }
        page.require_unprotected(&a)?;
        let filename = lock_live(&mut *conn, id).await?;
        let x = begin(&mut *conn, who, request, hash, deadline).await?;
        mark_deleted(&mut *conn, id, x.exec).await?;
        let prov = crate::wiki::Prov::human(me, request, x.exec);
        crate::wiki::attachment_event(
            &mut *conn,
            page.id,
            "attachment_removed",
            event_payload(id, &filename),
            &prov,
        )
        .await?;
        return Ok((
            Done {
                effects: Vec::new(),
                item: item(o.op_id, None, Some(page.id), vec![id]),
            },
            x,
        ));
    }
    let issue_id = r.issue_id.expect("an attachment is on an issue or a page");
    let issue = match r.note_id {
        Some(note) => note_for_write(&mut *conn, caller, me, note)
            .await?
            .filter(|(i, _)| i.id == issue_id)
            .map(|(i, _)| i),
        None => issue_for_write(&mut *conn, caller, me, Some(issue_id), None).await?,
    }
    .ok_or_else(attachment_not_found)?;
    let pp = issue_perms(&mut *conn, caller, issue.project).await?;
    writable(&pp)?;
    // R-A7: edit_issues, or its author still holding the permission that
    // added it (add_issue_notes on a note, add_issues on the issue).
    let add = if r.note_id.is_some() {
        ADD_ISSUE_NOTES
    } else {
        ADD_ISSUES
    };
    let may = pp.attributes_editable(issue.tracker, issue.own)
        || (author && pp.writable() && pp.allowed(add, issue.tracker));
    if !may {
        return Err(no_permission(
            "deleting an attachment needs edit_issues, or being its author with the permission that added it",
        ));
    }
    let filename = lock_live(&mut *conn, id).await?;
    let x = begin(&mut *conn, who, request, hash, deadline).await?;
    mark_deleted(&mut *conn, id, x.exec).await?;
    issue_events(
        &mut *conn,
        issue.id,
        "attachment_removed",
        &[(id, filename.clone(), r.note_id)],
        me,
        request,
        x.exec,
        o.op_id,
    )
    .await?;
    let _ = r.filename;
    Ok((
        Done {
            effects: vec![issue_effect(&issue, "attachment_removed")],
            item: item(o.op_id, Some(issue.id), None, vec![id]),
        },
        x,
    ))
}

/// The attachment, locked, still not deleted (a concurrent delete waited
/// on the container's lock): its file name.
async fn lock_live(conn: &mut PgConnection, id: Uuid) -> Result<String> {
    sqlx::query_scalar(
        "SELECT filename FROM attachments WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(attachment_not_found)
}

async fn mark_deleted(conn: &mut PgConnection, id: Uuid, exec: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE attachments SET deleted_at = now(), deleted_by_execution_id = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(exec)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
