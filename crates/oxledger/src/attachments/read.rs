// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 R-A5, R-A6, R-A9: reading attachments. The metadata goes with
//! the container (an issue, a note, a wiki page); the content is for people
//! only, with the R-A6 headers, from this origin or (with
//! `OXLEDGER_FILES_ORIGIN`) through a signed redirect.

use super::{blob_path, disabled};
use crate::auth::AuthContext;
use crate::error::{ApiError, ErrorCode};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use ox_core::IssueFacts;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use sqlx::PgConnection;
use std::path::PathBuf;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::ops::attachment_not_found;

/// R-A6: how long a signed download link holds.
pub const SIGNED_SECONDS: i64 = 300;

/// One attachment as its readers see it (R-A9: agents too, through `get`
/// and `wiki_get`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub id: Uuid,
    /// `[已遮蔽]` once redacted.
    pub filename: String,
    pub size: i64,
    /// R-A3: one of the four image types, or `application/octet-stream`.
    pub content_type: String,
    /// Who attached it.
    pub author: String,
    pub created_at: DateTime<Utc>,
    /// The content was redacted (D12): it cannot be downloaded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    /// Web app only: whether the caller may delete it now (R-A7, R-W14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_delete: Option<bool>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct MetaRow {
    pub id: Uuid,
    pub note_id: Option<Uuid>,
    pub filename: String,
    pub size: i64,
    pub content_type: String,
    pub author: String,
    pub author_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub redacted: bool,
}

impl MetaRow {
    pub(crate) fn meta(&self) -> AttachmentMeta {
        AttachmentMeta {
            id: self.id,
            filename: self.filename.clone(),
            size: self.size,
            content_type: self.content_type.clone(),
            author: self.author.clone(),
            created_at: self.created_at,
            redacted: self.redacted,
            can_delete: None,
        }
    }
}

const META_COLS: &str = "a.id, a.note_id, a.filename, a.size, a.content_type, p.name AS author,
     a.author_id, a.created_at, a.redacted_at IS NOT NULL AS redacted";

/// The live attachments of an issue (its own and its notes'), oldest
/// first; the caller filters the notes it may read.
pub(crate) async fn of_issue(conn: &mut PgConnection, issue: Uuid) -> Result<Vec<MetaRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {META_COLS} FROM attachments a JOIN principals p ON p.id = a.author_id
         WHERE a.issue_id = $1 AND a.deleted_at IS NULL ORDER BY a.created_at, a.id"
    )))
    .bind(issue)
    .fetch_all(&mut *conn)
    .await?)
}

/// The live attachments of a wiki page, oldest first.
pub(crate) async fn of_page(conn: &mut PgConnection, page: Uuid) -> Result<Vec<MetaRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {META_COLS} FROM attachments a JOIN principals p ON p.id = a.author_id
         WHERE a.wiki_page_id = $1 AND a.deleted_at IS NULL ORDER BY a.created_at, a.id"
    )))
    .bind(page)
    .fetch_all(&mut *conn)
    .await?)
}

/// R-A7 for one issue attachment: edit_issues (or edit_own_issues on one's
/// own issue), or its author still holding the permission that added it.
pub(crate) fn issue_may_delete(
    pp: &crate::perm::ProjectPerms,
    tracker: Uuid,
    own_issue: bool,
    me: Option<Uuid>,
    row: &MetaRow,
) -> bool {
    let add = if row.note_id.is_some() {
        crate::perm::ADD_ISSUE_NOTES
    } else {
        crate::perm::ADD_ISSUES
    };
    pp.attributes_editable(tracker, own_issue)
        || (me == Some(row.author_id) && pp.writable() && pp.allowed(add, tracker))
}

// ---------------------------------------------------------------- one attachment

/// `GET /attachments/{id}`: the metadata and where it hangs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct AttachmentView {
    #[serde(flatten)]
    pub meta: AttachmentMeta,
    pub project_key: String,
    /// An issue's (or its note's) attachment: the issue's current key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_id: Option<Uuid>,
    /// A wiki page's attachment: the page, its title, and the page's
    /// version when it was attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki_page_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki_version: Option<i32>,
}

/// An attachment the caller may read (R-A5).
pub(crate) struct Readable {
    pub view: AttachmentView,
    pub project: Uuid,
    pub blob: Uuid,
    pub blob_present: bool,
}

#[derive(sqlx::FromRow)]
struct FullRow {
    #[sqlx(flatten)]
    meta: MetaRow,
    project_id: Uuid,
    project_key: String,
    issue_id: Option<Uuid>,
    wiki_page_id: Option<Uuid>,
    wiki_version: Option<i32>,
    blob_id: Uuid,
    blob_present: bool,
}

