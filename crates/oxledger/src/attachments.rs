// SPDX-License-Identifier: GPL-2.0-or-later
//! Attachments (design 06 §2.4, §3.6 R-A1…R-A12; A2 (a)).
//!
//! Two steps (R-A1): a person uploads the raw bytes to a project
//! (`POST /projects/{id}/uploads?filename=`) and gets an `upload_id`, then a
//! command document attaches it. The upload is not a content write: no
//! history, no notification; unused for 24 hours it goes.
//!
//! The bytes are streamed to a temporary file while their SHA-256 is
//! computed (never the whole file in memory, R-A2), synced, then renamed to
//! `$OXLEDGER_FILES_DIR/<project_id>/<blob id first 2>/<blob id>` inside the
//! transaction that writes the blob row; a project that already has a
//! present generation of the same content keeps that one (R-A4). Without
//! `OXLEDGER_FILES_DIR` attachments are off (`forbidden_op`,
//! `attachments_disabled`).

use crate::auth::AuthContext;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, ADD_ISSUE_NOTES, ADD_ISSUES, EDIT_ISSUES, EDIT_WIKI_PAGES};
use crate::service::App;
use anyhow::{Context, Result};
use axum::body::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::AsyncWriteExt;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

pub mod gc;
pub mod ops;
pub mod read;
pub mod redact;
pub mod undo;

const MB: u64 = 1024 * 1024;
/// R-A2: the default and the largest allowed single-file limit.
pub const DEFAULT_MAX_BYTES: u64 = 20 * MB;
pub const MAX_MAX_BYTES: u64 = 100 * MB;
/// R-A2: bytes per person per day (the organization's time zone).
pub const DEFAULT_DAILY_BYTES: u64 = 1024 * MB;
/// R-A2: free space the files directory's disk keeps.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 5 * 1024 * MB;
/// R-A2: a person's unused uploads together.
pub const PENDING_LIMIT: u64 = 200 * MB;
/// R-A2: a person's uploads at the same time (per server process).
pub const CONCURRENT_UPLOADS: usize = 3;
/// R-A2: attachments in one command document.
pub const MAX_PER_DOCUMENT: usize = 10;
/// R-A1: an upload waits this long to be attached.
pub const UPLOAD_HOURS: i32 = 24;

/// Today in the organization's time zone (U42).
const ORG_DAY: &str = "(SELECT (now() AT TIME ZONE timezone)::date FROM org_settings)";

// ---------------------------------------------------------------- configuration

/// The attachment settings of this process (`OXLEDGER_FILES_DIR` and the
/// R-A2 limits).
#[derive(Debug, Clone)]
pub struct FilesConfig {
    /// `None`: attachments are off (R-A11).
    pub dir: Option<PathBuf>,
    pub max_bytes: u64,
    pub daily_bytes: u64,
    pub min_free_bytes: u64,
    /// R-A6: a separate download origin (signed redirects); `None` serves
    /// downloads from the API's own origin.
    pub origin: Option<String>,
    /// Tests (T-M2-59): the free space to report instead of asking the
    /// file system.
    pub free_space_override: Option<u64>,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_bytes: DEFAULT_MAX_BYTES,
            daily_bytes: DEFAULT_DAILY_BYTES,
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
            origin: None,
            free_space_override: None,
        }
    }
}

impl FilesConfig {
    /// On, with `dir` and the default limits.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
            ..Self::default()
        }
    }
}

