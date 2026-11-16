// SPDX-License-Identifier: GPL-2.0-or-later
//! The project wiki (design 06 §2.3, §3.5, §3.11, §3.13; M2.2).
//!
//! * Readers (R-W1): the project is not archived, has the `wiki` module and
//!   the caller holds `view_wiki_pages` (or is a system administrator).
//!   Everyone else gets the same `not_found` as for a missing project, for
//!   every read and every write (the user's decision of 2026-11-15: editing
//!   presupposes reading). A closed project's wiki is read only.
//! * Writes are command documents with exactly one wiki op ([`run`], from
//!   `POST /commands`), people only in M2.2: `wiki_put`, `wiki_move`,
//!   `wiki_delete`, `wiki_restore`, `wiki_protect`. Lock order (R-W10):
//!   operation request → project `FOR SHARE` → `wiki_settings` `FOR UPDATE`
//!   (tree changes: create, move, delete, restore, a `wiki_put` with `title`
//!   or `parent`) → the page `FOR UPDATE` → the new parent chain by id →
//!   the children by id. D12 takes the project row `FOR UPDATE`, so it runs
//!   wholly before or after any wiki write.
//! * Every write is a `human_direct` execution with one
//!   `execution_wiki_changes` row per page it changed (undo, R-W9:
//!   [`undo_preview`], [`undo_apply`]).
//! * Reads: the index, a page (following redirects), versions, one
//!   version, a diff (R-W7, [`diff`]), events, deleted pages (R-W16) and the
//!   settings (R-W13).

// Row tuples straight from sqlx.
#![allow(clippy::type_complexity)]

use crate::auth::AuthContext;
use crate::commands::{self, CommandResult, Executor, ItemResult, SearchReceipt};
use crate::error::{ApiError, ErrorCode, api};
use crate::markdown::{self, Source, Text};
use crate::perm::{
    self, DELETE_WIKI_PAGES, EDIT_WIKI_PAGES, MANAGE_WIKI, PROTECT_WIKI_PAGES, RENAME_WIKI_PAGES,
    VIEW_WIKI_EDITS, VIEW_WIKI_PAGES,
};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

/// `enabled_modules.module` of the wiki.
pub const WIKI_MODULE: &str = "wiki";
/// The wiki op kinds (design 06 §3.11).
pub const KINDS: [&str; 5] = [
    "wiki_put",
    "wiki_move",
    "wiki_delete",
    "wiki_restore",
    "wiki_protect",
];
/// R-W15.
pub const MAX_PAGES: i32 = 5000;
pub const MAX_VERSIONS: i32 = 10_000;
pub const MAX_BODY_BYTES: usize = 512 * 1024;
/// R-W4: a top-level page has depth 1.
pub const MAX_DEPTH: i32 = 10;
/// R-W2.
pub const MAX_TITLE_CHARS: usize = 100;
pub const MAX_COMMENT_CHARS: usize = 255;
/// R-W7: changed lines a diff returns.
pub const DIFF_MAX_LINES: usize = 2000;
/// R-W16, U18.
pub const DELETED_DAYS: i64 = 30;
pub const VERSIONS_PAGE: i64 = 50;
pub const ACTION_SETTINGS: &str = "wiki_settings";
/// R-W13: the start page when none is set.
pub const DEFAULT_START_TITLE: &str = "Wiki";
const NOT_FOUND: &str = "no such wiki page";
/// R-W2: characters a title may not hold (besides Cc and Cf).
const FORBIDDEN: &[char] = &['[', ']', '|', '#', '?', '/', '\\', ':', '<', '>', '"'];

pub(crate) fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, NOT_FOUND)
}

fn forbidden(reason: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

fn invalid_value(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

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

fn page_exists(page: Uuid) -> anyhow::Error {
    ApiError::new(
        ErrorCode::WikiPageExists,
        "a page of this wiki already has that title",
    )
    .with_details(json!({"page_id": page}))
    .into()
}

// ---------------------------------------------------------------- titles (R-W2)

/// Unicode general category Cf (format characters, Unicode 15.1): bidi
/// controls, zero width characters, tags and the like.
pub fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2
        | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Why a title is refused: `length`, `leading_dot` or `char` (with it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleError {
    Length,
    LeadingDot,
    Char(char),
}

/// R-W2: NFC, trimmed, 1–100 characters, none of `[ ] | # ? / \ : < > "`,
/// no Cc or Cf, not starting with `.`. The key is the database's
/// `wiki_title_key()` (runs of white space and `_` → one `_`,
/// NFKC_Casefold, C13): `a b`, `a_b` and `a__b` are one title.
pub fn normalize_title(raw: &str) -> std::result::Result<String, TitleError> {
    let t: String = raw.nfc().collect();
    let t = t.trim().to_string();
    if let Some(c) = t
        .chars()
        .find(|c| FORBIDDEN.contains(c) || c.is_control() || is_format_char(*c))
    {
        return Err(TitleError::Char(c));
    }
    let n = t.chars().count();
    if n == 0 || n > MAX_TITLE_CHARS {
        return Err(TitleError::Length);
    }
    if t.starts_with('.') {
        return Err(TitleError::LeadingDot);
    }
    Ok(t)
}

fn title_error(path: &str, e: TitleError) -> anyhow::Error {
    let (msg, details) = match e {
        TitleError::Length => (
            "a title is 1–100 characters".to_string(),
            json!({"reason": "length", "path": path}),
        ),
        TitleError::LeadingDot => (
            "a title may not start with '.'".to_string(),
            json!({"reason": "leading_dot", "path": path}),
        ),
        TitleError::Char(c) => (
            format!("a title may not contain U+{:04X}", c as u32),
            json!({"reason": "char", "char": c.to_string(),
                   "codepoint": format!("U+{:04X}", c as u32), "path": path}),
        ),
    };
    ApiError::new(ErrorCode::WikiTitleInvalid, msg)
        .with_details(details)
        .into()
}

fn checked_title(path: &str, raw: &str) -> Result<String> {
    normalize_title(raw).map_err(|e| title_error(path, e))
}

/// A title as it goes in a URL path (R-W2): spaces become `_`, the rest
/// percent-encoded except unreserved characters and non-ASCII.
pub fn url_title(title: &str) -> String {
    let mut o = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            ' ' => o.push('_'),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => o.push(c),
            c if !c.is_ascii() && !c.is_whitespace() => {
                let mut b = [0u8; 4];
                for x in c.encode_utf8(&mut b).bytes() {
                    o.push_str(&format!("%{x:02X}"));
                }
            }
            c => {
                let mut b = [0u8; 4];
                for x in c.encode_utf8(&mut b).bytes() {
                    o.push_str(&format!("%{x:02X}"));
                }
            }
        }
    }
    o
}

/// The title a URL path names: `_` is a space.
pub fn path_title(p: &str) -> String {
    p.replace('_', " ")
}

fn check_comment(path: &str, c: &str) -> Result<()> {
    if c.chars().count() > MAX_COMMENT_CHARS || c.chars().any(|c| c.is_control()) {
        return Err(invalid_value(
            path,
            "wiki_comment",
            "a comment is one line of at most 255 characters",
        ));
    }
    Ok(())
}

fn check_body(path: &str, b: &str) -> Result<()> {
    if b.len() > MAX_BODY_BYTES {
        return Err(ApiError::new(
            ErrorCode::PayloadTooLarge,
            format!("a wiki page is at most {MAX_BODY_BYTES} bytes"),
        )
        .with_details(json!({"limit": MAX_BODY_BYTES, "path": path}))
        .into());
    }
    if b.contains('\0') {
        return Err(invalid_value(path, "wiki_body", "the text holds a NUL"));
    }
    Ok(())
}

// ---------------------------------------------------------------- access (R-W1)

/// The caller's view of one project's wiki (they can read it).
#[derive(Debug, Clone)]
pub(crate) struct Access {
    pub project: Uuid,
    pub key: String,
    pub name: String,
    /// `active` or `closed`.
    pub status: String,
    pub admin: bool,
    perms: BTreeSet<String>,
}

impl Access {
    pub fn has(&self, p: &str) -> bool {
        self.admin || self.perms.contains(p)
    }

    /// Writes need an active project (closed: read only).
    pub fn writable(&self) -> bool {
        self.status == "active"
    }

    fn may(&self, p: &str) -> bool {
        self.writable() && self.has(p)
    }
}

/// A project id or key → its id, if any.
pub(crate) async fn project_id(conn: &mut PgConnection, text: &str) -> Result<Option<Uuid>> {
    let t = text.trim();
    Ok(match Uuid::parse_str(t) {
        Ok(id) => {
            sqlx::query_scalar("SELECT id FROM projects WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?
        }
        Err(_) => {
            if t.is_empty() || t.len() > 10 || !t.chars().all(|c| c.is_ascii_alphanumeric()) {
                None
            } else {
                sqlx::query_scalar("SELECT id FROM projects WHERE key = $1")
                    .bind(t.to_uppercase())
                    .fetch_optional(&mut *conn)
                    .await?
            }
        }
    })
}

/// R-W1: the caller's access to `project`'s wiki, `None` when they cannot
/// read it (missing, archived, no module, no `view_wiki_pages`).
pub(crate) async fn access_of(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<Option<Access>> {
    let row: Option<(String, String, String, bool)> = sqlx::query_as(
        "SELECT p.key, p.name, p.status,
                EXISTS (SELECT 1 FROM enabled_modules m WHERE m.project_id = p.id AND m.module = $2)
         FROM projects p WHERE p.id = $1",
    )
    .bind(project)
    .bind(WIKI_MODULE)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((key, name, status, module)) = row else {
        return Ok(None);
    };
    if status == "archived" || !module {
        return Ok(None);
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    let Some(pp) = perm::project_perms(&mut *conn, &actor, project).await? else {
        return Ok(None);
    };
    let mut perms = BTreeSet::new();
    for r in &pp.roles {
        for p in [
            VIEW_WIKI_PAGES,
            VIEW_WIKI_EDITS,
            EDIT_WIKI_PAGES,
            RENAME_WIKI_PAGES,
            DELETE_WIKI_PAGES,
            PROTECT_WIKI_PAGES,
            MANAGE_WIKI,
        ] {
            if r.perms.contains_key(p) {
                perms.insert(p.to_string());
            }
        }
    }
    let a = Access {
        project,
        key,
        name,
        status,
        admin: pp.admin,
        perms,
    };
    Ok(a.has(VIEW_WIKI_PAGES).then_some(a))
}

/// [`access_of`] for a project id or key; `not_found` when unreadable.
pub(crate) async fn access(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
) -> Result<Access> {
    let Some(id) = project_id(&mut *conn, project).await? else {
        return Err(not_found());
    };
    access_of(&mut *conn, caller, id)
        .await?
        .ok_or_else(not_found)
}

// ---------------------------------------------------------------- pages

#[derive(Debug, Clone, sqlx::FromRow)]
struct PageRow {
    id: Uuid,
    project_id: Uuid,
    title: String,
    parent_id: Option<Uuid>,
    current_version: i32,
    lock_version: i64,
    protected: bool,
    deleted_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

const PAGE_COLS: &str =
    "id, project_id, title, parent_id, current_version, lock_version, protected,
                         deleted_at, updated_at";

async fn live_by_title(
    conn: &mut PgConnection,
    project: Uuid,
    title: &str,
) -> Result<Option<PageRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PAGE_COLS} FROM wiki_pages
         WHERE project_id = $1 AND title_key = wiki_title_key($2) AND deleted_at IS NULL"
    )))
    .bind(project)
    .bind(title)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn page_by_id(conn: &mut PgConnection, project: Uuid, id: Uuid) -> Result<Option<PageRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PAGE_COLS} FROM wiki_pages WHERE project_id = $1 AND id = $2"
    )))
    .bind(project)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn lock_page(conn: &mut PgConnection, id: Uuid) -> Result<PageRow> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PAGE_COLS} FROM wiki_pages WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

/// A live page by title or by id; redirects are not followed (writes).
async fn resolve(
    conn: &mut PgConnection,
    project: Uuid,
    title: Option<&str>,
    id: Option<Uuid>,
) -> Result<Option<PageRow>> {
    match (title, id) {
        (Some(t), None) => live_by_title(&mut *conn, project, t).await,
        (None, Some(i)) => Ok(page_by_id(&mut *conn, project, i)
            .await?
            .filter(|p| p.deleted_at.is_none())),
        _ => Ok(None),
    }
}

/// Ancestors of `page`, nearest first (the page itself not included).
async fn ancestors(conn: &mut PgConnection, page: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE up(id, parent_id, n) AS (
             SELECT id, parent_id, 0 FROM wiki_pages WHERE id = $1
             UNION ALL
             SELECT p.id, p.parent_id, up.n + 1 FROM wiki_pages p JOIN up ON p.id = up.parent_id
             WHERE up.n < 64)
         SELECT id FROM up WHERE n > 0 ORDER BY n",
    )
    .bind(page)
    .fetch_all(&mut *conn)
    .await?)
}

/// Levels of the live subtree under and including `page` (1 for a leaf).
async fn height(conn: &mut PgConnection, page: Uuid) -> Result<i32> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE down(id, n) AS (
             SELECT id, 1 FROM wiki_pages WHERE id = $1
             UNION ALL
             SELECT p.id, down.n + 1 FROM wiki_pages p JOIN down ON p.parent_id = down.id
             WHERE p.deleted_at IS NULL AND down.n < 64)
         SELECT max(n) FROM down",
    )
    .bind(page)
    .fetch_one(&mut *conn)
    .await?)
}

async fn live_children(conn: &mut PgConnection, page: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM wiki_pages WHERE parent_id = $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(page)
    .fetch_all(&mut *conn)
    .await?)
}