/// R-A9: the attachment routes are for people (agents and the built-in AI
/// read the metadata through their containers).
fn people_only() -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, "only people read attached files")
        .with_details(json!({"reason": "attachments_people_only"}))
        .into()
}

/// R-A5: readable when the container is: the issue (scope), the note too
/// (private notes: view_private_notes or its author; never retracted), the
/// wiki (R-W1; a deleted page's attachments are not). Missing, deleted and
/// unreadable are one `not_found`.
pub(crate) async fn readable(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
) -> Result<Readable> {
    if caller.context_id().is_some() {
        return Err(people_only());
    }
    // The scope first, whether or not the id exists (no timing oracle
    // between missing and unreadable).
    let scope = crate::db::load_scope(&mut *conn, caller).await?;
    let row: Option<FullRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {META_COLS}, a.project_id, pr.key AS project_key, a.issue_id, a.wiki_page_id,
                a.wiki_version, a.blob_id, b.state = 'present' AS blob_present
         FROM attachments a JOIN principals p ON p.id = a.author_id
         JOIN projects pr ON pr.id = a.project_id
         JOIN attachment_blobs b ON b.id = a.blob_id
         WHERE a.id = $1 AND a.deleted_at IS NULL"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Err(attachment_not_found());
    };
    let mut view = AttachmentView {
        meta: r.meta.meta(),
        project_key: r.project_key.clone(),
        issue_key: None,
        note_id: r.meta.note_id,
        wiki_page_id: r.wiki_page_id,
        wiki_title: None,
        wiki_version: r.wiki_version,
    };
    if let Some(issue) = r.issue_id {
        type IRow = (Uuid, bool, Uuid, Option<Uuid>, String);
        let i: Option<IRow> = sqlx::query_as(
            "SELECT i.tracker_id, i.is_private, i.author_id, i.assigned_to_id, k.key
             FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
             WHERE i.id = $1 AND i.archived_at IS NULL",
        )
        .bind(issue)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((tracker, private, author, assignee, key)) = i else {
            return Err(attachment_not_found());
        };
        let facts = IssueFacts {
            project_id: r.project_id,
            tracker_id: tracker,
            is_private: private,
            author_id: author,
            assigned_to_id: assignee,
        };
        if !scope.can_see(&facts) {
            return Err(attachment_not_found());
        }
        if let Some(note) = r.meta.note_id {
            let n: Option<(bool, Uuid, Option<Uuid>)> = sqlx::query_as(
                "SELECT is_private, author_id, created_on_behalf_of_id FROM issue_notes
                 WHERE id = $1 AND issue_id = $2 AND retracted_at IS NULL",
            )
            .bind(note)
            .bind(issue)
            .fetch_optional(&mut *conn)
            .await?;
            let Some((np, na, nb)) = n else {
                return Err(attachment_not_found());
            };
            let mut reader = crate::citations::Reader::new(&mut *conn, caller).await?;
            if !reader
                .note_visible(&mut *conn, r.project_id, tracker, np, na, nb)
                .await?
            {
                return Err(attachment_not_found());
            }
        }
        view.issue_key = Some(key);
    } else if let Some(page) = r.wiki_page_id {
        if crate::wiki::access_of(&mut *conn, caller, r.project_id)
            .await?
            .is_none()
        {
            return Err(attachment_not_found());
        }
        let title: Option<String> =
            sqlx::query_scalar("SELECT title FROM wiki_pages WHERE id = $1 AND deleted_at IS NULL")
                .bind(page)
                .fetch_optional(&mut *conn)
                .await?;
        let Some(title) = title else {
            return Err(attachment_not_found());
        };
        view.wiki_title = Some(title);
    }
    Ok(Readable {
        view,
        project: r.project_id,
        blob: r.blob_id,
        blob_present: r.blob_present,
    })
}

/// `GET /attachments/{id}`.
pub async fn get(app: &App, caller: AuthContext, id: Uuid) -> Result<AttachmentView> {
    let mut tx = read_tx(app).await?;
    let r = readable(&mut tx, caller, id).await?;
    tx.commit().await?;
    Ok(r.view)
}

/// What `GET /attachments/{id}/content` sends.
pub enum Download {
    /// The file, from this origin (R-A6 headers).
    File(FileBody),
    /// `OXLEDGER_FILES_ORIGIN`: a signed link there (302).
    Redirect(String),
}

pub struct FileBody {
    pub path: PathBuf,
    pub filename: String,
    pub content_type: String,
    pub size: i64,
}