/// Reads the settings from environment variables (`get` is `std::env::var`
/// in the server); returns warnings for values it could not use.
pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> (FilesConfig, Vec<String>) {
    let mut cfg = FilesConfig::default();
    let mut warnings = Vec::new();
    let mut bytes = |name: &str, default: u64, max: u64| -> u64 {
        match get(name).map(|v| v.trim().to_string()) {
            None => default,
            Some(v) if v.is_empty() => default,
            Some(v) => match v.parse::<u64>() {
                Ok(n) if n >= 1 && n <= max => n,
                Ok(n) if n > max => {
                    warnings.push(format!("{name}={n} is over {max}; using {max}"));
                    max
                }
                _ => {
                    warnings.push(format!(
                        "{name}={v:?} is not a positive number of bytes; using {default}"
                    ));
                    default
                }
            },
        }
    };
    cfg.max_bytes = bytes(
        "OXLEDGER_ATTACHMENT_MAX_BYTES",
        DEFAULT_MAX_BYTES,
        MAX_MAX_BYTES,
    );
    cfg.daily_bytes = bytes(
        "OXLEDGER_UPLOAD_DAILY_BYTES",
        DEFAULT_DAILY_BYTES,
        u64::MAX / 2,
    );
    cfg.min_free_bytes = bytes(
        "OXLEDGER_FILES_MIN_FREE_BYTES",
        DEFAULT_MIN_FREE_BYTES,
        u64::MAX / 2,
    );
    cfg.dir = get("OXLEDGER_FILES_DIR")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    cfg.origin = get("OXLEDGER_FILES_ORIGIN")
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty());
    if let Some(o) = &cfg.origin
        && !(o.starts_with("https://") || o.starts_with("http://"))
    {
        warnings.push(format!(
            "OXLEDGER_FILES_ORIGIN={o:?} is not an http(s) origin; ignored"
        ));
        cfg.origin = None;
    }
    (cfg, warnings)
}

static CURRENT: RwLock<Option<Arc<FilesConfig>>> = RwLock::new(None);

/// Installs the settings for this process (the server at start-up; tests
/// install their own).
pub fn install(cfg: FilesConfig) {
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(Arc::new(cfg));
    }
}

/// The installed settings (attachments off when none were installed).
pub fn current() -> Arc<FilesConfig> {
    CURRENT
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| Arc::new(FilesConfig::default()))
}

/// Whether attachments are on (R-A11): the web app hides the attachment
/// areas otherwise.
pub fn enabled() -> bool {
    current().dir.is_some()
}

// ---------------------------------------------------------------- errors

fn forbidden(reason: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

pub(crate) fn disabled() -> anyhow::Error {
    forbidden(
        "attachments_disabled",
        "attachments are not enabled on this server (OXLEDGER_FILES_DIR)",
    )
}

fn invalid_value(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn too_large(limit: u64, what: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::PayloadTooLarge,
        format!("{what} is over the limit"),
    )
    .with_details(json!({"limit": limit}))
    .into()
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such project")
}

// ---------------------------------------------------------------- content (R-A3)

/// R-A3: the type from the first bytes only: the four image types, else
/// `application/octet-stream` (SVG, HTML and PDF included). The name and
/// the request's `Content-Type` are never trusted.
pub fn sniff(head: &[u8]) -> &'static str {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "image/png"
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "image/gif"
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// Whether a content type is shown inline (R-A6: the four image types).
pub fn is_image(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// A file name as stored: NFC, trimmed, 1–255 characters, no control or
/// format characters (R-W2's set), no `/` or `\`. `invalid_value`
/// (`kind = filename`) otherwise.
pub fn normalize_filename(raw: &str, path: &str) -> Result<String> {
    let t: String = raw.nfc().collect();
    let t = t.trim().to_string();
    let n = t.chars().count();
    if n == 0 || n > 255 {
        return Err(invalid_value(
            path,
            "filename",
            "a file name is 1 to 255 characters",
        ));
    }
    if t.chars()
        .any(|c| c.is_control() || crate::wiki::is_format_char(c) || c == '/' || c == '\\')
    {
        return Err(invalid_value(
            path,
            "filename",
            "a file name may not hold control or format characters, '/' or '\\'",
        ));
    }
    Ok(t)
}

/// Where a blob generation's bytes live.
pub fn blob_path(dir: &Path, project: Uuid, blob: Uuid) -> PathBuf {
    let id = blob.simple().to_string();
    dir.join(project.simple().to_string())
        .join(&id[..2])
        .join(id)
}

fn temp_dir(dir: &Path) -> PathBuf {
    dir.join(".tmp")
}

// ---------------------------------------------------------------- limits (R-A2)

static IN_FLIGHT: Mutex<Option<HashMap<Uuid, usize>>> = Mutex::new(None);

/// One running upload of a person; dropping it frees the slot.
struct Slot(Uuid);

impl Slot {
    fn take(user: Uuid) -> Result<Self> {
        let mut g = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        let n = m.entry(user).or_insert(0);
        if *n >= CONCURRENT_UPLOADS {
            return Err(ApiError::new(
                ErrorCode::RateLimited,
                format!("at most {CONCURRENT_UPLOADS} uploads at the same time"),
            )
            .with_details(json!({"limit": CONCURRENT_UPLOADS}))
            .into());
        }
        *n += 1;
        Ok(Self(user))
    }
}

/// Tests (T-M2-59): takes one of `user`'s upload slots until the guard
/// is dropped.
#[cfg(feature = "test-support")]
pub fn hold_upload_slot(user: Uuid) -> Result<impl Drop> {
    Slot::take(user)
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut g = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = g.as_mut()
            && let Some(n) = m.get_mut(&self.0)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

async fn free_space(cfg: &FilesConfig, dir: &Path) -> Result<u64> {
    if let Some(n) = cfg.free_space_override {
        return Ok(n);
    }
    let d = dir.to_path_buf();
    tokio::task::spawn_blocking(move || fs4::available_space(&d))
        .await?
        .with_context(|| format!("free space of {}", dir.display()))
}

/// A person's unused, unexpired uploads together.
async fn pending_bytes(conn: &mut PgConnection, user: Uuid) -> Result<u64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(size), 0)::bigint FROM uploads
         WHERE uploader_id = $1 AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n.max(0) as u64)
}