/// R-W4: `page` under `parent` (None: top level) makes no cycle and stays
/// within [`MAX_DEPTH`]; the parent chain is locked by id (R-W10).
async fn check_parent(
    conn: &mut PgConnection,
    path: &str,
    page: Option<Uuid>,
    parent: Option<Uuid>,
) -> Result<()> {
    let Some(parent) = parent else {
        return Ok(());
    };
    let mut chain = ancestors(&mut *conn, parent).await?;
    chain.push(parent);
    if let Some(p) = page
        && chain.contains(&p)
    {
        return Err(invalid_value(
            path,
            "wiki_cycle",
            "a page cannot move under itself or one of its children",
        ));
    }
    let mut sorted = chain.clone();
    sorted.sort();
    sqlx::query("SELECT 1 FROM wiki_pages WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&sorted)
        .execute(&mut *conn)
        .await?;
    let below = match page {
        Some(p) => height(&mut *conn, p).await?,
        None => 1,
    };
    if chain.len() as i32 + below > MAX_DEPTH {
        return Err(invalid_value(
            path,
            "wiki_depth",
            format!("pages nest at most {MAX_DEPTH} levels"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- command input (§3.11)

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// `{"project": id or key, "page": title}` or `{"project", "page_id"}`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiTarget {
    /// A project id or key.
    pub project: String,
    /// The page's title (a new title for a new page). Redirects are not
    /// followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<Uuid>,
}

/// A parent page: `{"page": title}` or `{"page_id": id}`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiPageRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiPutOp {
    pub op_id: Uuid,
    pub target: WikiTarget,
    /// Markdown, at most 524288 bytes (`payload_too_large`). Exactly one of
    /// `body` and `revert_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// R-W8 「回復到第 N 版」: the body is version N of this page, read in
    /// the write's transaction (serialized with D12); needs view_wiki_edits
    /// besides edit_wiki_pages (`forbidden_op`). An existing page only; a redacted version is
    /// `invalid_value` (`kind = redacted_source`), a version the page does
    /// not have `invalid_argument`. The comment defaults to 「回復到第 N 版」.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert_from: Option<i32>,
    /// At most 255 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Rename too (an existing page; `rename_wiki_pages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The parent; `null` is the top level, absent keeps it.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent: Option<Option<WikiPageRef>>,
    /// Attachments come with M2.4: any value is `invalid_argument`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Json>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiMoveOp {
    pub op_id: Uuid,
    pub target: WikiTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The new parent; `null` is the top level, absent keeps it.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent: Option<Option<WikiPageRef>>,
    /// Keep the old title as a redirect (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_redirect: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiDeleteOp {
    pub op_id: Uuid,
    pub target: WikiTarget,
    /// `"reparent"`: the children move to the deleted page's parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiRestoreOp {
    pub op_id: Uuid,
    /// `{"project", "page_id"}` of a page deleted within 30 days.
    pub target: WikiTarget,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiProtectOp {
    pub op_id: Uuid,
    pub target: WikiTarget,
    pub protected: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WikiOp {
    WikiPut(WikiPutOp),
    WikiMove(WikiMoveOp),
    WikiDelete(WikiDeleteOp),
    WikiRestore(WikiRestoreOp),
    WikiProtect(WikiProtectOp),
}

impl WikiOp {
    fn target(&self) -> &WikiTarget {
        match self {
            WikiOp::WikiPut(o) => &o.target,
            WikiOp::WikiMove(o) => &o.target,
            WikiOp::WikiDelete(o) => &o.target,
            WikiOp::WikiRestore(o) => &o.target,
            WikiOp::WikiProtect(o) => &o.target,
        }
    }

    fn op_id(&self) -> Uuid {
        match self {
            WikiOp::WikiPut(o) => o.op_id,
            WikiOp::WikiMove(o) => o.op_id,
            WikiOp::WikiDelete(o) => o.op_id,
            WikiOp::WikiRestore(o) => o.op_id,
            WikiOp::WikiProtect(o) => o.op_id,
        }
    }
}

/// `{"wiki_page": page_id, "lock_version": n}`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiPrecondition {
    pub wiki_page: Uuid,
    pub lock_version: i64,
}

/// A wiki command document: exactly one wiki op (design 06 §3.11).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiCommandDoc {
    /// Always 1.
    pub command_schema_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Exactly one wiki op.
    pub ops: Vec<WikiOp>,
    /// An update (`wiki_put` of an existing page) carries the page's
    /// `lock_version`; optional for the other ops; a new page takes none.
    #[serde(default)]
    pub preconditions: Vec<WikiPrecondition>,
}

/// The body of `POST /commands`: an issue command document or a wiki one
/// (OpenAPI only; the engine tells them apart by the op kinds).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum CommandBody {
    Issue(commands::CommandDoc),
    Wiki(WikiCommandDoc),
}

/// Whether `raw` is a wiki document (any op kind is a wiki kind).
pub(crate) fn is_wiki_doc(raw: &Json) -> bool {
    raw.get("ops").and_then(Json::as_array).is_some_and(|ops| {
        ops.iter().any(|o| {
            o.get("kind")
                .and_then(Json::as_str)
                .is_some_and(|k| KINDS.contains(&k))
        })
    })
}

fn parse(raw: &Json) -> Result<WikiCommandDoc> {
    let n = raw
        .get("ops")
        .and_then(Json::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if n != 1 {
        return Err(bad("ops", "a wiki document has exactly one op"));
    }
    let doc: WikiCommandDoc = serde_json::from_value(raw.clone())
        .map_err(|e| bad("", format!("not a valid wiki command document: {e}")))?;
    if doc.command_schema_version != 1 {
        return Err(bad(
            "command_schema_version",
            "command_schema_version must be 1",
        ));
    }
    if doc.title.as_ref().is_some_and(|t| t.chars().count() > 255) {
        return Err(bad("title", "title is at most 255 characters"));
    }
    if doc.preconditions.len() > 1 {
        return Err(bad(
            "preconditions",
            "a wiki document has one precondition at most",
        ));
    }
    let t = doc.ops[0].target();
    if t.page.is_some() == t.page_id.is_some() {
        return Err(bad(
            "ops[0].target",
            "target names the page by exactly one of page and page_id",
        ));
    }
    match &doc.ops[0] {
        WikiOp::WikiPut(o) => {
            if o.attachments.is_some() {
                return Err(bad_argument(
                    "ops[0].attachments",
                    "wiki attachments are not available yet",
                ));
            }
            if o.body.is_some() == o.revert_from.is_some() {
                return Err(bad(
                    "ops[0].body",
                    "wiki_put carries exactly one of body and revert_from",
                ));
            }
            check_ref("ops[0].parent", &o.parent)?;
        }
        WikiOp::WikiMove(o) => {
            if o.title.is_none() && o.parent.is_none() {
                return Err(bad("ops[0]", "wiki_move changes title and/or parent"));
            }
            check_ref("ops[0].parent", &o.parent)?;
        }
        WikiOp::WikiDelete(o) => {
            if o.children.as_deref().is_some_and(|c| c != "reparent") {
                return Err(bad("ops[0].children", "children is \"reparent\""));
            }
        }
        WikiOp::WikiRestore(o) => {
            if o.target.page_id.is_none() {
                return Err(bad(
                    "ops[0].target",
                    "wiki_restore names the page by page_id",
                ));
            }
        }
        WikiOp::WikiProtect(_) => {}
    }
    Ok(doc)
}

fn check_ref(path: &str, r: &Option<Option<WikiPageRef>>) -> Result<()> {
    if let Some(Some(r)) = r
        && r.page.is_some() == r.page_id.is_some()
    {
        return Err(bad(
            path,
            "a page reference is exactly one of page and page_id",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- command output

/// What a wiki command did to one page (`CommandResult.wiki`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiEffect {
    pub page_id: Uuid,
    pub project_id: Uuid,
    /// The title now.
    pub title: String,
    pub version: i32,
    pub lock_version: i64,
    pub changed: bool,
    pub created: bool,
    /// Children moved to the deleted page's parent (`wiki_delete`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reparented: Vec<Uuid>,
}

// ---------------------------------------------------------------- writing

/// Who writes (a person's `human_direct` execution).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prov {
    pub user: Uuid,
    pub request: Uuid,
    pub exec: Uuid,
}

async fn insert_version(
    conn: &mut PgConnection,
    page: Uuid,
    version: i32,
    body: &str,
    comment: &str,
    at: &Prov,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO wiki_page_versions (page_id, version, body, comment, origin, actor_id, actor_kind,
                                         operation_request_id, command_execution_id)
         VALUES ($1, $2, $3, $4, 'human_direct', $5, 'user', $6, $7)",
    )
    .bind(page)
    .bind(version)
    .bind(body)
    .bind(comment)
    .bind(at.user)
    .bind(at.request)
    .bind(at.exec)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn insert_event(
    conn: &mut PgConnection,
    page: Uuid,
    kind: &str,
    payload: Json,
    at: &Prov,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO wiki_events (page_id, seq, kind, payload, origin, actor_id, actor_kind,
                                  operation_request_id, command_execution_id)
         VALUES ($1, (SELECT COALESCE(max(seq), 0) + 1 FROM wiki_events WHERE page_id = $1),
                 $2, $3, 'human_direct', $4, 'user', $5, $6)",
    )
    .bind(page)
    .bind(kind)
    .bind(payload)
    .bind(at.user)
    .bind(at.request)
    .bind(at.exec)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One `execution_wiki_changes` row (see migration 0034).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub(crate) struct Change {
    pub execution_id: Uuid,
    pub page_id: Uuid,
    pub before_version: Option<i32>,
    pub after_version: i32,
    pub before_title: Option<String>,
    pub after_title: Option<String>,
    pub parent_changed: bool,
    pub before_parent: Option<Uuid>,
    pub after_parent: Option<Uuid>,
    pub before_deleted: Option<bool>,
    pub after_deleted: bool,
    pub before_protected: Option<bool>,
    pub after_protected: bool,
    pub before_lock_version: Option<i64>,
    pub after_lock_version: i64,
    pub reverted_by: Option<Uuid>,
}

impl Change {
    /// A row for an existing page from its state before (`b`).
    fn from_before(exec: Uuid, b: &PageRow) -> Change {
        Change {
            execution_id: exec,
            page_id: b.id,
            before_version: Some(b.current_version),
            after_version: b.current_version,
            before_deleted: Some(b.deleted_at.is_some()),
            after_deleted: b.deleted_at.is_some(),
            before_protected: Some(b.protected),
            after_protected: b.protected,
            before_lock_version: Some(b.lock_version),
            after_lock_version: b.lock_version,
            before_parent: b.parent_id,
            after_parent: b.parent_id,
            ..Change::default()
        }
    }

    /// Fills the "after" side from the page as written (`a`).
    fn after(mut self, a: &PageRow, title_before: &str) -> Change {
        self.after_version = a.current_version;
        self.after_deleted = a.deleted_at.is_some();
        self.after_protected = a.protected;
        self.after_lock_version = a.lock_version;
        self.parent_changed = self.before_parent != a.parent_id;
        self.after_parent = a.parent_id;
        if !self.parent_changed {
            self.before_parent = None;
            self.after_parent = None;
        }
        if a.title != title_before {
            self.before_title = Some(title_before.to_string());
            self.after_title = Some(a.title.clone());
        }
        self
    }
}

async fn insert_change(conn: &mut PgConnection, c: &Change) -> Result<()> {
    sqlx::query(
        "INSERT INTO execution_wiki_changes
             (execution_id, page_id, before_version, after_version, before_title, after_title,
              parent_changed, before_parent, after_parent, before_deleted, after_deleted,
              before_protected, after_protected, before_lock_version, after_lock_version)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(c.execution_id)
    .bind(c.page_id)
    .bind(c.before_version)
    .bind(c.after_version)
    .bind(&c.before_title)
    .bind(&c.after_title)
    .bind(c.parent_changed)
    .bind(c.before_parent)
    .bind(c.after_parent)
    .bind(c.before_deleted)
    .bind(c.after_deleted)
    .bind(c.before_protected)
    .bind(c.after_protected)
    .bind(c.before_lock_version)
    .bind(c.after_lock_version)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The project's wiki settings row `FOR UPDATE` (R-W10): (page_count,
/// start_page_id).
async fn lock_settings(conn: &mut PgConnection, project: Uuid) -> Result<(i32, Option<Uuid>)> {
    let r = sqlx::query_as(
        "SELECT page_count, start_page_id FROM wiki_settings WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    fault("wiki.after_settings").await?;
    Ok(r)
}

/// A rename's title change: the new title takes over its key (a redirect
/// with that key goes), the old key redirects here when `keep`.
async fn retitle(
    conn: &mut PgConnection,
    page: &PageRow,
    new_title: &str,
    keep: bool,
) -> Result<()> {
    let mut sp = conn.begin().await?;
    let r = sqlx::query(
        "UPDATE wiki_pages SET title = $2, title_key = wiki_title_key($2) WHERE id = $1",
    )
    .bind(page.id)
    .bind(new_title)
    .execute(&mut *sp)
    .await;
    if let Err(e) = r {
        drop(sp);
        if unique_violation(&e) {
            let other = live_by_title(&mut *conn, page.project_id, new_title).await?;
            return Err(page_exists(other.map(|p| p.id).unwrap_or_default()));
        }
        return Err(e.into());
    }
    sp.commit().await?;
    sqlx::query(
        "DELETE FROM wiki_redirects WHERE project_id = $1 AND title_key = wiki_title_key($2)",
    )
    .bind(page.project_id)
    .bind(new_title)
    .execute(&mut *conn)
    .await?;
    if keep {
        sqlx::query(
            "INSERT INTO wiki_redirects (project_id, title_key, page_id)
             SELECT $1, wiki_title_key($2), $3 WHERE wiki_title_key($2) <> wiki_title_key($4)
             ON CONFLICT (project_id, title_key) DO UPDATE SET page_id = EXCLUDED.page_id, created_at = now()",
        )
        .bind(page.project_id)
        .bind(&page.title)
        .bind(page.id)
        .bind(new_title)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

/// Soft-deletes `page` (already locked): redirects to it go, the start
/// page falls back (R-W5), the page count goes down.
async fn soft_delete(conn: &mut PgConnection, page: Uuid, project: Uuid, exec: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE wiki_pages SET deleted_at = now(), deleted_by_execution_id = $2,
                lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(page)
    .bind(exec)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM wiki_redirects WHERE page_id = $1")
        .bind(page)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE wiki_settings SET page_count = page_count - 1,
                start_page_id = CASE WHEN start_page_id = $2 THEN NULL ELSE start_page_id END,
                lock_version = lock_version + CASE WHEN start_page_id = $2 THEN 1 ELSE 0 END
         WHERE project_id = $1",
    )
    .bind(project)
    .bind(page)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Brings back a deleted page (locked): its title must be free (savepoint:
/// a unique violation is `wiki_page_exists`, never a 500).
async fn undelete(conn: &mut PgConnection, page: &PageRow, parent: Option<Uuid>) -> Result<()> {
    let mut sp = conn.begin().await?;
    let r = sqlx::query(
        "UPDATE wiki_pages SET deleted_at = NULL, deleted_by_execution_id = NULL, parent_id = $2,
                lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(page.id)
    .bind(parent)
    .execute(&mut *sp)
    .await;
    if let Err(e) = r {
        drop(sp);
        if unique_violation(&e) {
            let other = live_by_title(&mut *conn, page.project_id, &page.title).await?;
            return Err(page_exists(other.map(|p| p.id).unwrap_or_default()));
        }
        return Err(e.into());
    }
    sp.commit().await?;
    sqlx::query(
        "DELETE FROM wiki_redirects WHERE project_id = $1 AND title_key = wiki_title_key($2)",
    )
    .bind(page.project_id)
    .bind(&page.title)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE wiki_settings SET page_count = page_count + 1 WHERE project_id = $1")
        .bind(page.project_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The restore parent: the old one while it is live and the depth allows.
async fn restore_parent(conn: &mut PgConnection, page: &PageRow) -> Result<Option<Uuid>> {
    let Some(p) = page.parent_id else {
        return Ok(None);
    };
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM wiki_pages WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(p)
    .fetch_one(&mut *conn)
    .await?;
    if !live {
        return Ok(None);
    }
    let depth = ancestors(&mut *conn, p).await?.len() as i32 + 1;
    if depth + height(&mut *conn, page.id).await? > MAX_DEPTH {
        return Ok(None);
    }
    Ok(Some(p))
}

async fn set_parent(conn: &mut PgConnection, page: Uuid, parent: Option<Uuid>) -> Result<()> {
    sqlx::query(
        "UPDATE wiki_pages SET parent_id = $2, lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(page)
    .bind(parent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn version_body(
    conn: &mut PgConnection,
    page: Uuid,
    v: i32,
) -> Result<Option<(String, bool)>> {
    Ok(sqlx::query_as(
        "SELECT body, redaction_id IS NOT NULL FROM wiki_page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(page)
    .bind(v)
    .fetch_optional(&mut *conn)
    .await?)
}

/// A new version (`current_version + 1`) of a locked page.
async fn new_version(
    conn: &mut PgConnection,
    page: &PageRow,
    body: &str,
    comment: &str,
    at: &Prov,
) -> Result<()> {
    let v = page.current_version + 1;
    insert_version(&mut *conn, page.id, v, body, comment, at).await?;
    sqlx::query(
        "UPDATE wiki_pages SET current_version = $2, lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(page.id)
    .bind(v)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- run (POST /commands)

struct Ctx<'a> {
    caller: AuthContext,
    who: &'a Executor,
    request: Uuid,
    hash: [u8; 32],
    a: Access,
    op_id: Uuid,
    deadline: Option<DateTime<Utc>>,
}

impl Ctx<'_> {
    async fn begin(&self, conn: &mut PgConnection) -> Result<(Prov, String)> {
        commands::check_deadline(&mut *conn, self.deadline).await?;
        fault("wiki.before_write").await?;
        let token = commands::search_token(&mut *conn).await?;
        let exec = commands::record_human_execution(
            &mut *conn,
            self.request,
            self.who.user_id,
            &self.hash,
            &token,
        )
        .await?;
        Ok((
            Prov {
                user: self.who.user_id,
                request: self.request,
                exec,
            },
            token,
        ))
    }

    fn require(&self, perm: &str, what: &str) -> Result<()> {
        if !self.a.has(perm) {
            return Err(forbidden(
                "permission",
                format!("{what} needs {perm} in this project"),
            ));
        }
        if !self.a.writable() {
            return Err(forbidden(
                "project_not_writable",
                "the project is closed: its wiki is read only",
            ));
        }
        Ok(())
    }

    fn require_unprotected(&self, page: &PageRow) -> Result<()> {
        if page.protected && !self.a.has(PROTECT_WIKI_PAGES) {
            return Err(forbidden(
                "wiki_protected",
                "this page is protected: only people with protect_wiki_pages change it",
            ));
        }
        Ok(())
    }
}

fn conflict(p: &PageRow, expected: i64, updated_by: Option<Uuid>) -> anyhow::Error {
    ApiError::new(
        ErrorCode::Conflict,
        "the page changed since you read it; here is its current version",
    )
    .with_details(json!({
        "page_id": p.id, "expected_lock_version": expected, "lock_version": p.lock_version,
        "current_version": p.current_version, "updated_by": updated_by,
    }))
    .into()
}

async fn author_of(conn: &mut PgConnection, page: Uuid, v: i32) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT actor_id FROM wiki_page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(page)
    .bind(v)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The precondition, when given, holds for `page` (FOR UPDATE).
async fn check_precondition(
    conn: &mut PgConnection,
    pre: Option<&WikiPrecondition>,
    page: &PageRow,
) -> Result<()> {
    if let Some(pre) = pre {
        if pre.wiki_page != page.id {
            return Err(bad_argument(
                "preconditions[0].wiki_page",
                "the precondition names another page than the target",
            ));
        }
        if pre.lock_version != page.lock_version {
            let by = author_of(&mut *conn, page.id, page.current_version).await?;
            return Err(conflict(page, pre.lock_version, by));
        }
    }
    Ok(())
}

/// The parent a reference names: a live page of the project.
async fn parent_of(
    conn: &mut PgConnection,
    project: Uuid,
    r: &Option<WikiPageRef>,
) -> Result<Option<Uuid>> {
    match r {
        None => Ok(None),
        Some(r) => Ok(Some(
            resolve(&mut *conn, project, r.page.as_deref(), r.page_id)
                .await?
                .ok_or_else(not_found)?
                .id,
        )),
    }
}

/// `POST /commands` with a wiki document (from `commands::run`): parsed,
/// the operation request opened (a used key replays), then run.
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
    let r = execute(&mut *conn, caller, who, request, hash, &doc).await?;
    let stored = json!({
        "execution_id": r.execution_id,
        "consistent_after": r.search.consistent_after,
        "items": r.items,
        "wiki": r.wiki.iter().map(|w| json!({
            "page_id": w.page_id, "project_id": w.project_id, "version": w.version,
            "lock_version": w.lock_version, "changed": w.changed, "created": w.created,
            "reparented": w.reparented,
        })).collect::<Vec<_>>(),
    });
    commands::complete_request(&mut *conn, request, &stored).await?;
    Ok(r)
}

/// A resent wiki document: the stored result, with titles as they are now
/// and only pages of wikis the caller still reads.
async fn replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    user: Uuid,
    operation_id: Uuid,
    fp: &[u8; 32],
) -> Result<CommandResult> {
    let r = commands::stored_result(&mut *conn, user, operation_id, commands::ACTION_COMMAND, fp)
        .await?;
    let mut wiki = Vec::new();
    let mut seen: BTreeMap<Uuid, bool> = BTreeMap::new();
    let mut kept: BTreeSet<Uuid> = BTreeSet::new();
    for e in r["wiki"].as_array().cloned().unwrap_or_default() {
        let (Some(page), Some(project)) = (
            e["page_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()),
            e["project_id"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok()),
        ) else {
            continue;
        };
        let ok = match seen.get(&project) {
            Some(ok) => *ok,
            None => {
                let ok = access_of(&mut *conn, caller, project).await?.is_some();
                seen.insert(project, ok);
                ok
            }
        };
        if !ok {
            continue;
        }
        let title: String = sqlx::query_scalar("SELECT title FROM wiki_pages WHERE id = $1")
            .bind(page)
            .fetch_one(&mut *conn)
            .await?;
        kept.insert(page);
        wiki.push(WikiEffect {
            page_id: page,
            project_id: project,
            title,
            version: e["version"].as_i64().unwrap_or_default() as i32,
            lock_version: e["lock_version"].as_i64().unwrap_or_default(),
            changed: e["changed"].as_bool().unwrap_or_default(),
            created: e["created"].as_bool().unwrap_or_default(),
            reparented: serde_json::from_value(e["reparented"].clone()).unwrap_or_default(),
        });
    }
    let items = serde_json::from_value::<Vec<ItemResult>>(r["items"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.wiki_page_id.is_some_and(|p| kept.contains(&p)))
        .collect();
    Ok(CommandResult {
        status: "succeeded".into(),
        execution_id: r["execution_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default(),
        effects: Vec::new(),
        items,
        search: SearchReceipt {
            consistent_after: r["consistent_after"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
        time_entries: Vec::new(),
        wiki,
    })
}

async fn execute(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    request: Uuid,
    hash: [u8; 32],
    doc: &WikiCommandDoc,
) -> Result<CommandResult> {
    let op = &doc.ops[0];
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    let project = project_id(&mut *conn, &op.target().project)
        .await?
        .ok_or_else(not_found)?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *conn)
        .await?;
    commands::check_deadline(&mut *conn, deadline).await?;
    let a = access_of(&mut *conn, caller, project)
        .await?
        .ok_or_else(not_found)?;
    let cx = Ctx {
        caller,
        who,
        request,
        hash,
        a,
        op_id: op.op_id(),
        deadline,
    };
    let pre = doc.preconditions.first();
    let (prov, token, effect, applied) = match op {
        WikiOp::WikiPut(o) => put(&mut *conn, &cx, o, pre).await?,
        WikiOp::WikiMove(o) => move_page(&mut *conn, &cx, o, pre).await?,
        WikiOp::WikiDelete(o) => delete(&mut *conn, &cx, o, pre).await?,
        WikiOp::WikiRestore(o) => restore(&mut *conn, &cx, o, pre).await?,
        WikiOp::WikiProtect(o) => protect(&mut *conn, &cx, o, pre).await?,
    };
    let _ = cx.caller;
    Ok(CommandResult {
        status: "succeeded".into(),
        execution_id: prov.exec,
        effects: Vec::new(),
        items: vec![ItemResult {
            op_id: cx.op_id,
            status: if applied { "applied" } else { "no_effect" }.into(),
            issue_id: None,
            version_id: None,
            time_entry_id: None,
            wiki_page_id: Some(effect.page_id),
        }],
        search: SearchReceipt {
            consistent_after: token,
        },
        time_entries: Vec::new(),
        wiki: vec![effect],
    })
}

type Done = (Prov, String, WikiEffect, bool);

fn effect(p: &PageRow, changed: bool, created: bool) -> WikiEffect {
    WikiEffect {
        page_id: p.id,
        project_id: p.project_id,
        title: p.title.clone(),
        version: p.current_version,
        lock_version: p.lock_version,
        changed,
        created,
        reparented: Vec::new(),
    }
}

/// `wiki_put` (R-W3): create, or update with the page's lock_version.
async fn put(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiPutOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    let found = resolve(
        &mut *conn,
        project,
        o.target.page.as_deref(),
        o.target.page_id,
    )
    .await?;
    let Some(found) = found else {
        if o.target.page_id.is_some() {
            return Err(not_found());
        }
        return create(conn, cx, o, pre).await;
    };
    let Some(pre) = pre else {
        if o.target.page_id.is_some() {
            return Err(bad_argument(
                "preconditions",
                "updating a page needs its lock_version precondition",
            ));
        }
        // R-W3: a create whose title is taken.
        return Err(page_exists(found.id));
    };
    let tree = o.title.is_some() || o.parent.is_some();
    if tree {
        lock_settings(&mut *conn, project).await?;
    }
    let page = lock_page(&mut *conn, found.id).await?;
    if page.deleted_at.is_some() {
        return Err(not_found());
    }
    check_precondition(&mut *conn, Some(pre), &page).await?;
    let new_parent = match &o.parent {
        None => page.parent_id,
        Some(r) => parent_of(&mut *conn, project, r).await?,
    };
    cx.require(EDIT_WIKI_PAGES, "editing a wiki page")?;
    cx.require_unprotected(&page)?;
    let new_title = match &o.title {
        None => page.title.clone(),
        Some(t) => checked_title("ops[0].title", t)?,
    };
    let retitled = new_title != page.title;
    let reparented = new_parent != page.parent_id;
    if retitled || reparented {
        cx.require(RENAME_WIKI_PAGES, "renaming or moving a wiki page")?;
    }
    // R-W8: a revert reads its source while holding the project row FOR
    // SHARE (taken in `execute`), which a D12 redaction takes FOR UPDATE: the
    // two run one after the other (R-W10), never copying a redacted body.
    let (body, comment) = match o.revert_from {
        Some(n) => {
            // A revert reads an earlier version for the caller: the history
            // right (R-W7) too, so it is no way around it (使用者決定 2026-11-16).
            cx.require(VIEW_WIKI_EDITS, "reverting to an earlier version")?;
            let Some((b, redacted)) = version_body(&mut *conn, page.id, n).await? else {
                return Err(bad_argument(
                    "ops[0].revert_from",
                    format!("the page has no version {n}"),
                ));
            };
            if redacted {
                return Err(invalid_value(
                    "ops[0].revert_from",
                    "redacted_source",
                    "a redacted version is no revert source",
                ));
            }
            let c = o
                .comment
                .clone()
                .unwrap_or_else(|| format!("回復到第 {n} 版"));
            (b, c)
        }
        None => (
            o.body.clone().unwrap_or_default(),
            o.comment.clone().unwrap_or_default(),
        ),
    };
    check_body("ops[0].body", &body)?;
    check_comment("ops[0].comment", &comment)?;
    let (old_body, _) = version_body(&mut *conn, page.id, page.current_version)
        .await?
        .unwrap_or_default();
    let rewritten = old_body != body;
    if rewritten && page.current_version >= MAX_VERSIONS {
        return Err(invalid_value(
            "ops[0].body",
            "wiki_version_limit",
            format!("a page keeps at most {MAX_VERSIONS} versions"),
        ));
    }
    if retitled
        && let Some(other) = live_by_title(&mut *conn, project, &new_title).await?
        && other.id != page.id
    {
        return Err(page_exists(other.id));
    }
    if reparented {
        check_parent(&mut *conn, "ops[0].parent", Some(page.id), new_parent).await?;
    }
    let (prov, token) = cx.begin(&mut *conn).await?;
    if !(rewritten || retitled || reparented) {
        return Ok((prov, token, effect(&page, false, false), false));
    }
    write_update(
        &mut *conn,
        &prov,
        &page,
        rewritten.then_some((body.as_str(), comment.as_str())),
        retitled.then_some(new_title.as_str()),
        reparented.then_some(new_parent),
        true,
    )
    .await?;
    let after = lock_page(&mut *conn, page.id).await?;
    insert_change(
        &mut *conn,
        &Change::from_before(prov.exec, &page).after(&after, &page.title),
    )
    .await?;
    Ok((prov, token, effect(&after, true, false), true))
}

/// The writes of an update: a version, a rename (with a redirect when
/// `keep`), a move; one event each for the rename and the move.
async fn write_update(
    conn: &mut PgConnection,
    at: &Prov,
    page: &PageRow,
    body: Option<(&str, &str)>,
    title: Option<&str>,
    parent: Option<Option<Uuid>>,
    keep: bool,
) -> Result<()> {
    if let Some((b, c)) = body {
        new_version(&mut *conn, page, b, c, at).await?;
    }
    if let Some(t) = title {
        retitle(&mut *conn, page, t, keep).await?;
        insert_event(
            &mut *conn,
            page.id,
            "renamed",
            json!({"from": page.title, "to": t}),
            at,
        )
        .await?;
    }
    if let Some(p) = parent {
        set_parent(&mut *conn, page.id, p).await?;
        insert_event(
            &mut *conn,
            page.id,
            "moved",
            json!({"from_parent": page.parent_id, "to_parent": p}),
            at,
        )
        .await?;
    }
    if body.is_none() && parent.is_none() {
        // A rename alone still moves the page's lock version.
        sqlx::query(
            "UPDATE wiki_pages SET lock_version = lock_version + 1, updated_at = now() WHERE id = $1",
        )
        .bind(page.id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn create(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiPutOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    if pre.is_some() {
        return Err(bad_argument(
            "preconditions",
            "a new page takes no precondition",
        ));
    }
    if o.title.is_some() {
        return Err(bad_argument(
            "ops[0].title",
            "a new page takes its title from target.page",
        ));
    }
    if o.revert_from.is_some() {
        return Err(bad_argument(
            "ops[0].revert_from",
            "a new page has no version to revert to",
        ));
    }
    let body = o.body.clone().unwrap_or_default();
    let raw_title = o.target.page.as_deref().unwrap_or_default();
    let (count, _) = lock_settings(&mut *conn, project).await?;
    let parent = match &o.parent {
        None | Some(None) => None,
        Some(r) => parent_of(&mut *conn, project, r).await?,
    };
    cx.require(EDIT_WIKI_PAGES, "creating a wiki page")?;
    let title = checked_title("ops[0].target.page", raw_title)?;
    check_body("ops[0].body", &body)?;
    let comment = o.comment.clone().unwrap_or_default();
    check_comment("ops[0].comment", &comment)?;
    // Under the settings lock: no create or restore of this project runs
    // in between, so the title check holds.
    if let Some(other) = live_by_title(&mut *conn, project, &title).await? {
        return Err(page_exists(other.id));
    }
    if count >= MAX_PAGES {
        return Err(invalid_value(
            "ops[0]",
            "wiki_page_limit",
            format!("a wiki holds at most {MAX_PAGES} pages"),
        ));
    }
    check_parent(&mut *conn, "ops[0].parent", None, parent).await?;
    let (prov, token) = cx.begin(&mut *conn).await?;
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO wiki_pages (id, project_id, title, title_key, parent_id, current_version, lock_version)
         VALUES ($1, $2, $3, wiki_title_key($3), $4, 1, 1)
         ON CONFLICT (project_id, title_key) WHERE deleted_at IS NULL DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(project)
    .bind(&title)
    .bind(parent)
    .fetch_optional(&mut *conn)
    .await?;
    if inserted.is_none() {
        let other = live_by_title(&mut *conn, project, &title).await?;
        return Err(page_exists(other.map(|p| p.id).unwrap_or_default()));
    }
    sqlx::query(
        "DELETE FROM wiki_redirects WHERE project_id = $1 AND title_key = wiki_title_key($2)",
    )
    .bind(project)
    .bind(&title)
    .execute(&mut *conn)
    .await?;
    insert_version(&mut *conn, id, 1, &body, &comment, &prov).await?;
    insert_event(
        &mut *conn,
        id,
        "created",
        json!({"title": title, "parent_id": parent}),
        &prov,
    )
    .await?;
    sqlx::query("UPDATE wiki_settings SET page_count = page_count + 1 WHERE project_id = $1")
        .bind(project)
        .execute(&mut *conn)
        .await?;
    let page = lock_page(&mut *conn, id).await?;
    insert_change(
        &mut *conn,
        &Change {
            execution_id: prov.exec,
            page_id: id,
            after_version: 1,
            after_title: Some(title.clone()),
            parent_changed: parent.is_some(),
            after_parent: parent,
            after_deleted: false,
            after_protected: false,
            after_lock_version: page.lock_version,
            ..Change::default()
        },
    )
    .await?;
    Ok((prov, token, effect(&page, true, true), true))
}

/// `wiki_move` (R-W4).
async fn move_page(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiMoveOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    let found = resolve(
        &mut *conn,
        project,
        o.target.page.as_deref(),
        o.target.page_id,
    )
    .await?
    .ok_or_else(not_found)?;
    lock_settings(&mut *conn, project).await?;
    let page = lock_page(&mut *conn, found.id).await?;
    if page.deleted_at.is_some() {
        return Err(not_found());
    }
    check_precondition(&mut *conn, pre, &page).await?;
    let new_parent = match &o.parent {
        None => page.parent_id,
        Some(r) => parent_of(&mut *conn, project, r).await?,
    };
    cx.require(RENAME_WIKI_PAGES, "renaming or moving a wiki page")?;
    cx.require_unprotected(&page)?;
    let new_title = match &o.title {
        None => page.title.clone(),
        Some(t) => checked_title("ops[0].title", t)?,
    };
    let retitled = new_title != page.title;
    let reparented = new_parent != page.parent_id;
    if retitled
        && let Some(other) = live_by_title(&mut *conn, project, &new_title).await?
        && other.id != page.id
    {
        return Err(page_exists(other.id));
    }
    if reparented {
        check_parent(&mut *conn, "ops[0].parent", Some(page.id), new_parent).await?;
    }
    let (prov, token) = cx.begin(&mut *conn).await?;
    if !(retitled || reparented) {
        return Ok((prov, token, effect(&page, false, false), false));
    }
    write_update(
        &mut *conn,
        &prov,
        &page,
        None,
        retitled.then_some(new_title.as_str()),
        reparented.then_some(new_parent),
        o.keep_redirect.unwrap_or(true),
    )
    .await?;
    let after = lock_page(&mut *conn, page.id).await?;
    insert_change(
        &mut *conn,
        &Change::from_before(prov.exec, &page).after(&after, &page.title),
    )
    .await?;
    Ok((prov, token, effect(&after, true, false), true))
}

/// `wiki_delete` (R-W5): soft delete; children move up with `reparent`.
async fn delete(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiDeleteOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    let found = resolve(
        &mut *conn,
        project,
        o.target.page.as_deref(),
        o.target.page_id,
    )
    .await?
    .ok_or_else(not_found)?;
    lock_settings(&mut *conn, project).await?;
    let page = lock_page(&mut *conn, found.id).await?;
    if page.deleted_at.is_some() {
        return Err(not_found());
    }
    check_precondition(&mut *conn, pre, &page).await?;
    cx.require(DELETE_WIKI_PAGES, "deleting a wiki page")?;
    cx.require_unprotected(&page)?;
    let children = live_children(&mut *conn, page.id).await?;
    if !children.is_empty() && o.children.is_none() {
        return Err(ApiError::new(
            ErrorCode::WikiHasChildren,
            "the page has child pages; delete it with children: \"reparent\"",
        )
        .with_details(json!({"page_id": page.id, "children": children.len()}))
        .into());
    }
    // R-W10: the children by id.
    let mut kids = Vec::new();
    for c in &children {
        kids.push(lock_page(&mut *conn, *c).await?);
    }
    // Moving a protected child is a move of a protected page (R-W6).
    for k in &kids {
        cx.require_unprotected(k)?;
    }
    let (prov, token) = cx.begin(&mut *conn).await?;
    for k in &kids {
        set_parent(&mut *conn, k.id, page.parent_id).await?;
        insert_event(
            &mut *conn,
            k.id,
            "moved",
            json!({"from_parent": page.id, "to_parent": page.parent_id}),
            &prov,
        )
        .await?;
        let after = lock_page(&mut *conn, k.id).await?;
        insert_change(
            &mut *conn,
            &Change::from_before(prov.exec, k).after(&after, &k.title),
        )
        .await?;
    }
    soft_delete(&mut *conn, page.id, project, prov.exec).await?;
    insert_event(
        &mut *conn,
        page.id,
        "deleted",
        json!({"children": children}),
        &prov,
    )
    .await?;
    let after = lock_page(&mut *conn, page.id).await?;
    insert_change(
        &mut *conn,
        &Change::from_before(prov.exec, &page).after(&after, &page.title),
    )
    .await?;
    let mut e = effect(&after, true, false);
    e.reparented = children;
    Ok((prov, token, e, true))
}

/// `wiki_restore` (R-W5): within 30 days, `manage_wiki`.
async fn restore(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiRestoreOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    // R-W16: deleted pages exist only for manage_wiki.
    if !cx.a.has(MANAGE_WIKI) {
        return Err(not_found());
    }
    let id = o.target.page_id.unwrap_or_default();
    lock_settings(&mut *conn, project).await?;
    let found = page_by_id(&mut *conn, project, id)
        .await?
        .ok_or_else(not_found)?;
    let page = lock_page(&mut *conn, found.id).await?;
    let Some(at) = page.deleted_at else {
        return Err(not_found());
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await?;
    if now - at > chrono::Duration::days(DELETED_DAYS) {
        return Err(not_found());
    }
    check_precondition(&mut *conn, pre, &page).await?;
    cx.require(MANAGE_WIKI, "restoring a wiki page")?;
    let (count, _) = sqlx::query_as::<_, (i32, Option<Uuid>)>(
        "SELECT page_count, start_page_id FROM wiki_settings WHERE project_id = $1",
    )
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(other) = live_by_title(&mut *conn, project, &page.title).await? {
        return Err(page_exists(other.id));
    }
    if count >= MAX_PAGES {
        return Err(invalid_value(
            "ops[0]",
            "wiki_page_limit",
            format!("a wiki holds at most {MAX_PAGES} pages"),
        ));
    }
    let parent = restore_parent(&mut *conn, &page).await?;
    let (prov, token) = cx.begin(&mut *conn).await?;
    undelete(&mut *conn, &page, parent).await?;
    insert_event(
        &mut *conn,
        page.id,
        "restored",
        json!({"parent_id": parent}),
        &prov,
    )
    .await?;
    let after = lock_page(&mut *conn, page.id).await?;
    insert_change(
        &mut *conn,
        &Change::from_before(prov.exec, &page).after(&after, &page.title),
    )
    .await?;
    Ok((prov, token, effect(&after, true, false), true))
}

/// `wiki_protect` (R-W6).
async fn protect(
    conn: &mut PgConnection,
    cx: &Ctx<'_>,
    o: &WikiProtectOp,
    pre: Option<&WikiPrecondition>,
) -> Result<Done> {
    let project = cx.a.project;
    let found = resolve(
        &mut *conn,
        project,
        o.target.page.as_deref(),
        o.target.page_id,
    )
    .await?
    .ok_or_else(not_found)?;
    let page = lock_page(&mut *conn, found.id).await?;
    if page.deleted_at.is_some() {
        return Err(not_found());
    }
    check_precondition(&mut *conn, pre, &page).await?;
    cx.require(PROTECT_WIKI_PAGES, "protecting a wiki page")?;
    let (prov, token) = cx.begin(&mut *conn).await?;
    if page.protected == o.protected {
        return Ok((prov, token, effect(&page, false, false), false));
    }
    set_protected(&mut *conn, page.id, o.protected, &prov).await?;
    let after = lock_page(&mut *conn, page.id).await?;
    insert_change(
        &mut *conn,
        &Change::from_before(prov.exec, &page).after(&after, &page.title),
    )
    .await?;
    Ok((prov, token, effect(&after, true, false), true))
}

async fn set_protected(conn: &mut PgConnection, page: Uuid, on: bool, at: &Prov) -> Result<()> {
    sqlx::query(
        "UPDATE wiki_pages SET protected = $2, lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(page)
    .bind(on)
    .execute(&mut *conn)
    .await?;
    insert_event(
        &mut *conn,
        page,
        if on { "protected" } else { "unprotected" },
        json!({}),
        at,
    )
    .await
}

// ---------------------------------------------------------------- reads

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiProjectRef {
    pub id: Uuid,
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiPageLink {
    pub id: Uuid,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiTreeItem {
    pub id: Uuid,
    pub title: String,
    pub parent_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiIndexCan {
    /// Create pages (`edit_wiki_pages`, active project).
    pub create: bool,
    /// Deleted pages, restore and settings (`manage_wiki`).
    pub manage: bool,
}

/// `GET /projects/{id}/wiki`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiIndex {
    pub project: WikiProjectRef,
    /// R-W13: the set start page, else the page titled "Wiki", else none
    /// (show the index).
    pub start_page: Option<WikiPageLink>,
    /// The project is closed: every write is refused.
    pub read_only: bool,
    pub can: WikiIndexCan,
    /// Every live page (at most 5000), by title.
    pub tree: Vec<WikiTreeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiPerson {
    pub id: Uuid,
    pub name: String,
    /// `user` or `agent`.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiPageCan {
    pub edit: bool,
    pub rename: bool,
    pub delete: bool,
    pub protect: bool,
    /// Versions and diffs (`view_wiki_edits`): a read permission.
    pub history: bool,
}

/// `GET /projects/{id}/wiki/pages/{title}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiPage {
    pub id: Uuid,
    pub project: WikiProjectRef,
    pub title: String,
    pub body: String,
    /// Sanitized HTML, references resolved for the caller.
    pub body_html: String,
    pub version: i32,
    pub lock_version: i64,
    pub protected: bool,
    pub parent: Option<WikiPageLink>,
    /// From the top level down to the parent.
    pub breadcrumbs: Vec<WikiPageLink>,
    pub children: Vec<WikiPageLink>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<WikiPerson>,
    /// The current version is an agent's (R-M3).
    pub by_agent: bool,
    /// Attachments come with M2.4: always empty.
    pub attachments: Vec<Json>,
    /// The title asked for, when it was an old title (a redirect).
    pub redirected_from: Option<String>,
    pub read_only: bool,
    pub can: WikiPageCan,
}

async fn person(conn: &mut PgConnection, id: Uuid) -> Result<Option<WikiPerson>> {
    let r: Option<(String, String)> =
        sqlx::query_as("SELECT name, kind FROM principals WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(r.map(|(name, kind)| WikiPerson { id, name, kind }))
}

fn project_ref(a: &Access) -> WikiProjectRef {
    WikiProjectRef {
        id: a.project,
        key: a.key.clone(),
        name: a.name.clone(),
    }
}

async fn start_page(conn: &mut PgConnection, project: Uuid) -> Result<Option<WikiPageLink>> {
    let r: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT p.id, p.title FROM wiki_settings s JOIN wiki_pages p ON p.id = s.start_page_id
         WHERE s.project_id = $1 AND p.deleted_at IS NULL",
    )
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((id, title)) = r {
        return Ok(Some(WikiPageLink { id, title }));
    }
    Ok(live_by_title(&mut *conn, project, DEFAULT_START_TITLE)
        .await?
        .map(|p| WikiPageLink {
            id: p.id,
            title: p.title,
        }))
}

pub async fn index(app: &App, caller: AuthContext, project: &str) -> Result<WikiIndex> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let tree: Vec<(Uuid, String, Option<Uuid>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, title, parent_id, updated_at FROM wiki_pages
         WHERE project_id = $1 AND deleted_at IS NULL ORDER BY title, id LIMIT $2",
    )
    .bind(a.project)
    .bind(MAX_PAGES as i64)
    .fetch_all(&mut *tx)
    .await?;
    let start = start_page(&mut tx, a.project).await?;
    tx.commit().await?;
    Ok(WikiIndex {
        project: project_ref(&a),
        start_page: start,
        read_only: !a.writable(),
        can: WikiIndexCan {
            create: a.may(EDIT_WIKI_PAGES),
            manage: a.has(MANAGE_WIKI),
        },
        tree: tree
            .into_iter()
            .map(|(id, title, parent_id, updated_at)| WikiTreeItem {
                id,
                title,
                parent_id,
                updated_at,
            })
            .collect(),
    })
}

/// A live page by a URL title (`_` is a space), following a redirect.
async fn find_for_read(
    conn: &mut PgConnection,
    project: Uuid,
    title: &str,
) -> Result<(PageRow, Option<String>)> {
    let t = path_title(title);
    if let Some(p) = live_by_title(&mut *conn, project, &t).await? {
        return Ok((p, None));
    }
    let to: Option<Uuid> = sqlx::query_scalar(
        "SELECT page_id FROM wiki_redirects WHERE project_id = $1 AND title_key = wiki_title_key($2)",
    )
    .bind(project)
    .bind(&t)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(to) = to else {
        return Err(not_found());
    };
    let p = page_by_id(&mut *conn, project, to)
        .await?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(not_found)?;
    Ok((p, Some(t)))
}

async fn links(conn: &mut PgConnection, ids: &[Uuid]) -> Result<BTreeMap<Uuid, String>> {
    Ok(
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, title FROM wiki_pages WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect(),
    )
}

/// One version's author, origin and whether an agent wrote it.
async fn version_meta(
    conn: &mut PgConnection,
    page: Uuid,
    v: i32,
) -> Result<(Option<WikiPerson>, bool)> {
    let r: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT actor_id, actor_kind FROM wiki_page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(page)
    .bind(v)
    .fetch_optional(&mut *conn)
    .await?;
    match r {
        None => Ok((None, false)),
        Some((a, k)) => Ok((person(&mut *conn, a).await?, k == "agent")),
    }
}

async fn render(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    container: Uuid,
    body: &str,
    agent: bool,
) -> Result<String> {
    Ok(markdown::render_for(
        &mut *conn,
        caller,
        project,
        &[Text {
            text: body,
            src: Source {
                format: commands::TextFormat::Markdown,
                agent,
                container,
            },
        }],
    )
    .await?
    .pop()
    .unwrap_or_default())
}

fn page_can(a: &Access, p: &PageRow) -> WikiPageCan {
    let unlocked = !p.protected || a.has(PROTECT_WIKI_PAGES);
    WikiPageCan {
        edit: a.may(EDIT_WIKI_PAGES) && unlocked,
        rename: a.may(RENAME_WIKI_PAGES) && unlocked,
        delete: a.may(DELETE_WIKI_PAGES) && unlocked,
        protect: a.may(PROTECT_WIKI_PAGES),
        history: a.has(VIEW_WIKI_EDITS),
    }
}

pub async fn page(app: &App, caller: AuthContext, project: &str, title: &str) -> Result<WikiPage> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let (p, redirected_from) = find_for_read(&mut tx, a.project, title).await?;
    let (body, _) = version_body(&mut tx, p.id, p.current_version)
        .await?
        .unwrap_or_default();
    let (updated_by, by_agent) = version_meta(&mut tx, p.id, p.current_version).await?;
    let body_html = render(&mut tx, caller, a.project, p.id, &body, by_agent).await?;
    let mut up = ancestors(&mut tx, p.id).await?;
    up.reverse();
    let kids = live_children(&mut tx, p.id).await?;
    let mut ids = up.clone();
    ids.extend(&kids);
    let names = links(&mut tx, &ids).await?;
    let link = |id: &Uuid| WikiPageLink {
        id: *id,
        title: names.get(id).cloned().unwrap_or_default(),
    };
    let mut children: Vec<WikiPageLink> = kids.iter().map(link).collect();
    children.sort_by(|x, y| x.title.cmp(&y.title));
    let out = WikiPage {
        id: p.id,
        project: project_ref(&a),
        title: p.title.clone(),
        body,
        body_html,
        version: p.current_version,
        lock_version: p.lock_version,
        protected: p.protected,
        parent: p.parent_id.as_ref().map(link),
        breadcrumbs: up.iter().map(link).collect(),
        children,
        updated_at: p.updated_at,
        updated_by,
        by_agent,
        attachments: Vec::new(),
        redirected_from,
        read_only: !a.writable(),
        can: page_can(&a, &p),
    };
    tx.commit().await?;
    Ok(out)
}

fn need_history(a: &Access) -> Result<()> {
    if !a.has(VIEW_WIKI_EDITS) {
        return Err(forbidden(
            "permission",
            "the history of wiki pages needs view_wiki_edits",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiVersionItem {
    pub version: i32,
    pub author: Option<WikiPerson>,
    /// `human_direct`, `agent_direct` or `external_agent`.
    pub origin: String,
    pub comment: String,
    /// Bytes of the body.
    pub size: i64,
    pub created_at: DateTime<Utc>,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiVersionList {
    pub page_id: Uuid,
    pub title: String,
    pub current_version: i32,
    /// Newest first, 50 per page.
    pub items: Vec<WikiVersionItem>,
    pub next_cursor: Option<String>,
}

async fn version_items(
    conn: &mut PgConnection,
    page: Uuid,
    before: Option<i32>,
    limit: i64,
) -> Result<Vec<WikiVersionItem>> {
    let rows: Vec<(i32, Uuid, String, String, i64, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT version, actor_id, origin, comment, octet_length(body)::bigint, created_at,
                redaction_id IS NOT NULL
         FROM wiki_page_versions WHERE page_id = $1 AND ($2::int IS NULL OR version < $2)
         ORDER BY version DESC LIMIT $3",
    )
    .bind(page)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (version, actor, origin, comment, size, created_at, redacted) in rows {
        out.push(WikiVersionItem {
            version,
            author: person(&mut *conn, actor).await?,
            origin,
            comment,
            size,
            created_at,
            redacted,
        });
    }
    Ok(out)
}

fn version_cursor(c: Option<&str>) -> Result<Option<i32>> {
    c.map(|c| {
        c.strip_prefix("v")
            .and_then(|n| n.parse::<i32>().ok())
            .ok_or_else(|| api(ErrorCode::InvalidCursor, "not a wiki versions cursor"))
    })
    .transpose()
}

pub async fn versions(
    app: &App,
    caller: AuthContext,
    project: &str,
    title: &str,
    cursor: Option<&str>,
) -> Result<WikiVersionList> {
    let before = version_cursor(cursor)?;
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let (p, _) = find_for_read(&mut tx, a.project, title).await?;
    need_history(&a)?;
    let mut items = version_items(&mut tx, p.id, before, VERSIONS_PAGE + 1).await?;
    tx.commit().await?;
    let more = items.len() as i64 > VERSIONS_PAGE;
    items.truncate(VERSIONS_PAGE as usize);
    let next_cursor = if more {
        items.last().map(|i| format!("v{}", i.version))
    } else {
        None
    };
    Ok(WikiVersionList {
        page_id: p.id,
        title: p.title,
        current_version: p.current_version,
        items,
        next_cursor,
    })
}

/// `GET …/versions/{n}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiVersionView {
    pub page_id: Uuid,
    pub title: String,
    pub version: i32,
    pub current_version: i32,
    /// `[已遮蔽]` when redacted.
    pub body: String,
    pub body_html: String,
    pub comment: String,
    pub author: Option<WikiPerson>,
    pub origin: String,
    pub created_at: DateTime<Utc>,
    pub redacted: bool,
}

pub async fn version(
    app: &App,
    caller: AuthContext,
    project: &str,
    title: &str,
    n: i32,
) -> Result<WikiVersionView> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let (p, _) = find_for_read(&mut tx, a.project, title).await?;
    need_history(&a)?;
    let r: Option<(String, String, Uuid, String, String, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT body, comment, actor_id, actor_kind, origin, created_at, redaction_id IS NOT NULL
         FROM wiki_page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(p.id)
    .bind(n)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((body, comment, actor, kind, origin, created_at, redacted)) = r else {
        return Err(api(ErrorCode::NotFound, "no such version"));
    };
    let body_html = render(&mut tx, caller, a.project, p.id, &body, kind == "agent").await?;
    let author = person(&mut tx, actor).await?;
    tx.commit().await?;
    Ok(WikiVersionView {
        page_id: p.id,
        title: p.title,
        version: n,
        current_version: p.current_version,
        body,
        body_html,
        comment,
        author,
        origin,
        created_at,
        redacted,
    })
}

// ---------------------------------------------------------------- diff (R-W7)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DiffWord {
    /// `equal`, `delete` or `insert`.
    pub op: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DiffLine {
    /// `equal`, `delete` or `insert`.
    pub op: String,
    /// The line without its line break.
    pub text: String,
    /// Changed lines paired with their counterpart: word-level pieces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<DiffWord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DiffHunk {
    /// 1-based line numbers where the hunk starts.
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiDiff {
    pub page_id: Uuid,
    pub from: i32,
    pub to: i32,
    pub hunks: Vec<DiffHunk>,
    /// More than 2,000 changed lines: the first 2,000 only.
    pub truncated: bool,
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// R-W7 word pieces: runs of ASCII letters and digits, runs of white
/// space, and every other character on its own (so CJK text compares by
/// character).
pub fn words(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = 0u8; // 1 alnum, 2 space
    for (i, c) in s.char_indices() {
        let k = if c.is_ascii_alphanumeric() {
            1
        } else if c.is_whitespace() {
            2
        } else {
            0
        };
        if i > start && (k == 0 || k != kind) {
            out.push(&s[start..i]);
            start = i;
        }
        kind = k;
        if k == 0 {
            let end = i + c.len_utf8();
            out.push(&s[i..end]);
            start = end;
            kind = 3;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn word_diff(old: &str, new: &str) -> (Vec<DiffWord>, Vec<DiffWord>) {
    let (a, b) = (words(old), words(new));
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &a, &b);
    let (mut l, mut r) = (Vec::new(), Vec::new());
    let push = |v: &mut Vec<DiffWord>, op: &str, text: String| {
        if let Some(last) = v.last_mut()
            && last.op == op
        {
            last.text.push_str(&text);
            return;
        }
        v.push(DiffWord {
            op: op.into(),
            text,
        });
    };
    for op in ops {
        let (tag, o, n) = op.as_tag_tuple();
        let ot: String = a[o].concat();
        let nt: String = b[n].concat();
        match tag {
            similar::DiffTag::Equal => {
                push(&mut l, "equal", ot);
                push(&mut r, "equal", nt);
            }
            similar::DiffTag::Delete => push(&mut l, "delete", ot),
            similar::DiffTag::Insert => push(&mut r, "insert", nt),
            similar::DiffTag::Replace => {
                push(&mut l, "delete", ot);
                push(&mut r, "insert", nt);
            }
        }
    }
    (l, r)
}

/// R-W7: a line diff (3 lines of context) with word pieces for changed
/// lines; at most [`DIFF_MAX_LINES`] changed lines.
pub fn diff(old: &str, new: &str) -> (Vec<DiffHunk>, bool) {
    let (a, b) = (split_lines(old), split_lines(new));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let ops =
        similar::capture_diff_slices_deadline(similar::Algorithm::Myers, &a, &b, Some(deadline));
    let mut hunks = Vec::new();
    let mut changed = 0usize;
    let mut truncated = false;
    'groups: for group in similar::group_diff_ops(ops, 3) {
        let Some(first) = group.first() else { continue };
        let (_, o0, n0) = first.as_tag_tuple();
        let mut h = DiffHunk {
            old_start: o0.start + 1,
            new_start: n0.start + 1,
            lines: Vec::new(),
        };
        for op in &group {
            let (tag, o, n) = op.as_tag_tuple();
            let line = |op: &str, text: &str, words: Option<Vec<DiffWord>>| DiffLine {
                op: op.into(),
                text: text.into(),
                words,
            };
            match tag {
                similar::DiffTag::Equal => {
                    for i in o {
                        h.lines.push(line("equal", a[i], None));
                    }
                }
                similar::DiffTag::Delete | similar::DiffTag::Insert | similar::DiffTag::Replace => {
                    let dels: Vec<&str> = o.map(|i| a[i]).collect();
                    let ins: Vec<&str> = n.map(|i| b[i]).collect();
                    if changed + dels.len() + ins.len() > DIFF_MAX_LINES {
                        truncated = true;
                    }
                    let paired = dels.len().min(ins.len());
                    let mut lw: Vec<Option<Vec<DiffWord>>> = vec![None; dels.len()];
                    let mut rw: Vec<Option<Vec<DiffWord>>> = vec![None; ins.len()];
                    for i in 0..paired {
                        let (l, r) = word_diff(dels[i], ins[i]);
                        lw[i] = Some(l);
                        rw[i] = Some(r);
                    }
                    for (i, d) in dels.iter().enumerate() {
                        if changed >= DIFF_MAX_LINES {
                            break;
                        }
                        h.lines.push(line("delete", d, lw[i].take()));
                        changed += 1;
                    }
                    for (i, s) in ins.iter().enumerate() {
                        if changed >= DIFF_MAX_LINES {
                            break;
                        }
                        h.lines.push(line("insert", s, rw[i].take()));
                        changed += 1;
                    }
                    if truncated {
                        hunks.push(h);
                        break 'groups;
                    }
                }
            }
        }
        hunks.push(h);
    }
    (hunks, truncated)
}

pub async fn page_diff(
    app: &App,
    caller: AuthContext,
    project: &str,
    title: &str,
    from: Option<i32>,
    to: Option<i32>,
) -> Result<WikiDiff> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let (p, _) = find_for_read(&mut tx, a.project, title).await?;
    need_history(&a)?;
    let to = to.unwrap_or(p.current_version);
    let from = from.unwrap_or(to - 1);
    let old = if from == 0 {
        Some((String::new(), false))
    } else {
        version_body(&mut tx, p.id, from).await?
    };
    let new = version_body(&mut tx, p.id, to).await?;
    tx.commit().await?;
    let (Some((old, _)), Some((new, _))) = (old, new) else {
        return Err(api(ErrorCode::NotFound, "no such version"));
    };
    let (hunks, truncated) = tokio::task::spawn_blocking(move || diff(&old, &new)).await?;
    Ok(WikiDiff {
        page_id: p.id,
        from,
        to,
        hunks,
        truncated,
    })
}

// ---------------------------------------------------------------- events

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct WikiEventItem {
    /// The id D12 takes (`…/events/{event_id}/redact`).
    pub id: Uuid,
    pub seq: i64,
    /// `created`, `renamed`, `moved`, `deleted`, `restored`, `protected`,
    /// `unprotected`.
    pub kind: String,
    /// `created {title, parent_id}`, `renamed {from, to}`, `moved
    /// {from_parent, to_parent}`, `deleted {children}`, `restored
    /// {parent_id}`; `"[已遮蔽]"` when redacted.
    pub payload: Json,
    pub actor: Option<WikiPerson>,
    pub origin: String,
    pub at: DateTime<Utc>,
    pub redacted: bool,
}

/// `GET …/pages/{title}/events` (`view_wiki_edits`): the page's changes
/// other than content, oldest first.
pub async fn events(
    app: &App,
    caller: AuthContext,
    project: &str,
    title: &str,
) -> Result<Vec<WikiEventItem>> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let (p, _) = find_for_read(&mut tx, a.project, title).await?;
    need_history(&a)?;
    let rows: Vec<(Uuid, i64, String, Json, Uuid, String, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT id, seq, kind, payload, actor_id, origin, at, redaction_id IS NOT NULL
         FROM wiki_events WHERE page_id = $1 ORDER BY seq",
    )
    .bind(p.id)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::new();
    for (id, seq, kind, payload, actor, origin, at, redacted) in rows {
        out.push(WikiEventItem {
            id,
            seq,
            kind,
            payload,
            actor: person(&mut tx, actor).await?,
            origin,
            at,
            redacted,
        });
    }
    tx.commit().await?;
    Ok(out)
}

// ---------------------------------------------------------------- deleted pages (R-W16)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiDeletedItem {
    pub id: Uuid,
    pub title: String,
    pub version: i32,
    pub deleted_at: DateTime<Utc>,
    pub deleted_by: Option<WikiPerson>,
    /// The title is used by a live page now: restoring is
    /// `wiki_page_exists` until it is renamed.
    pub title_taken: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiDeletedList {
    pub items: Vec<WikiDeletedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiDeletedPage {
    pub id: Uuid,
    pub project: WikiProjectRef,
    pub title: String,
    pub body: String,
    pub body_html: String,
    pub version: i32,
    pub parent_id: Option<Uuid>,
    pub deleted_at: DateTime<Utc>,
    pub deleted_by: Option<WikiPerson>,
    pub title_taken: bool,
    /// With `view_wiki_edits` only.
    pub versions: Option<Vec<WikiVersionItem>>,
}

fn need_manage(a: &Access) -> Result<()> {
    if !a.has(MANAGE_WIKI) {
        return Err(forbidden(
            "permission",
            "deleted wiki pages need manage_wiki",
        ));
    }
    Ok(())
}

async fn deleted_by(conn: &mut PgConnection, exec: Option<Uuid>) -> Result<Option<WikiPerson>> {
    let Some(e) = exec else { return Ok(None) };
    let actor: Option<Uuid> =
        sqlx::query_scalar("SELECT actor_id FROM command_executions WHERE id = $1")
            .bind(e)
            .fetch_optional(&mut *conn)
            .await?;
    match actor {
        Some(a) => person(&mut *conn, a).await,
        None => Ok(None),
    }
}

pub async fn deleted(app: &App, caller: AuthContext, project: &str) -> Result<WikiDeletedList> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    need_manage(&a)?;
    let rows: Vec<(Uuid, String, i32, DateTime<Utc>, Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT p.id, p.title, p.current_version, p.deleted_at, p.deleted_by_execution_id,
                EXISTS (SELECT 1 FROM wiki_pages o WHERE o.project_id = p.project_id
                          AND o.title_key = p.title_key AND o.deleted_at IS NULL)
         FROM wiki_pages p
         WHERE p.project_id = $1 AND p.deleted_at > now() - make_interval(days => $2)
         ORDER BY p.deleted_at DESC, p.id",
    )
    .bind(a.project)
    .bind(DELETED_DAYS as i32)
    .fetch_all(&mut *tx)
    .await?;
    let mut items = Vec::new();
    for (id, title, version, deleted_at, by, title_taken) in rows {
        items.push(WikiDeletedItem {
            id,
            title,
            version,
            deleted_at,
            deleted_by: deleted_by(&mut tx, by).await?,
            title_taken,
        });
    }
    tx.commit().await?;
    Ok(WikiDeletedList { items })
}

pub async fn deleted_page(
    app: &App,
    caller: AuthContext,
    project: &str,
    page_id: Uuid,
) -> Result<WikiDeletedPage> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    need_manage(&a)?;
    let r: Option<(
        Uuid,
        String,
        i32,
        Option<Uuid>,
        DateTime<Utc>,
        Option<Uuid>,
        bool,
    )> = sqlx::query_as(
        "SELECT p.id, p.title, p.current_version, p.parent_id, p.deleted_at,
                    p.deleted_by_execution_id,
                    EXISTS (SELECT 1 FROM wiki_pages o WHERE o.project_id = p.project_id
                              AND o.title_key = p.title_key AND o.deleted_at IS NULL)
             FROM wiki_pages p
             WHERE p.project_id = $1 AND p.id = $2
               AND p.deleted_at > now() - make_interval(days => $3)",
    )
    .bind(a.project)
    .bind(page_id)
    .bind(DELETED_DAYS as i32)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, title, version, parent_id, deleted_at, by, title_taken)) = r else {
        return Err(not_found());
    };
    let (body, _) = version_body(&mut tx, id, version)
        .await?
        .unwrap_or_default();
    let (_, agent) = version_meta(&mut tx, id, version).await?;
    let body_html = render(&mut tx, caller, a.project, id, &body, agent).await?;
    let versions = if a.has(VIEW_WIKI_EDITS) {
        Some(version_items(&mut tx, id, None, MAX_VERSIONS as i64).await?)
    } else {
        None
    };
    let deleted_by = deleted_by(&mut tx, by).await?;
    tx.commit().await?;
    Ok(WikiDeletedPage {
        id,
        project: project_ref(&a),
        title,
        body,
        body_html,
        version,
        parent_id,
        deleted_at,
        deleted_by,
        title_taken,
        versions,
    })
}

// ---------------------------------------------------------------- settings (R-W13)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WikiSettings {
    pub start_page_id: Option<Uuid>,
    /// The page the wiki opens on now (R-W13 fallback included).
    pub start_page: Option<WikiPageLink>,
    pub lock_version: i64,
    /// `manage_wiki` in an active project.
    pub can_manage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiSettingsPut {
    /// A live page of this wiki, or null (the page titled "Wiki").
    pub start_page_id: Option<Uuid>,
    pub lock_version: i64,
}

async fn settings_of(conn: &mut PgConnection, a: &Access) -> Result<WikiSettings> {
    let (start_page_id, lock_version): (Option<Uuid>, i64) = sqlx::query_as(
        "SELECT start_page_id, lock_version FROM wiki_settings WHERE project_id = $1",
    )
    .bind(a.project)
    .fetch_one(&mut *conn)
    .await?;
    Ok(WikiSettings {
        start_page_id,
        start_page: start_page(&mut *conn, a.project).await?,
        lock_version,
        can_manage: a.may(MANAGE_WIKI),
    })
}

pub async fn settings(app: &App, caller: AuthContext, project: &str) -> Result<WikiSettings> {
    let mut tx = read_tx(app).await?;
    let a = access(&mut tx, caller, project).await?;
    let s = settings_of(&mut tx, &a).await?;
    tx.commit().await?;
    Ok(s)
}

/// `PUT /projects/{id}/wiki/settings`: operation request → executor →
/// project `FOR SHARE` → settings `FOR UPDATE` → checks → write →
/// `project_setting_events` (`wiki_start_page`).
pub async fn put_settings(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &WikiSettingsPut,
) -> Result<WikiSettings> {
    let who = commands::executor(&caller)?;
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    let pid = project_id(&mut tx, project).await?.ok_or_else(not_found)?;
    let h: [u8; 32] = Sha256::digest(
        json!({"project": pid, "input": serde_json::to_value(input)?})
            .to_string()
            .as_bytes(),
    )
    .into();
    let fp = commands::fingerprint(ACTION_SETTINGS, &h);
    let Some(request) =
        commands::open_request(&mut tx, who.user_id, operation_id, ACTION_SETTINGS, &fp).await?
    else {
        let stored =
            commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_SETTINGS, &fp)
                .await?;
        // The wiki must still be readable to see the stored answer; titles
        // are read now (a D12 title redaction may have changed them).
        let a = access_of(&mut tx, caller, pid)
            .await?
            .ok_or_else(not_found)?;
        let start_page_id = stored["start_page_id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok());
        // The stored page by its title now; none when it is gone (no
        // fallback to the "Wiki" page, which is not what was set).
        let link: Option<(Uuid, String)> = match start_page_id {
            Some(sp) => {
                sqlx::query_as(
                    "SELECT id, title FROM wiki_pages
                     WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
                )
                .bind(sp)
                .bind(a.project)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        let s = WikiSettings {
            start_page_id,
            start_page: link.map(|(id, title)| WikiPageLink { id, title }),
            lock_version: stored["lock_version"].as_i64().unwrap_or_default(),
            can_manage: a.may(MANAGE_WIKI),
        };
        tx.commit().await?;
        return Ok(s);
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(pid)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, pid)
        .await?
        .ok_or_else(not_found)?;
    if !a.has(MANAGE_WIKI) {
        return Err(forbidden(
            "permission",
            "the wiki settings need manage_wiki",
        ));
    }
    if !a.writable() {
        return Err(forbidden(
            "project_not_writable",
            "the project is closed: its wiki is read only",
        ));
    }
    let (old, lv): (Option<Uuid>, i64) = sqlx::query_as(
        "SELECT start_page_id, lock_version FROM wiki_settings WHERE project_id = $1 FOR UPDATE",
    )
    .bind(pid)
    .fetch_one(&mut *tx)
    .await?;
    if lv != input.lock_version {
        let current = settings_of(&mut tx, &a).await?;
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the wiki settings changed; here are the current ones",
        )
        .with_details(json!({"current_lock_version": lv, "current": current}))
        .into());
    }
    if let Some(sp) = input.start_page_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM wiki_pages
                            WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL)",
        )
        .bind(sp)
        .bind(pid)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(invalid_value(
                "start_page_id",
                "start_page_invalid",
                "the start page is a live page of this wiki",
            ));
        }
    }
    if old != input.start_page_id {
        sqlx::query(
            "UPDATE wiki_settings SET start_page_id = $2, lock_version = lock_version + 1,
                    updated_by = $3, updated_at = now()
             WHERE project_id = $1",
        )
        .bind(pid)
        .bind(input.start_page_id)
        .bind(who.user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
             VALUES ($1, 'wiki_start_page', $2, $3, $4)",
        )
        .bind(pid)
        .bind(who.user_id)
        .bind(request)
        .bind(json!({"before": old, "after": input.start_page_id}))
        .execute(&mut *tx)
        .await?;
    }
    let s = settings_of(&mut tx, &a).await?;
    commands::complete_request(
        &mut tx,
        request,
        &json!({"project_id": pid, "start_page_id": s.start_page_id, "lock_version": s.lock_version}),
    )
    .await?;
    tx.commit().await?;
    Ok(s)
}

// ---------------------------------------------------------------- undo (R-W9)

/// One page an execution changed, as its undo would treat it.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct UndoWikiPage {
    pub page_id: Uuid,
    pub project_id: Uuid,
    /// The page's title now.
    pub title: String,
    /// `created` (undo: soft delete), `updated` (content, title, parent or
    /// protection back), `deleted` (undo: restore), `restored` (undo:
    /// delete again) or `reparented` (a child of a deleted page; undo: back
    /// under it).
    pub change: String,
    /// `{version?, title?, parent_id?, deleted?, protected?}` before and
    /// after the execution, and now.
    pub before: Json,
    pub after: Json,
    pub current: Json,
    pub restorable: bool,
    /// `expired`, `changed_since`, `redacted_source`, `redacted`,
    /// `title_taken`, `has_children`, `dependency`, `value_no_longer_valid`,
    /// `wiki_depth`, `wiki_page_limit`, `wiki_version_limit` (the undo
    /// answers `invalid_value` with that kind) or `permission`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Whether execution `id` wrote any wiki page (its undo is the wiki's).
pub(crate) async fn has_changes(conn: &mut PgConnection, id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM execution_wiki_changes WHERE execution_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

async fn changes_of(conn: &mut PgConnection, id: Uuid) -> Result<Vec<Change>> {
    Ok(sqlx::query_as(
        "SELECT execution_id, page_id, before_version, after_version, before_title, after_title,
                parent_changed, before_parent, after_parent, before_deleted, after_deleted,
                before_protected, after_protected, before_lock_version, after_lock_version, reverted_by
         FROM execution_wiki_changes WHERE execution_id = $1 ORDER BY page_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Later writes of `page` (rows with a larger lock version).
async fn later(conn: &mut PgConnection, page: Uuid, lv: i64) -> Result<Vec<Change>> {
    Ok(sqlx::query_as(
        "SELECT execution_id, page_id, before_version, after_version, before_title, after_title,
                parent_changed, before_parent, after_parent, before_deleted, after_deleted,
                before_protected, after_protected, before_lock_version, after_lock_version, reverted_by
         FROM execution_wiki_changes WHERE page_id = $1 AND after_lock_version > $2",
    )
    .bind(page)
    .bind(lv)
    .fetch_all(&mut *conn)
    .await?)
}

fn kind_of(c: &Change, deleted_pages: &BTreeSet<Uuid>) -> &'static str {
    if c.before_version.is_none() {
        "created"
    } else if c.before_deleted == Some(false) && c.after_deleted {
        "deleted"
    } else if c.before_deleted == Some(true) && !c.after_deleted {
        "restored"
    } else if c.parent_changed
        && c.before_parent.is_some_and(|p| deleted_pages.contains(&p))
        && c.before_version == Some(c.after_version)
        && c.before_title.is_none()
    {
        "reparented"
    } else {
        "updated"
    }
}

fn side(c: &Change, before: bool) -> Json {
    let mut m = serde_json::Map::new();
    if before {
        if let Some(v) = c.before_version
            && v != c.after_version
        {
            m.insert("version".into(), json!(v));
        }
        if let Some(t) = &c.before_title {
            m.insert("title".into(), json!(t));
        }
        if c.parent_changed {
            m.insert("parent_id".into(), json!(c.before_parent));
        }
        if c.before_deleted.is_some_and(|d| d != c.after_deleted) {
            m.insert("deleted".into(), json!(c.before_deleted));
        }
        if c.before_protected.is_some_and(|p| p != c.after_protected) {
            m.insert("protected".into(), json!(c.before_protected));
        }
    } else {
        if c.before_version != Some(c.after_version) {
            m.insert("version".into(), json!(c.after_version));
        }
        if let Some(t) = &c.after_title {
            m.insert("title".into(), json!(t));
        }
        if c.parent_changed {
            m.insert("parent_id".into(), json!(c.after_parent));
        }
        if c.before_deleted != Some(c.after_deleted) {
            m.insert("deleted".into(), json!(c.after_deleted));
        }
        if c.before_protected != Some(c.after_protected) {
            m.insert("protected".into(), json!(c.after_protected));
        }
    }
    Json::Object(m)
}

fn now_side(p: &PageRow, like: &Json) -> Json {
    let mut m = serde_json::Map::new();
    if let Json::Object(o) = like {
        for k in o.keys() {
            let v = match k.as_str() {
                "version" => json!(p.current_version),
                "title" => json!(p.title),
                "parent_id" => json!(p.parent_id),
                "deleted" => json!(p.deleted_at.is_some()),
                "protected" => json!(p.protected),
                _ => Json::Null,
            };
            m.insert(k.clone(), v);
        }
    }
    Json::Object(m)
}

/// The permission undoing a row needs (the original op's).
fn undo_perm(kind: &str, c: &Change) -> Vec<&'static str> {
    match kind {
        "created" => vec![EDIT_WIKI_PAGES],
        "deleted" => vec![DELETE_WIKI_PAGES],
        "restored" => vec![MANAGE_WIKI],
        "reparented" => vec![DELETE_WIKI_PAGES],
        _ => {
            let mut v = Vec::new();
            if c.before_version != Some(c.after_version) {
                v.push(EDIT_WIKI_PAGES);
            }
            if c.before_title.is_some() || c.parent_changed {
                v.push(RENAME_WIKI_PAGES);
            }
            if c.before_protected != Some(c.after_protected) {
                v.push(PROTECT_WIKI_PAGES);
            }
            v
        }
    }
}

/// The undo preview of execution `id`'s wiki rows, as `caller` sees them
/// now (pages of wikis they cannot read are left out).
pub(crate) async fn undo_preview(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    expired: bool,
) -> Result<Vec<UndoWikiPage>> {
    let rows = changes_of(&mut *conn, id).await?;
    let deleted_pages: BTreeSet<Uuid> = rows
        .iter()
        .filter(|c| c.before_deleted == Some(false) && c.after_deleted)
        .map(|c| c.page_id)
        .collect();
    let mut access_cache: BTreeMap<Uuid, Option<Access>> = BTreeMap::new();
    let mut out = Vec::new();
    // The pages first, then the children that depend on them.
    let mut order: Vec<&Change> = rows.iter().collect();
    order.sort_by_key(|c| (kind_of(c, &deleted_pages) == "reparented", c.page_id));
    for c in order {
        let p: PageRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PAGE_COLS} FROM wiki_pages WHERE id = $1"
        )))
        .bind(c.page_id)
        .fetch_one(&mut *conn)
        .await?;
        let a = match access_cache.get(&p.project_id) {
            Some(a) => a.clone(),
            None => {
                let a = access_of(&mut *conn, caller, p.project_id).await?;
                access_cache.insert(p.project_id, a.clone());
                a
            }
        };
        let Some(a) = a else { continue };
        let kind = kind_of(c, &deleted_pages);
        let before = side(c, true);
        let after = side(c, false);
        let current = now_side(&p, &before);
        let reason = if expired {
            Some("expired")
        } else if c.reverted_by.is_some() {
            Some("changed_since")
        } else {
            undo_reason(&mut *conn, &a, c, kind, &p, &out).await?
        };
        out.push(UndoWikiPage {
            page_id: c.page_id,
            project_id: p.project_id,
            title: p.title.clone(),
            change: kind.into(),
            before,
            after,
            current,
            restorable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }
    Ok(out)
}

async fn undo_reason(
    conn: &mut PgConnection,
    a: &Access,
    c: &Change,
    kind: &str,
    p: &PageRow,
    earlier: &[UndoWikiPage],
) -> Result<Option<&'static str>> {
    let later = later(&mut *conn, c.page_id, c.after_lock_version).await?;
    let redacted = |t: &Option<String>| t.as_deref() == Some(crate::redaction::REDACTED);
    match kind {
        "created" | "restored" => {
            if !later.is_empty() || p.deleted_at.is_some() {
                return Ok(Some("changed_since"));
            }
            if !live_children(&mut *conn, p.id).await?.is_empty() {
                return Ok(Some("has_children"));
            }
        }
        "deleted" => {
            if !later.is_empty() || p.deleted_at.is_none() {
                return Ok(Some("changed_since"));
            }
            if let Some(o) = live_by_title(&mut *conn, p.project_id, &p.title).await?
                && o.id != p.id
            {
                return Ok(Some("title_taken"));
            }
            // The restore makes one more live page (R-W5 limit).
            let count: i32 =
                sqlx::query_scalar("SELECT page_count FROM wiki_settings WHERE project_id = $1")
                    .bind(p.project_id)
                    .fetch_one(&mut *conn)
                    .await?;
            if count >= MAX_PAGES {
                return Ok(Some("wiki_page_limit"));
            }
        }
        "reparented" => {
            let parent_ok = earlier
                .iter()
                .any(|e| e.page_id == c.before_parent.unwrap_or_default() && e.restorable);
            if later.iter().any(|l| l.parent_changed)
                || p.parent_id != c.after_parent
                || p.deleted_at.is_some()
            {
                return Ok(Some("changed_since"));
            }
            if !parent_ok {
                return Ok(Some("dependency"));
            }
            // R-W4: back under the restored page, at the depth its restore
            // puts it (as `restore_parent`), the child's subtree included.
            if let Some(bp) = c.before_parent {
                let parent: PageRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {PAGE_COLS} FROM wiki_pages WHERE id = $1"
                )))
                .bind(bp)
                .fetch_one(&mut *conn)
                .await?;
                let depth = match restore_parent(&mut *conn, &parent).await? {
                    Some(pp) => ancestors(&mut *conn, pp).await?.len() as i32 + 2,
                    None => 1,
                };
                if depth + height(&mut *conn, p.id).await? > MAX_DEPTH {
                    return Ok(Some("wiki_depth"));
                }
            }
        }
        _ => {
            if p.deleted_at.is_some() {
                return Ok(Some("changed_since"));
            }
            let content = c.before_version != Some(c.after_version);
            if content && p.current_version != c.after_version {
                return Ok(Some("changed_since"));
            }
            if c.before_title.is_some() {
                if redacted(&c.before_title) || redacted(&c.after_title) {
                    return Ok(Some("redacted"));
                }
                if later.iter().any(|l| l.before_title.is_some())
                    || Some(&p.title) != c.after_title.as_ref()
                {
                    return Ok(Some("changed_since"));
                }
            }
            if c.parent_changed
                && (later.iter().any(|l| l.parent_changed) || p.parent_id != c.after_parent)
            {
                return Ok(Some("changed_since"));
            }
            let prot = c.before_protected != Some(c.after_protected);
            if prot
                && (later
                    .iter()
                    .any(|l| l.before_protected != Some(l.after_protected))
                    || p.protected != c.after_protected)
            {
                return Ok(Some("changed_since"));
            }
            if content && let Some(v) = c.before_version {
                match version_body(&mut *conn, p.id, v).await? {
                    Some((_, true)) => return Ok(Some("redacted_source")),
                    None => return Ok(Some("changed_since")),
                    _ => {}
                }
                // The undo writes a new version (R-W3 limit).
                if p.current_version >= MAX_VERSIONS {
                    return Ok(Some("wiki_version_limit"));
                }
            }
            if let Some(t) = &c.before_title
                && let Some(o) = live_by_title(&mut *conn, p.project_id, t).await?
                && o.id != p.id
            {
                return Ok(Some("title_taken"));
            }
            if c.parent_changed
                && let Some(bp) = c.before_parent
            {
                let live: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM wiki_pages WHERE id = $1 AND deleted_at IS NULL)",
                )
                .bind(bp)
                .fetch_one(&mut *conn)
                .await?;
                let up = ancestors(&mut *conn, bp).await?;
                if !live || up.contains(&p.id) || bp == p.id {
                    return Ok(Some("value_no_longer_valid"));
                }
                // R-W4: the depth back under the old parent, subtree included.
                if up.len() as i32 + 1 + height(&mut *conn, p.id).await? > MAX_DEPTH {
                    return Ok(Some("wiki_depth"));
                }
            }
        }
    }
    let perms = undo_perm(kind, c);
    if !a.writable() || perms.iter().any(|x| !a.has(x)) {
        return Ok(Some("permission"));
    }
    if kind != "restored"
        && p.protected
        && !a.has(PROTECT_WIKI_PAGES)
        && undo_perm(kind, c) != vec![PROTECT_WIKI_PAGES]
    {
        return Ok(Some("permission"));
    }
    Ok(None)
}

/// `POST /executions/{id}/undo` of a wiki execution, in the caller's
/// transaction after the operation request: locks (R-W10: projects FOR
/// SHARE, their wiki settings FOR UPDATE, the pages by id), the preview
/// under them, then the restorable rows not skipped, as one new execution
/// that reverts `id` (and can itself be undone).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn undo_apply(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    request: Uuid,
    id: Uuid,
    expired_at: DateTime<Utc>,
    skip: &[Uuid],
    payload: &[u8; 32],
) -> Result<(usize, Uuid, String, Json)> {
    let pages: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT p.id, p.project_id FROM execution_wiki_changes c JOIN wiki_pages p ON p.id = c.page_id
         WHERE c.execution_id = $1 ORDER BY p.id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let projects: BTreeSet<Uuid> = pages.iter().map(|p| p.1).collect();
    for p in &projects {
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(p)
            .execute(&mut *conn)
            .await?;
    }
    for p in &projects {
        lock_settings(&mut *conn, *p).await?;
    }
    // Children a delete moved may have moved again: lock every page row
    // the undo may write, by id.
    let ids: Vec<Uuid> = pages.iter().map(|p| p.0).collect();
    sqlx::query("SELECT 1 FROM wiki_pages WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let expired = now - expired_at > chrono::Duration::days(crate::undo::UNDO_DAYS);
    let pv = undo_preview(&mut *conn, caller, id, expired).await?;
    // R-W5/R-W3 limits: the undo would break one, so it is refused as the
    // op would be (not a partial undo).
    for w in pv.iter().filter(|w| !skip.contains(&w.page_id)) {
        match w.reason.as_deref() {
            Some("wiki_page_limit") => {
                return Err(invalid_value(
                    "skip_wiki_pages",
                    "wiki_page_limit",
                    format!("a wiki holds at most {MAX_PAGES} pages"),
                ));
            }
            Some("wiki_version_limit") => {
                return Err(invalid_value(
                    "skip_wiki_pages",
                    "wiki_version_limit",
                    format!("a page keeps at most {MAX_VERSIONS} versions"),
                ));
            }
            _ => {}
        }
    }
    let rows: BTreeMap<Uuid, Change> = changes_of(&mut *conn, id)
        .await?
        .into_iter()
        .map(|c| (c.page_id, c))
        .collect();
    // A child goes back under the deleted page only when that page comes
    // back in this undo (not skipped, restorable); `pv` lists pages first.
    let mut todo: Vec<&UndoWikiPage> = Vec::new();
    let mut held: BTreeSet<Uuid> = BTreeSet::new();
    for w in &pv {
        if !w.restorable || skip.contains(&w.page_id) {
            continue;
        }
        if w.change == "reparented"
            && !rows[&w.page_id]
                .before_parent
                .is_some_and(|bp| todo.iter().any(|t| t.page_id == bp))
        {
            held.insert(w.page_id);
            continue;
        }
        todo.push(w);
    }
    let reasons: Json = pv
        .iter()
        .filter_map(|w| {
            let r = if held.contains(&w.page_id) {
                Some("dependency")
            } else {
                w.reason.as_deref()
            };
            r.map(|r| json!({"page_id": w.page_id, "field": w.change, "reason": r}))
        })
        .collect();
    if todo.is_empty() {
        return Ok((0, Uuid::nil(), String::new(), reasons));
    }
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    commands::check_deadline(&mut *conn, deadline).await?;
    let token = commands::search_token(&mut *conn).await?;
    let h: [u8; 32] = Sha256::digest(payload).into();
    let exec =
        commands::record_undo_execution(&mut *conn, request, who.user_id, &h, &token, id).await?;
    let at = Prov {
        user: who.user_id,
        request,
        exec,
    };
    for w in &todo {
        let c = &rows[&w.page_id];
        let before = lock_page(&mut *conn, w.page_id).await?;
        match w.change.as_str() {
            "created" | "restored" => {
                soft_delete(&mut *conn, before.id, before.project_id, exec).await?;
                insert_event(
                    &mut *conn,
                    before.id,
                    "deleted",
                    json!({"children": []}),
                    &at,
                )
                .await?;
            }
            "deleted" => {
                let parent = restore_parent(&mut *conn, &before).await?;
                undelete(&mut *conn, &before, parent).await?;
                insert_event(
                    &mut *conn,
                    before.id,
                    "restored",
                    json!({"parent_id": parent}),
                    &at,
                )
                .await?;
            }
            "reparented" => {
                set_parent(&mut *conn, before.id, c.before_parent).await?;
                insert_event(
                    &mut *conn,
                    before.id,
                    "moved",
                    json!({"from_parent": before.parent_id, "to_parent": c.before_parent}),
                    &at,
                )
                .await?;
            }
            _ => {
                let body = match c.before_version {
                    Some(v) if v != c.after_version => version_body(&mut *conn, before.id, v)
                        .await?
                        .map(|b| (b.0, v)),
                    _ => None,
                };
                let comment = body
                    .as_ref()
                    .map(|(_, v)| format!("復原：回到第 {v} 版"))
                    .unwrap_or_default();
                write_update(
                    &mut *conn,
                    &at,
                    &before,
                    body.as_ref().map(|(b, _)| (b.as_str(), comment.as_str())),
                    c.before_title.as_deref(),
                    c.parent_changed.then_some(c.before_parent),
                    false,
                )
                .await?;
                if c.before_protected != Some(c.after_protected) {
                    set_protected(
                        &mut *conn,
                        before.id,
                        c.before_protected.unwrap_or_default(),
                        &at,
                    )
                    .await?;
                }
            }
        }
        let after = lock_page(&mut *conn, w.page_id).await?;
        insert_change(
            &mut *conn,
            &Change::from_before(exec, &before).after(&after, &before.title),
        )
        .await?;
        sqlx::query(
            "UPDATE execution_wiki_changes SET reverted_by = $3 WHERE execution_id = $1 AND page_id = $2",
        )
        .bind(id)
        .bind(w.page_id)
        .bind(exec)
        .execute(&mut *conn)
        .await?;
    }
    Ok((todo.len(), exec, token, reasons))
}

// ---------------------------------------------------------------- D12 (§3.13)

/// The page a redaction route names: its id (a deleted page too, for
/// `manage_wiki`), else its title as in the page routes.
pub(crate) async fn page_for_redaction(
    conn: &mut PgConnection,
    a: &Access,
    seg: &str,
) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(seg.trim())
        && let Some(p) = page_by_id(&mut *conn, a.project, id).await?
        && (p.deleted_at.is_none() || a.has(MANAGE_WIKI))
    {
        return Ok(p.id);
    }
    Ok(find_for_read(&mut *conn, a.project, seg).await?.0.id)
}

/// The new title of a title redaction: valid (R-W2) and not another live
/// page's.
pub(crate) async fn check_redaction_title(
    conn: &mut PgConnection,
    project: Uuid,
    page: Uuid,
    new_title: &str,
) -> Result<()> {
    let t = checked_title("new_title", new_title)?;
    if t == crate::redaction::REDACTED {
        return Err(title_error("new_title", TitleError::Char('[')));
    }
    if let Some(o) = live_by_title(&mut *conn, project, &t).await?
        && o.id != page
    {
        return Err(page_exists(o.id));
    }
    Ok(())
}

/// Writes a wiki redaction (the caller holds the project `FOR UPDATE` and
/// the page `FOR UPDATE`; `row` is the page, or the event of
/// `WikiEvent`); returns the `wiki_redactions` id. The derived copies
/// follow in `redaction::derived`.
pub(crate) async fn redact(
    conn: &mut PgConnection,
    target: &crate::redaction::Target,
    row: Uuid,
    by: Uuid,
    reason_code: &str,
    reason: &str,
) -> Result<Uuid> {
    use crate::redaction::{REDACTED, Target};
    let rec = |page: Uuid, kind: &'static str, version: Option<i32>, event: Option<Uuid>| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO wiki_redactions (page_id, target, version, event_id, reason_code, reason, redacted_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(page)
        .bind(kind)
        .bind(version)
        .bind(event)
        .bind(reason_code)
        .bind(reason)
        .bind(by)
    };
    match target {
        Target::WikiVersion { version, .. } => {
            let id = rec(row, "version", Some(*version), None)
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE wiki_page_versions SET body = $3, comment = $3, redaction_id = $4
                 WHERE page_id = $1 AND version = $2",
            )
            .bind(row)
            .bind(version)
            .bind(REDACTED)
            .bind(id)
            .execute(&mut *conn)
            .await?;
            Ok(id)
        }
        Target::WikiEvent { event, .. } => {
            let page: Uuid = sqlx::query_scalar("SELECT page_id FROM wiki_events WHERE id = $1")
                .bind(event)
                .fetch_one(&mut *conn)
                .await?;
            let id = rec(page, "event", None, Some(*event))
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query(
                "UPDATE wiki_events SET payload = to_jsonb($2::text), redaction_id = $3 WHERE id = $1",
            )
            .bind(event)
            .bind(REDACTED)
            .bind(id)
            .execute(&mut *conn)
            .await?;
            Ok(id)
        }
        Target::WikiTitle { new_title, .. } => {
            let t = checked_title("new_title", new_title)?;
            let id = rec(row, "title", None, None).fetch_one(&mut *conn).await?;
            // Every event that holds a title of the page.
            sqlx::query(
                "UPDATE wiki_events SET payload = to_jsonb($2::text), redaction_id = $3
                 WHERE page_id = $1 AND kind IN ('created', 'renamed')",
            )
            .bind(row)
            .bind(REDACTED)
            .bind(id)
            .execute(&mut *conn)
            .await?;
            sqlx::query("DELETE FROM wiki_redirects WHERE page_id = $1")
                .bind(row)
                .execute(&mut *conn)
                .await?;
            let p = lock_page(&mut *conn, row).await?;
            let mut sp = conn.begin().await?;
            let r = sqlx::query(
                "UPDATE wiki_pages SET title = $2, title_key = wiki_title_key($2),
                        lock_version = lock_version + 1
                 WHERE id = $1",
            )
            .bind(row)
            .bind(&t)
            .execute(&mut *sp)
            .await;
            match r {
                Ok(_) => sp.commit().await?,
                Err(e) => {
                    drop(sp);
                    if unique_violation(&e) {
                        let other = live_by_title(&mut *conn, p.project_id, &t).await?;
                        return Err(page_exists(other.map(|p| p.id).unwrap_or_default()));
                    }
                    return Err(e.into());
                }
            }
            Ok(id)
        }
        _ => Err(anyhow::anyhow!("not a wiki redaction target")),
    }
}

// ---------------------------------------------------------------- links (R-RF4)

/// What a reader sees of wiki links: written value → link HTML pieces.
#[derive(Debug, Default)]
pub(crate) struct WikiLinks {
    /// written value → (href, class) for pages the reader may open or create.
    pub links: BTreeMap<String, (String, &'static str)>,
}

/// `[[KEY:title#section|text]]`'s target: (project key or None, title).
pub(crate) fn link_target(v: &str) -> Option<(Option<String>, String)> {
    let v = v.split('#').next().unwrap_or_default();
    let (key, title) = match v.split_once(':') {
        Some((k, t)) => {
            let ok = (2..=10).contains(&k.len())
                && k.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && k.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !ok {
                return None;
            }
            (Some(k.to_string()), t)
        }
        None => (None, v),
    };
    let title = normalize_title(title).ok()?;
    Some((key, title))
}

/// R-RF4: resolves `values` (as written) for `caller`, `project` being the
/// project the text belongs to.
pub(crate) async fn resolve_links(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
    values: &BTreeSet<String>,
) -> Result<WikiLinks> {
    let mut out = WikiLinks::default();
    let mut access_cache: BTreeMap<Uuid, Option<Access>> = BTreeMap::new();
    for v in values {
        let Some((key, title)) = link_target(v) else {
            continue;
        };
        let pid = match &key {
            None => Some(project),
            Some(k) => project_id(&mut *conn, k).await?,
        };
        let Some(pid) = pid else { continue };
        let a = match access_cache.get(&pid) {
            Some(a) => a.clone(),
            None => {
                let a = access_of(&mut *conn, caller, pid).await?;
                access_cache.insert(pid, a.clone());
                a
            }
        };
        let Some(a) = a else { continue };
        let exists = live_by_title(&mut *conn, pid, &title).await?.is_some()
            || {
                let r: Option<Uuid> = sqlx::query_scalar(
                "SELECT r.page_id FROM wiki_redirects r JOIN wiki_pages p ON p.id = r.page_id
                 WHERE r.project_id = $1 AND r.title_key = wiki_title_key($2) AND p.deleted_at IS NULL",
            )
            .bind(pid)
            .bind(&title)
            .fetch_optional(&mut *conn)
            .await?;
                r.is_some()
            };
        let href = format!("/projects/{}/wiki/{}", a.key, url_title(&title));
        if exists {
            out.links.insert(v.clone(), (href, "ox-wiki"));
        } else if a.may(EDIT_WIKI_PAGES) {
            out.links.insert(v.clone(), (href, "ox-wiki ox-wiki-new"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(normalize_title("  部署流程 ").unwrap(), "部署流程");
        assert_eq!(normalize_title(""), Err(TitleError::Length));
        assert_eq!(normalize_title(&"a".repeat(101)), Err(TitleError::Length));
        assert!(normalize_title(&"字".repeat(100)).is_ok());
        assert_eq!(normalize_title(".x"), Err(TitleError::LeadingDot));
        assert_eq!(
            normalize_title("a\u{202E}b"),
            Err(TitleError::Char('\u{202E}'))
        );
        assert_eq!(
            normalize_title("a\u{200B}b"),
            Err(TitleError::Char('\u{200B}'))
        );
        assert_eq!(normalize_title("a\tb"), Err(TitleError::Char('\t')));
        for c in FORBIDDEN {
            assert_eq!(
                normalize_title(&format!("a{c}b")),
                Err(TitleError::Char(*c))
            );
        }
        // NFC: e + combining acute is é.
        assert_eq!(normalize_title("e\u{301}").unwrap(), "\u{e9}");
    }

    #[test]
    fn word_pieces() {
        assert_eq!(words("abc def"), vec!["abc", " ", "def"]);
        assert_eq!(words("部署a1,b"), vec!["部", "署", "a1", ",", "b"]);
        assert_eq!(words(""), Vec::<&str>::new());
    }

    #[test]
    fn diff_lines_and_words() {
        let (h, t) = diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(!t);
        assert_eq!(h.len(), 1);
        let ops: Vec<&str> = h[0].lines.iter().map(|l| l.op.as_str()).collect();
        assert_eq!(ops, vec!["equal", "delete", "insert", "equal"]);
        assert!(h[0].lines[1].words.is_some());
        let (_, t) = diff("", &"x\n".repeat(3000));
        assert!(t);
    }

    #[test]
    fn links_parse() {
        assert_eq!(
            link_target("AUTH:部署#a"),
            Some((Some("AUTH".into()), "部署".into()))
        );
        assert_eq!(link_target("部署"), Some((None, "部署".into())));
        assert_eq!(link_target("x:y"), None);
    }

    #[test]
    fn url_titles() {
        assert_eq!(url_title("a b"), "a_b");
        assert_eq!(url_title("部"), "%E9%83%A8");
        assert_eq!(url_title("a&b"), "a%26b");
    }
}