/// The file of a readable, not redacted attachment whose content is still
/// present; redacted or purged content is the same `not_found` (T-M2-21a).
async fn file_of(conn: &mut PgConnection, caller: AuthContext, id: Uuid) -> Result<FileBody> {
    let cfg = super::current();
    let Some(dir) = cfg.dir.clone() else {
        return Err(disabled());
    };
    let r = readable(&mut *conn, caller, id).await?;
    if r.view.meta.redacted || !r.blob_present {
        return Err(attachment_not_found());
    }
    Ok(FileBody {
        path: blob_path(&dir, r.project, r.blob),
        filename: r.view.meta.filename,
        content_type: r.view.meta.content_type,
        size: r.view.meta.size,
    })
}

/// `GET /attachments/{id}/content` (R-A5, R-A6).
pub async fn content(app: &App, caller: AuthContext, id: Uuid) -> Result<Download> {
    let mut tx = read_tx(app).await?;
    let f = file_of(&mut tx, caller, id).await?;
    tx.commit().await?;
    let origin = super::current().origin.clone();
    // An anonymous reader's link is signed for the nil user and checked
    // again as anonymous (agents never get this far: `readable`).
    match (origin, caller.user_id().unwrap_or(Uuid::nil())) {
        (Some(origin), user) => {
            let exp = Utc::now().timestamp() + SIGNED_SECONDS;
            let sig = sign(app, id, user, exp).await?;
            Ok(Download::Redirect(format!(
                "{}/files/{id}?u={user}&e={exp}&s={sig}",
                origin.trim_end_matches('/')
            )))
        }
        (None, _) => Ok(Download::File(f)),
    }
}

/// The query of a signed link (`/files/{id}?u=&e=&s=`).
#[derive(Debug, Deserialize)]
pub struct Signed {
    pub u: Uuid,
    pub e: i64,
    pub s: String,
}

impl Signed {
    /// The link's parts, or `None` when any is missing or malformed (the
    /// route answers that with the same `not_found`).
    pub fn parse(q: &std::collections::HashMap<String, String>) -> Option<Self> {
        Some(Signed {
            u: q.get("u")?.parse().ok()?,
            e: q.get("e")?.parse().ok()?,
            s: q.get("s")?.clone(),
        })
    }
}

/// `GET /files/{id}` on the download origin: a link signed for this
/// attachment and person, not expired; read access is checked again as
/// that person. Anything wrong is `not_found`.
pub async fn signed_content(app: &App, id: Uuid, q: &Signed) -> Result<FileBody> {
    let key = key(app).await?;
    let ok = q.e >= Utc::now().timestamp()
        && hex::decode(&q.s).ok().is_some_and(|sig| {
            let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
            mac.update(message(id, q.u, q.e).as_bytes());
            mac.verify_slice(&sig).is_ok()
        });
    if !ok {
        return Err(attachment_not_found());
    }
    let mut tx = read_tx(app).await?;
    let reader = if q.u.is_nil() {
        AuthContext::anonymous()
    } else {
        AuthContext::notification_recipient(q.u)
    };
    let f = file_of(&mut tx, reader, id).await.map_err(|e| {
        // An inactive user, or any other refusal: the same not_found; a
        // database fault stays itself.
        if crate::error::classify(&e).is_some() {
            attachment_not_found()
        } else {
            e
        }
    })?;
    tx.commit().await?;
    Ok(f)
}

fn message(id: Uuid, user: Uuid, exp: i64) -> String {
    format!("attachment.{id}.{user}.{exp}")
}

async fn sign(app: &App, id: Uuid, user: Uuid, exp: i64) -> Result<String> {
    let key = key(app).await?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    mac.update(message(id, user, exp).as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

static KEY: OnceCell<[u8; 32]> = OnceCell::const_new();

/// The signing key (`server_secrets.files_key`), created on first use and
/// shared by every server process through the database.
async fn key(app: &App) -> Result<&'static [u8; 32]> {
    KEY.get_or_try_init(|| async {
        let fresh = crate::auth::random_secret();
        sqlx::query(
            "INSERT INTO server_secrets (name, secret) VALUES ('files_key', $1)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&fresh[..])
        .execute(&app.pool)
        .await?;
        let k: Vec<u8> =
            sqlx::query_scalar("SELECT secret FROM server_secrets WHERE name = 'files_key'")
                .fetch_one(&app.pool)
                .await?;
        let k: [u8; 32] = k
            .try_into()
            .map_err(|_| anyhow::anyhow!("files_key is not 32 bytes"))?;
        anyhow::Ok(k)
    })
    .await
}