async fn used_today(conn: &mut PgConnection, user: Uuid) -> Result<u64> {
    let n: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT bytes FROM upload_daily_usage WHERE user_id = $1 AND day = {ORG_DAY}"
    )))
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(n.unwrap_or(0).max(0) as u64)
}

// ---------------------------------------------------------------- who may upload (R-A1)

/// R-A1: a project the caller cannot see is `not_found`; one where they hold
/// none of the permissions that attach files (`add_issues`, `edit_issues`,
/// `add_issue_notes`, `edit_wiki_pages`, in a project taking writes) is
/// `forbidden_op`.
async fn may_upload(conn: &mut PgConnection, caller: AuthContext, project: &str) -> Result<Uuid> {
    let Some(id) = crate::wiki::project_id(&mut *conn, project).await? else {
        return Err(not_found());
    };
    let actor = perm::actor(&mut *conn, caller).await?;
    let issue = perm::project_perms(&mut *conn, &actor, id).await?;
    let wiki = crate::wiki::access_with(&mut *conn, &actor, id).await?;
    let issue_write = issue.as_ref().is_some_and(|p| {
        p.writable()
            && [ADD_ISSUES, EDIT_ISSUES, ADD_ISSUE_NOTES]
                .iter()
                .any(|x| p.allowed_any(x))
    });
    let wiki_write = wiki
        .as_ref()
        .is_some_and(|a| a.writable() && a.has(EDIT_WIKI_PAGES));
    if issue_write || wiki_write {
        return Ok(id);
    }
    // Sees the project: reads its wiki, or holds a role in it (a member's
    // roles, or the Non member role of a public project), not archived.
    let sees = wiki.is_some()
        || issue
            .as_ref()
            .is_some_and(|p| p.status != "archived" && !p.roles.is_empty());
    if !sees {
        return Err(not_found());
    }
    Err(forbidden(
        "permission",
        "uploading needs a permission that attaches files in this project",
    ))
}

// ---------------------------------------------------------------- upload

/// `POST /projects/{id}/uploads` (R-A1). The same shape whatever happened
/// to the content before (R-A12: never "this was redacted").
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct UploadView {
    pub upload_id: Uuid,
    pub filename: String,
    pub size: i64,
    /// R-A3: from the first bytes only.
    pub content_type: String,
    /// Attach it before this, or it goes (R-A1).
    pub expires_at: DateTime<Utc>,
}

/// A temporary file removed when dropped (an error, a cancelled request);
/// after the rename there is nothing at the path.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not remove {}: {e}", self.0.display());
        }
    }
}

/// Creates a directory (and its parents) only the server's account reads
/// (0700 on Unix).
async fn private_dir(p: &Path) -> Result<()> {
    let d = p.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&d)
    })
    .await?
    .with_context(|| format!("creating {}", p.display()))
}

/// Creates a new file only the server's account reads (0600 on Unix).
async fn private_file(p: &Path) -> std::io::Result<tokio::fs::File> {
    let mut o = tokio::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(p).await
}

/// Makes a rename durable (Unix: fsync the directory); best effort.
async fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(d) = dir {
        let d = d.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || std::fs::File::open(&d)?.sync_all()).await;
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// R-A10: the synchronous scan hook (after the upload, before the blob is
/// written). M2 has no scanner: it accepts everything (B-17).
fn scan(_file: &Path) -> Result<()> {
    Ok(())
}

/// Receives an upload: `caller` is a signed-in person (the API answers
/// anonymous callers 401 before this), `content_length` the request's
/// header when it has one. Checks every limit that does not need the body
/// first (a `Content-Length` over the limit is refused unread), then
/// streams the body to a temporary file.
pub async fn upload<S, E>(
    app: &App,
    caller: AuthContext,
    project: &str,
    filename: Option<&str>,
    content_length: Option<u64>,
    body: S,
) -> Result<UploadView>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let cfg = current();
    let Some(dir) = cfg.dir.clone() else {
        return Err(disabled());
    };
    let (user, _) = caller.require_human_session()?;
    let filename = normalize_filename(
        filename.ok_or_else(|| {
            ApiError::new(ErrorCode::InvalidRequest, "filename is required")
                .with_details(json!({"path": "filename"}))
        })?,
        "filename",
    )?;
    let project = {
        let mut conn = app.pool.acquire().await?;
        may_upload(&mut conn, caller, project).await?
    };
    // §3.1: Content-Length is required, so every limit is checked before
    // a byte is read.
    let Some(want) = content_length else {
        return Err(
            ApiError::new(ErrorCode::InvalidRequest, "Content-Length is required")
                .with_details(json!({"path": "Content-Length", "kind": "content_length_required"}))
                .into(),
        );
    };
    if want > cfg.max_bytes {
        return Err(too_large(cfg.max_bytes, "the file"));
    }
    let _slot = Slot::take(user)?;
    {
        let mut conn = app.pool.acquire().await?;
        if pending_bytes(&mut conn, user).await? + want > PENDING_LIMIT {
            return Err(too_large(PENDING_LIMIT, "your unused uploads"));
        }
        if used_today(&mut conn, user).await? + want > cfg.daily_bytes {
            return Err(too_large(cfg.daily_bytes, "today's uploads"));
        }
    }
    private_dir(&temp_dir(&dir)).await?;
    if free_space(&cfg, &dir).await? < cfg.min_free_bytes {
        return Err(ApiError::new(
            ErrorCode::InsufficientStorage,
            "the attachment storage is nearly full; try again later",
        )
        .into());
    }

    // Removed on every way out, a cancelled request included (once
    // renamed into place there is nothing left to remove).
    let tmp = TempFile(temp_dir(&dir).join(format!("{}.part", Uuid::now_v7().simple())));
    let (size, sha, head) = receive(&tmp.0, body, cfg.max_bytes, Some(want)).await?;
    store(
        app, &cfg, &dir, &tmp.0, user, project, &filename, size, &sha, &head,
    )
    .await
}

/// Streams the body to `tmp`, hashing as it goes; `(size, sha256, first
/// bytes)`. Over `max` is `payload_too_large`; more bytes than the
/// `Content-Length` said, an empty body or a broken stream is
/// `invalid_request`.
async fn receive<S, E>(
    tmp: &Path,
    mut body: S,
    max: u64,
    content_length: Option<u64>,
) -> Result<(u64, [u8; 32], Vec<u8>)>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut file = private_file(tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(16);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| {
            ApiError::new(
                ErrorCode::InvalidRequest,
                format!("the upload broke off: {e}"),
            )
            .with_details(json!({"path": "body"}))
        })?;
        size += chunk.len() as u64;
        if size > max {
            return Err(too_large(max, "the file"));
        }
        if let Some(n) = content_length
            && size > n
        {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                "the body is longer than its Content-Length",
            )
            .with_details(json!({"path": "body"}))
            .into());
        }
        if head.len() < 16 {
            let take = (16 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if size == 0 {
        return Err(invalid_value("body", "empty", "the file is empty"));
    }
    if let Some(n) = content_length
        && size != n
    {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "the body is shorter than its Content-Length",
        )
        .with_details(json!({"path": "body"}))
        .into());
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    Ok((size, hasher.finalize().into(), head))
}

/// Writes the blob (or reuses the project's present generation of the same
/// content), the upload and the day's usage in one transaction.
#[allow(clippy::too_many_arguments)]
async fn store(
    app: &App,
    cfg: &FilesConfig,
    dir: &Path,
    tmp: &Path,
    user: Uuid,
    project: Uuid,
    filename: &str,
    size: u64,
    sha: &[u8; 32],
    head: &[u8],
) -> Result<UploadView> {
    scan(tmp)?;
    let content_type = sniff(head);
    for _ in 0..3 {
        let mut tx = app.pool.begin().await?;
        // The project first, FOR SHARE: a redaction holds it FOR UPDATE and
        // then updates the generation this may hold FOR SHARE below (the
        // inserts' FK checks on projects would wait the other way).
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(project)
            .execute(&mut *tx)
            .await?;
        let used: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "INSERT INTO upload_daily_usage (user_id, day, bytes) VALUES ($1, {ORG_DAY}, $2)
             ON CONFLICT (user_id, day) DO UPDATE SET bytes = upload_daily_usage.bytes + EXCLUDED.bytes
             RETURNING bytes"
        )))
        .bind(user)
        .bind(size as i64)
        .fetch_one(&mut *tx)
        .await?;
        if used as u64 > cfg.daily_bytes {
            return Err(too_large(cfg.daily_bytes, "today's uploads"));
        }
        if pending_bytes(&mut tx, user).await? + size > PENDING_LIMIT {
            return Err(too_large(PENDING_LIMIT, "your unused uploads"));
        }
        let new_id = Uuid::now_v7();
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO attachment_blobs (id, project_id, sha256, size, sniffed_type)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (project_id, sha256) WHERE state = 'present' DO NOTHING
             RETURNING id",
        )
        .bind(new_id)
        .bind(project)
        .bind(&sha[..])
        .bind(size as i64)
        .bind(content_type)
        .fetch_optional(&mut *tx)
        .await?;
        let (blob, blob_type) = match inserted {
            Some(id) => {
                let dest = blob_path(dir, project, id);
                if let Some(parent) = dest.parent() {
                    private_dir(parent).await?;
                }
                tokio::fs::rename(tmp, &dest)
                    .await
                    .with_context(|| format!("moving the upload to {}", dest.display()))?;
                sync_dir(dest.parent()).await;
                (id, content_type.to_string())
            }
            None => {
                // The present generation, kept present until this commits
                // (the GC purges under FOR UPDATE).
                let found: Option<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, sniffed_type FROM attachment_blobs
                     WHERE project_id = $1 AND sha256 = $2 AND state = 'present' FOR SHARE",
                )
                .bind(project)
                .bind(&sha[..])
                .fetch_optional(&mut *tx)
                .await?;
                match found {
                    Some(f) => f,
                    // Purged in between: try again with a new generation.
                    None => continue,
                }
            }
        };
        let (id, expires_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO uploads (project_id, uploader_id, blob_id, filename, size, expires_at)
             VALUES ($1, $2, $3, $4, $5, now() + make_interval(hours => $6))
             RETURNING id, expires_at",
        )
        .bind(project)
        .bind(user)
        .bind(blob)
        .bind(filename)
        .bind(size as i64)
        .bind(UPLOAD_HOURS)
        .fetch_one(&mut *tx)
        .await?;
        if let Err(e) = tx.commit().await {
            // The renamed file (if any) is an orphan now; the daily scan
            // removes it (T-M2-57).
            return Err(e.into());
        }
        return Ok(UploadView {
            upload_id: id,
            filename: filename.to_string(),
            size: size as i64,
            content_type: blob_type,
            expires_at,
        });
    }
    Err(api(
        ErrorCode::TemporarilyUnavailable,
        "the upload could not be stored; try again",
    ))
}
