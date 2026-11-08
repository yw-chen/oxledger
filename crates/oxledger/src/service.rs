// SPDX-License-Identifier: GPL-2.0-or-later
//! Read-side use cases shared by MCP (and REST in M1): search, get, resolve.
//! Every query goes through the caller's per-request scope.

use crate::auth::AuthContext;
use crate::citations::{self, Source, TextField};
use crate::cursor::{self, Position};
use crate::db::{self, push_pred};
use crate::error::{ApiError, ErrorCode, api};
use crate::evidence::{self, CpText, FieldMatch, Highlight, Strength, Terms, Window};
use crate::filter::{self, FilterInput};
use crate::search::SearchIndex;
use crate::search::Wait;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::{IssueFacts, Pred};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Row};
use std::collections::HashMap;
use uuid::Uuid;

pub struct App {
    pub pool: PgPool,
    pub index: SearchIndex,
}

/// One read: a READ ONLY, REPEATABLE READ transaction, so the authorization
/// checks and the data they guard come from the same snapshot on the same
/// connection (contract 02 §3.7, §3.8 item 2).
pub(crate) async fn read_tx(app: &App) -> Result<sqlx::Transaction<'static, Postgres>> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// A bad read-tool parameter (`invalid_argument`, contract 02 §4.4).
pub(crate) fn user_err(msg: impl Into<String>) -> anyhow::Error {
    api(ErrorCode::InvalidArgument, msg)
}

const MAX_LIMIT: u32 = 100;
const DEFAULT_LIMIT: u32 = 50;
const SEARCH_CANDIDATES: usize = 500;
/// With free text, the ranked candidate list grows 500 → 2000 → 8000 until
/// enough candidates survive the SQL re-check (visibility drift, status and
/// custom field filters live only in SQL). Beyond this the result is truncated.
const MAX_SEARCH_CANDIDATES: usize = 8000;

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    /// Free text (BM25 over subject and description; Chinese works without word segmentation).
    pub q: Option<String>,
    /// Project key (e.g. "AUTH"), name, or alias.
    pub project: Option<String>,
    /// Tracker name or alias (e.g. "bug", "缺陷").
    pub tracker: Option<String>,
    /// Exact status name or alias.
    pub status: Option<String>,
    /// "open" (todo + doing), "todo", "doing" or "done".
    pub status_category: Option<String>,
    /// "me", "none", a user login/name, or a group name.
    pub assignee: Option<String>,
    /// Milestone (§17.15): a version id (any readable project), or — only with
    /// `project` — a version name or "next" (else `invalid_filter`,
    /// `reason: project_scope_required`).
    pub version: Option<String>,
    /// true: open issues whose due_date is before today (org time zone;
    /// `meta.today`). Not indexed, so no reindex when the day changes.
    pub overdue: Option<bool>,
    /// Custom field filter, e.g. {"and": [{"field": "Severity", "op": "in", "value": ["High", "Critical"]},
    /// {"field": "Estimate (h)", "op": "between", "value": [2, 8]}]}. Combined with the other filters by AND.
    #[schemars(with = "Option<FilterInput>")]
    pub filter: Option<serde_json::Value>,
    /// Max items, default 50, max 100.
    pub limit: Option<u32>,
    /// Opaque cursor from a previous `next_cursor`.
    pub cursor: Option<String>,
    /// With `q`: "none" (no snippets), "brief" (default: one ~60-character snippet per issue)
    /// or "full" (up to two ~160-character snippets per matching field).
    pub explain: Option<String>,
    /// Fields per item: key, subject, project, tracker, status, status_category, assignee,
    /// priority, is_private, created_at, updated_at, lock_version, cf:<custom field name>.
    /// `key` is always returned. Default: key, subject, project, tracker, status,
    /// status_category, assignee, is_private, updated_at.
    pub fields: Option<Vec<String>>,
    /// A write receipt's `search.consistent_after` token: with `q`, wait until
    /// the index includes that write. `search_lagging` is not a write failure:
    /// do not propose again or ask for approval again; use `get` to cite what
    /// was just written.
    pub consistent_after: Option<String>,
    /// How long to wait for `consistent_after`, 0–2000 ms (default 1000).
    pub wait_ms: Option<u32>,
    /// "return" (default: `outcome: "search_lagging"`, no items) or "error"
    /// (`search_not_caught_up`).
    pub on_lag: Option<String>,
}

/// One line of an issue list (children of an issue).
#[derive(Debug, Serialize, JsonSchema, FromRow)]
pub struct IssueBrief {
    pub key: String,
    pub subject: String,
    pub project: String,
    pub tracker: String,
    pub status: String,
    pub status_category: String,
    pub assignee: Option<String>,
    pub is_private: bool,
    pub updated_at: DateTime<Utc>,
    #[sqlx(skip)]
    #[serde(skip)]
    #[schemars(skip)]
    pub id: Uuid,
}

/// Which fields matched, recomputed from the current text (design 03 §3.4 step 6).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MatchInfo {
    /// subject and/or description.
    pub fields: Vec<String>,
    /// primary: a full term matched; fallback: only single CJK characters or identifier parts.
    pub strength: Strength,
}

/// A snippet of the current text and its citation (design 03 §3.6).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchEvidence {
    /// subject | description
    pub field: String,
    /// current | rechecked (the index may still rank by older text; the snippet is from the current text).
    pub evidence_status: String,
    /// The cited text, verbatim: pass it unchanged as `quote`.
    pub text: String,
    /// Ranges of `text` in code points, half-open.
    pub highlights: Vec<Highlight>,
    pub citation_id: String,
}

/// One search hit. Only the requested `fields` appear (and `key`).
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct SearchItem {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_category: Option<String>,
    /// null when unassigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_private: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// As re-checked in SQL (not the index): pass back as a precondition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_version: Option<i32>,
    /// Requested `cf:<name>` fields that apply to this issue and the caller may read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_fields: Option<Vec<CustomFieldValue>>,
    /// With `q`: position across pages (page 2 starts at limit + 1). Compare
    /// rank and `match.strength`; there is no score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// null without `q`.
    #[serde(rename = "match")]
    pub matched: Option<MatchInfo>,
    /// Snippets (empty without `q` or with explain=none).
    pub evidence: Vec<SearchEvidence>,
    #[serde(skip)]
    #[schemars(skip)]
    pub id: Uuid,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Consistency {
    /// eventual (free text, no token) | satisfied (the token's write is in the
    /// index) | lagging (it is not yet: no items) | not_applicable (SQL only).
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waited_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResult {
    /// "ok", or "search_lagging": the index has not caught up with
    /// `consistent_after` yet. Not a write failure and not "nothing found":
    /// do not propose again or ask for approval again; `get` reads the write.
    pub outcome: String,
    /// false when `truncated` or lagging.
    pub complete: bool,
    /// Only with `q`: the server stopped after ranking 8000 candidates without
    /// finding everything this page needs. More matches may exist (even if there
    /// is no `next_cursor`); narrow `q` or the filters.
    pub truncated: bool,
    pub consistency: Consistency,
    pub items: Vec<SearchItem>,
    pub next_cursor: Option<String>,
    /// Some snippets were left out to keep the response small (the item list is still complete).
    pub evidence_truncated: bool,
    /// Only with `overdue`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<SearchMeta>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchMeta {
    /// The date `overdue` compared with (R10, org time zone), YYYY-MM-DD.
    pub today: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IssueDetail {
    pub key: String,
    /// Set when the requested key is an old key of a moved issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_key: Option<String>,
    pub subject: String,
    pub description: String,
    pub project: String,
    pub tracker: String,
    pub status: String,
    pub status_category: String,
    pub author: String,
    pub assignee: Option<String>,
    pub is_private: bool,
    pub parent: Option<String>,
    pub children: Vec<IssueBrief>,
    pub start_date: Option<NaiveDate>,
    pub due_date: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Pass back as `expected_lock_version` when updating (M1).
    pub lock_version: i32,
    /// Statuses the caller may move this issue to (workflow), current first.
    pub allowed_statuses: Vec<String>,
    /// Custom fields that apply to this issue (tracker and project) and that the
    /// caller may read, in display order; `value` is null when empty.
    pub custom_fields: Vec<CustomFieldValue>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CustomFieldValue {
    pub name: String,
    /// int | float | date | string | text | list | user | bool | link
    #[serde(rename = "type")]
    pub field_type: String,
    /// Number, "YYYY-MM-DD", string or bool; list: {id, label, active}; user: {id, name}.
    pub value: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, JsonSchema, FromRow)]
pub struct Candidate {
    pub kind: String,
    pub id: Uuid,
    pub name: String,
    /// The label that matched (name, login, key or alias).
    pub matched: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ResolveResult {
    pub candidates: Vec<Candidate>,
    /// When nothing matched: valid values for the requested kind.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub valid_values: Vec<String>,
}

// ---------------------------------------------------------------- resolve

const KINDS: [&str; 4] = ["project", "tracker", "status", "principal"];

/// Name/alias lookup. `kind` = project | tracker | status | principal (user or group).
pub async fn resolve(
    app: &App,
    caller: AuthContext,
    text: &str,
    kind: Option<&str>,
) -> Result<ResolveResult> {
    let mut tx = read_tx(app).await?;
    let r = resolve_in(&mut tx, caller, text, kind).await?;
    tx.commit().await?;
    Ok(r)
}

pub(crate) async fn resolve_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    text: &str,
    kind: Option<&str>,
) -> Result<ResolveResult> {
    if let Some(k) = kind
        && !KINDS.contains(&k)
    {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("kind must be one of {}", KINDS.join(", ")),
        )
        .with_details(serde_json::json!({"path": "kind", "allowed": KINDS}))
        .into());
    }
    let scope = db::load_scope(&mut *conn, caller).await?;
    let visible_projects: Vec<Uuid> = scope.project_ids().collect();
    let t = text.trim();
    // Exact (case-insensitive) matches first; substring matches only if nothing exact.
    for exact in [true, false] {
        let cmp = if exact {
            "lower(x.label) = lower($1)"
        } else {
            "x.label ILIKE '%' || $1 || '%'"
        };
        let sql = format!(
            "SELECT DISTINCT kind, id, name, label AS matched FROM (
               SELECT 'project' AS kind, p.id, p.name, l.label FROM projects p
                 CROSS JOIN LATERAL (VALUES (p.key), (p.name)) AS l(label)
                 WHERE p.id = ANY($2)
               UNION ALL SELECT 'tracker', id, name, name FROM trackers
               UNION ALL SELECT 'status', id, name, name FROM issue_statuses
               UNION ALL SELECT 'principal', p.id, p.name, l.label FROM principals p
                 CROSS JOIN LATERAL (VALUES (p.name), (p.login)) AS l(label)
                 WHERE p.status = 'active' AND p.kind IN ('user', 'group') AND l.label IS NOT NULL
               UNION ALL SELECT a.entity_kind, a.entity_id, COALESCE(pr.name, t.name, s.name, pp.name), a.label
                 FROM aliases a
                 LEFT JOIN projects pr ON a.entity_kind = 'project' AND pr.id = a.entity_id AND pr.id = ANY($2)
                 LEFT JOIN trackers t ON a.entity_kind = 'tracker' AND t.id = a.entity_id
                 LEFT JOIN issue_statuses s ON a.entity_kind = 'status' AND s.id = a.entity_id
                 LEFT JOIN principals pp ON a.entity_kind = 'principal' AND pp.id = a.entity_id AND pp.kind IN ('user', 'group')
                 WHERE COALESCE(pr.name, t.name, s.name, pp.name) IS NOT NULL
             ) AS x(kind, id, name, label)
             WHERE {cmp} AND ($3::text IS NULL OR x.kind = $3)
             ORDER BY kind, name LIMIT 20"
        );
        let found: Vec<Candidate> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(t)
            .bind(&visible_projects)
            .bind(kind)
            .fetch_all(&mut *conn)
            .await?;
        if !found.is_empty() {
            return Ok(ResolveResult {
                candidates: found,
                valid_values: Vec::new(),
            });
        }
    }
    let valid_values = match kind {
        Some("tracker") => {
            sqlx::query_scalar("SELECT name FROM trackers ORDER BY position, name")
                .fetch_all(&mut *conn)
                .await?
        }
        Some("status") => {
            sqlx::query_scalar("SELECT name FROM issue_statuses ORDER BY position, name")
                .fetch_all(&mut *conn)
                .await?
        }
        Some("project") => {
            sqlx::query_scalar(
                "SELECT key || ' ' || name FROM projects WHERE id = ANY($1) ORDER BY key",
            )
            .bind(&visible_projects)
            .fetch_all(&mut *conn)
            .await?
        }
        _ => Vec::new(),
    };
    Ok(ResolveResult {
        candidates: Vec::new(),
        valid_values,
    })
}

/// `resolve` with `purpose: "assignee"` (§8.8): principals matching `text`
/// that an issue of `project` may be assigned to (the same function the
/// command check uses). Several → `ambiguous_reference` with the candidates;
/// none → the assignable names as `valid_values`.
pub async fn resolve_assignee(
    app: &App,
    caller: AuthContext,
    text: &str,
    project: &str,
) -> Result<ResolveResult> {
    let mut tx = read_tx(app).await?;
    let project = crate::describe::visible_project(&mut tx, caller, project).await?;
    let allowed = crate::describe::assignable(&mut tx, project, None).await?;
    let ids: Vec<Uuid> = allowed.iter().map(|a| a.id).collect();
    let t = text.trim();
    // Exact (case-insensitive) on name or login first, then substring, both
    // only among the assignable.
    let mut candidates = Vec::new();
    for cmp in [
        "lower(l.label) = lower($1)",
        // `%` and `_` in the text are literal.
        r"l.label ILIKE '%' || replace(replace(replace($1, '\', '\\'), '%', '\%'), '_', '\_') || '%'",
    ] {
        candidates = sqlx::query_as::<_, Candidate>(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT ON (p.name, p.id) 'principal' AS kind, p.id, p.name, l.label AS matched
               FROM principals p
               CROSS JOIN LATERAL (VALUES (p.name), (p.login)) AS l(label)
              WHERE p.id = ANY($2) AND l.label IS NOT NULL AND {cmp}
              ORDER BY p.name, p.id LIMIT 20"
        )))
        .bind(t)
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        if !candidates.is_empty() {
            break;
        }
    }
    tx.commit().await?;
    if candidates.len() > 1 {
        return Err(ApiError::new(
            ErrorCode::AmbiguousReference,
            format!("{} people match {text:?}; ask which one", candidates.len()),
        )
        .with_details(serde_json::json!({
            "path": "text",
            "candidates": candidates.iter().map(|c| serde_json::json!({"id": c.id, "name": c.name, "kind": c.kind})).collect::<Vec<_>>(),
        }))
        .into());
    }
    let valid_values = if candidates.is_empty() {
        allowed.into_iter().map(|a| a.name).collect()
    } else {
        Vec::new()
    };
    Ok(ResolveResult {
        candidates,
        valid_values,
    })
}

/// Resolves a filter value to exactly one id of `kind`, or explains why not.
pub(crate) async fn resolve_one(
    conn: &mut PgConnection,
    caller: AuthContext,
    kind: &str,
    text: &str,
) -> Result<Uuid> {
    let r = resolve_in(conn, caller, text, Some(kind)).await?;
    let mut ids: Vec<Uuid> = r.candidates.iter().map(|c| c.id).collect();
    ids.sort();
    ids.dedup();
    match ids.as_slice() {
        [one] => Ok(*one),
        // `resolve` only returns what the caller can see, so the lists below
        // reveal nothing hidden.
        [] if r.valid_values.is_empty() => Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("no {kind} matches {text:?}"),
        )
        .with_details(serde_json::json!({"path": kind}))
        .into()),
        [] => Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "no {kind} matches {text:?}; valid: {}",
                r.valid_values.join(", ")
            ),
        )
        .with_details(serde_json::json!({"path": kind, "allowed": r.valid_values}))
        .into()),
        _ => Err(ApiError::new(
            ErrorCode::AmbiguousReference,
            format!(
                "{kind} {text:?} is ambiguous: {}",
                r.candidates
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with_details(serde_json::json!({
            "path": kind,
            "candidates": r.candidates.iter().map(|c| serde_json::json!({"id": c.id, "name": c.name})).collect::<Vec<_>>(),
        }))
        .into()),
    }
}

// ---------------------------------------------------------------- search

const BRIEF_SELECT: &str = "SELECT i.id, k.key, i.subject, p.key AS project, t.name AS tracker,
        s.name AS status, s.category AS status_category, a.name AS assignee, i.is_private, i.updated_at
     FROM issues i
     JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
     JOIN projects p ON p.id = i.project_id
     JOIN trackers t ON t.id = i.tracker_id
     JOIN issue_statuses s ON s.id = i.status_id
     LEFT JOIN principals a ON a.id = i.assigned_to_id
     WHERE i.archived_at IS NULL AND ";

#[derive(FromRow)]
struct BriefRow {
    id: Uuid,
    #[sqlx(flatten)]
    brief: IssueBrief,
}

fn rows_to_briefs(rows: Vec<BriefRow>) -> Vec<IssueBrief> {
    rows.into_iter()
        .map(|r| IssueBrief {
            id: r.id,
            ..r.brief
        })
        .collect()
}

/// Fields a search item may carry (design 03 §3.6; `lock_version`,
/// `created_at`, `priority` for contract 02 用法 3). `cf:<name>` besides.
pub const SEARCH_FIELDS: &[&str] = &[
    "key",
    "subject",
    "project",
    "tracker",
    "status",
    "status_category",
    "assignee",
    "priority",
    "is_private",
    "created_at",
    "updated_at",
    "lock_version",
];
/// Without `fields`: the line the search returned before design 03.
const DEFAULT_SEARCH_FIELDS: &[&str] = &[
    "key",
    "subject",
    "project",
    "tracker",
    "status",
    "status_category",
    "assignee",
    "is_private",
    "updated_at",
];

const SEARCH_FROM: &str = " FROM issues i
     JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
     JOIN projects p ON p.id = i.project_id
     JOIN trackers t ON t.id = i.tracker_id
     JOIN issue_statuses s ON s.id = i.status_id
     LEFT JOIN principals a ON a.id = i.assigned_to_id
     LEFT JOIN issue_priorities pr ON pr.id = i.priority_id
     WHERE i.archived_at IS NULL AND ";

fn search_column(f: &str) -> Option<&'static str> {
    Some(match f {
        "subject" => "i.subject",
        "project" => "p.key AS project",
        "tracker" => "t.name AS tracker",
        "status" => "s.name AS status",
        "status_category" => "s.category AS status_category",
        "assignee" => "a.name AS assignee",
        "priority" => "pr.name AS priority",
        "is_private" => "i.is_private",
        "created_at" => "i.created_at",
        "updated_at" => "i.updated_at",
        "lock_version" => "i.lock_version",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Explain {
    None,
    Brief,
    Full,
}

/// `invalid_argument` for an unknown field; `allowed` lists the fixed fields
/// and the custom fields the caller may read (never hidden ones, §3.8).
pub(crate) fn unknown_field(path: &str, bad: &str, fixed: &[&str], cf: &[String]) -> anyhow::Error {
    let mut allowed: Vec<String> = fixed.iter().map(|s| s.to_string()).collect();
    allowed.extend(cf.iter().map(|n| format!("cf:{n}")));
    ApiError::new(ErrorCode::InvalidArgument, format!("unknown field {bad:?}"))
        .with_details(serde_json::json!({"path": path, "allowed": allowed}))
        .into()
}

/// Custom fields the caller may read somewhere: `visible` ones, plus (people
/// only) fields their roles may read and, for admins, all. Agents read
/// `visible = true` fields only (contract 02 §3.5).
pub(crate) async fn readable_cf_names(
    conn: &mut PgConnection,
    caller: AuthContext,
) -> Result<Vec<String>> {
    let actor = crate::perm::actor(&mut *conn, caller).await?;
    let (admin, ids): (bool, Vec<i32>) = if actor.is_agent() {
        (false, Vec::new())
    } else {
        (
            actor.is_admin,
            crate::custom_fields::readable_projects(&mut *conn, caller, &actor.principal_ids)
                .await?
                .into_keys()
                .collect(),
        )
    };
    Ok(sqlx::query_scalar(
        "SELECT name FROM field_defs WHERE index_state <> 'dropping'
           AND (visible OR $1 OR id = ANY($2)) ORDER BY position, name",
    )
    .bind(admin)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?)
}

/// Splits `fields` into built-in fields and `cf:` names, rejecting unknown ones.
async fn search_fields(
    conn: &mut PgConnection,
    caller: AuthContext,
    fields: Option<&Vec<String>>,
) -> Result<(Vec<&'static str>, Vec<String>)> {
    let Some(list) = fields else {
        return Ok((DEFAULT_SEARCH_FIELDS.to_vec(), Vec::new()));
    };
    let mut builtin: Vec<&'static str> = Vec::new();
    let mut cf: Vec<String> = Vec::new();
    let mut bad: Option<String> = None;
    for f in list {
        if let Some(name) = f.strip_prefix("cf:") {
            if !cf.iter().any(|c| c == name) {
                cf.push(name.to_string());
            }
        } else if let Some(k) = SEARCH_FIELDS.iter().find(|k| **k == f.as_str()) {
            if !builtin.contains(k) {
                builtin.push(k);
            }
        } else {
            bad = Some(f.clone());
            break;
        }
    }
    let readable = if bad.is_some() || !cf.is_empty() {
        readable_cf_names(&mut *conn, caller).await?
    } else {
        Vec::new()
    };
    if bad.is_none()
        && let Some(c) = cf.iter().find(|c| !readable.contains(c))
    {
        bad = Some(format!("cf:{c}"));
    }
    match bad {
        Some(b) => Err(unknown_field("fields", &b, SEARCH_FIELDS, &readable)),
        None => Ok((builtin, cf)),
    }
}

fn item_from_row(r: &PgRow, sel: &[&str]) -> Result<SearchItem> {
    let mut it = SearchItem {
        key: r.try_get("key")?,
        id: r.try_get("id")?,
        ..Default::default()
    };
    for f in sel {
        match *f {
            "subject" => it.subject = Some(r.try_get("subject")?),
            "project" => it.project = Some(r.try_get("project")?),
            "tracker" => it.tracker = Some(r.try_get("tracker")?),
            "status" => it.status = Some(r.try_get("status")?),
            "status_category" => it.status_category = Some(r.try_get("status_category")?),
            "assignee" => it.assignee = Some(r.try_get("assignee")?),
            "priority" => it.priority = Some(r.try_get("priority")?),
            "is_private" => it.is_private = Some(r.try_get("is_private")?),
            "created_at" => it.created_at = Some(r.try_get("created_at")?),
            "updated_at" => it.updated_at = Some(r.try_get("updated_at")?),
            "lock_version" => it.lock_version = Some(r.try_get("lock_version")?),
            _ => {}
        }
    }
    Ok(it)
}

/// One text field of a candidate: which, the raw text, its code points and
/// what the query matches in it.
type FieldText = (TextField, String, CpText, Option<FieldMatch>);

/// The current text of one candidate.
struct Texts {
    fields: Vec<FieldText>,
}

impl Texts {
    fn matched(&self) -> bool {
        self.fields.iter().any(|f| f.3.is_some())
    }
}

/// Snippet budget of a whole response (design 03 §3.5 step 4), in code points.
const BRIEF_BUDGET_PER_ITEM: usize = 120;
const FULL_BUDGET: usize = 8000;
const BRIEF_WINDOW: usize = 60;
const FULL_WINDOW: usize = 160;
const FULL_WINDOWS_PER_FIELD: usize = 2;

pub async fn search(app: &App, caller: AuthContext, p: SearchParams) -> Result<SearchResult> {
    // Consistency first, outside the read transaction (no connection held
    // while waiting; 03 §4.4).
    let waited = consistency_wait(app, caller, &p).await?;
    if let Some(Wait::Lagging { waited_ms }) = waited {
        // An invalid request is an input error, not "wait and retry": run it
        // once (results dropped) so its errors come back first.
        let mut tx = read_tx(app).await?;
        search_in(&mut tx, app, caller, p.clone()).await?;
        tx.commit().await?;
        if p.on_lag.as_deref() == Some("error") {
            return Err(ApiError::new(
                ErrorCode::SearchNotCaughtUp,
                "the search index has not caught up with consistent_after yet",
            )
            .with_retry_after(LAG_RETRY_MS)
            .into());
        }
        return Ok(SearchResult {
            outcome: "search_lagging".into(),
            complete: false,
            truncated: false,
            consistency: Consistency {
                status: "lagging".into(),
                reason: None,
                waited_ms: Some(waited_ms),
                retry_after_ms: Some(LAG_RETRY_MS),
            },
            items: Vec::new(),
            next_cursor: None,
            evidence_truncated: false,
            meta: None,
        });
    }
    let mut tx = read_tx(app).await?;
    let mut r = search_in(&mut tx, app, caller, p).await?;
    tx.commit().await?;
    if let Some(Wait::Satisfied { waited_ms }) = waited {
        r.consistency = Consistency {
            status: "satisfied".into(),
            reason: None,
            waited_ms: Some(waited_ms),
            retry_after_ms: None,
        };
    }
    Ok(r)
}

const LAG_RETRY_MS: u64 = 500;
const MAX_WAIT_MS: u32 = 2000;
const DEFAULT_WAIT_MS: u32 = 1000;

/// Parses `c1.<data_epoch base64url>.<xid8>` (03 §4.3).
pub fn parse_token(t: &str) -> Option<(Uuid, u64)> {
    use base64::Engine as _;
    let mut parts = t.split('.');
    let (Some("c1"), Some(e), Some(x), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(e)
        .ok()?;
    let epoch = Uuid::from_slice(&bytes).ok()?;
    if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((epoch, x.parse().ok()?))
}

/// Validates the consistency parameters; with free text and a token, waits.
/// `None`: no token, or SQL only (`not_applicable`, no wait).
async fn consistency_wait(
    app: &App,
    caller: AuthContext,
    p: &SearchParams,
) -> Result<Option<Wait>> {
    let wait_ms = p.wait_ms.unwrap_or(DEFAULT_WAIT_MS);
    if wait_ms > MAX_WAIT_MS {
        return Err(user_err(format!(
            "wait_ms {wait_ms}: use 0..={MAX_WAIT_MS}"
        )));
    }
    match p.on_lag.as_deref() {
        None | Some("return") | Some("error") => {}
        Some(other) => return Err(user_err(format!("on_lag {other:?}: use return or error"))),
    }
    let Some(t) = p.consistent_after.as_deref() else {
        return Ok(None);
    };
    let Some((epoch, x)) = parse_token(t.trim()) else {
        return Err(ApiError::new(
            ErrorCode::InvalidConsistencyToken,
            "consistent_after is not a token from a write receipt; search again without it",
        )
        .with_details(serde_json::json!({"path": "consistent_after"}))
        .into());
    };
    let current: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
        .fetch_one(&app.pool)
        .await?;
    if epoch != current {
        return Err(api(
            ErrorCode::ConsistencyEpochChanged,
            "the search data was reset since this token; search again without consistent_after \
             (the write itself is not affected)",
        ));
    }
    if !p.q.as_deref().is_some_and(|q| !q.trim().is_empty()) {
        // SQL reads its own commits (03 §4.4): not_applicable, no wait.
        return Ok(None);
    }
    Ok(Some(
        app.index
            .wait_for(
                caller.principal_id(),
                x,
                std::time::Duration::from_millis(wait_ms as u64),
            )
            .await,
    ))
}

async fn search_in(
    conn: &mut PgConnection,
    app: &App,
    caller: AuthContext,
    p: SearchParams,
) -> Result<SearchResult> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let visible = scope.predicate();
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
    let explain = match p.explain.as_deref() {
        None | Some("brief") => Explain::Brief,
        Some("none") => Explain::None,
        Some("full") => Explain::Full,
        Some(other) => {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                format!("explain {other:?}: use none, brief or full"),
            )
            .with_details(
                serde_json::json!({"path": "explain", "allowed": ["none", "brief", "full"]}),
            )
            .into());
        }
    };
    let (sel, cf_sel) = search_fields(&mut *conn, caller, p.fields.as_ref()).await?;
    let text = p.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    // §3.4 step 1: non-blank text without any term must not list everything.
    let terms = text.map(Terms::new);
    if terms.as_ref().is_some_and(Terms::is_empty) {
        return Err(ApiError::new(
            ErrorCode::InvalidQuery,
            "q has no searchable words or characters (only punctuation?)",
        )
        .with_details(serde_json::json!({
            "suggestion": "use words, identifiers or CJK characters, or leave q out to list by filters"
        }))
        .into());
    }
    // The cursor is bound to who asked, what was asked and the order (§4.5).
    let qhash = cursor::query_hash(&serde_json::json!({
        "q": text, "project": &p.project, "tracker": &p.tracker, "status": &p.status,
        "status_category": &p.status_category, "assignee": &p.assignee, "filter": &p.filter,
        "version": &p.version, "overdue": &p.overdue,
        "order": if text.is_some() { "rank" } else { "updated_desc" },
    }));
    let pos = match p.cursor.as_deref() {
        None => None,
        Some(c) => Some(cursor::decode(&app.pool, c, caller.principal_id(), &qhash).await?),
    };
    let after: Option<(DateTime<Utc>, Uuid)> = match (&pos, text) {
        (Some(Position::Keyset { t, i }), None) => Some((*t, *i)),
        (None, _) | (Some(Position::Rank { .. }), Some(_)) => None,
        _ => {
            return Err(api(
                ErrorCode::InvalidCursor,
                "invalid cursor; search again without it",
            ));
        }
    };
    let offset: i64 = match pos {
        Some(Position::Rank { o }) => o as i64,
        _ => 0,
    };

    let mut filters: Vec<Pred> = Vec::new();
    let mut sql_extra: Vec<(&str, Uuid)> = Vec::new();
    let mut project_id = None;
    if let Some(v) = p.project.as_deref() {
        let id = resolve_one(&mut *conn, caller, "project", v).await?;
        project_id = Some(id);
        filters.push(Pred::ProjectIs(id));
    }
    if let Some(v) = p.version.as_deref() {
        // §17.15: names and `next` exist only inside one project; ids do not.
        let id = match Uuid::parse_str(v.trim()) {
            Ok(id) => id,
            Err(_) => {
                let Some(project) = project_id else {
                    return Err(ApiError::new(
                        ErrorCode::InvalidFilter,
                        "version by name or next needs exactly one project (or use the version id)",
                    )
                    .with_details(
                        serde_json::json!({"path": "version", "reason": "project_scope_required"}),
                    )
                    .into());
                };
                crate::versions::resolve_for_search(&mut *conn, project, v).await?
            }
        };
        sql_extra.push(("i.fixed_version_id = ", id));
    }
    let today = match p.overdue {
        Some(true) => Some(crate::versions::today(&mut *conn).await?),
        _ => None,
    };
    if let Some(v) = p.tracker.as_deref() {
        filters.push(Pred::TrackerIn(vec![
            resolve_one(&mut *conn, caller, "tracker", v).await?,
        ]));
    }
    if let Some(v) = p.status.as_deref() {
        sql_extra.push((
            "i.status_id = ",
            resolve_one(&mut *conn, caller, "status", v).await?,
        ));
    }
    let categories: Option<Vec<String>> = match p.status_category.as_deref() {
        None => None,
        Some("open") => Some(vec!["todo".into(), "doing".into()]),
        Some(c @ ("todo" | "doing" | "done")) => Some(vec![c.into()]),
        Some(other) => {
            return Err(user_err(format!(
                "status_category {other:?}: use open, todo, doing or done"
            )));
        }
    };
    let mut assignee_none = false;
    match p.assignee.as_deref() {
        None => {}
        Some("none") => assignee_none = true,
        Some("me") => match caller.user_id() {
            Some(me) => filters.push(Pred::AssigneeIn(vec![me])),
            None => return Err(user_err("assignee=me needs an authenticated caller")),
        },
        Some(v) => filters.push(Pred::AssigneeIn(vec![
            resolve_one(&mut *conn, caller, "principal", v).await?,
        ])),
    }

    let custom = match &p.filter {
        None => None,
        Some(v) => {
            Some(filter::resolve(&mut *conn, caller, &scope, &filter::parse_input(v)?).await?)
        }
    };

    let mut all = vec![visible];
    all.extend(filters);
    let pred = Pred::And(all).simplify();

    let push_where = |qb: &mut QueryBuilder<Postgres>| {
        push_pred(qb, "i", &pred);
        for (frag, id) in &sql_extra {
            qb.push(" AND ").push(*frag).push_bind(*id);
        }
        if let Some(c) = &categories {
            qb.push(" AND s.category = ANY(")
                .push_bind(c.clone())
                .push(")");
        }
        if assignee_none {
            qb.push(" AND i.assigned_to_id IS NULL");
        }
        // §17.15 overdue: computed per query, never indexed.
        if let Some(t) = today {
            qb.push(" AND i.due_date < ")
                .push_bind(t)
                .push(" AND s.category <> 'done'");
        }
        // Custom fields live only in PostgreSQL.
        if let Some(f) = &custom {
            qb.push(" AND ");
            filter::push_filter(qb, "i", f);
        }
    };
    // Only the requested columns (§3.8 "fields 下推"), plus what identifies,
    // orders and authorizes a row.
    let mut select =
        String::from("SELECT i.id, k.key, i.updated_at AS sort_at, i.project_id, i.tracker_id");
    for f in &sel {
        if let Some(c) = search_column(f) {
            select.push_str(", ");
            select.push_str(c);
        }
    }
    select.push_str(SEARCH_FROM);

    let mut truncated = false;
    let mut checked: HashMap<Uuid, Texts> = HashMap::new();
    let (rows, more): (Vec<PgRow>, bool) = match (text, &terms) {
        (Some(q), Some(terms)) => {
            // Free text: rank in the index (already filtered by visibility +
            // the index-side filters), re-check every candidate in SQL — the
            // index may lag the database, and some filters exist only in SQL —
            // then compare the current text in batches (design 03 §3.4): a
            // candidate whose text no longer matches is dropped whatever
            // `explain` says. Grow the candidate list until this page and one
            // lookahead item survive. Best effort: each round asks the index
            // again, so a concurrent index update can shift the ranking.
            let need = (offset + limit + 1) as usize;
            let batch = 2 * limit as usize;
            let mut k = SEARCH_CANDIDATES;
            let survivors = loop {
                let hits = app.index.search(q, &pred, k).map_err(|e| {
                    tracing::error!("search index failed: {e:#}");
                    api(
                        ErrorCode::SearchUnavailable,
                        "the search index is not available right now; try again later",
                    )
                })?;
                let exhausted = hits.len() < k;
                let score_of: HashMap<Uuid, f32> =
                    hits.iter().map(|h| (h.issue_id, h.score)).collect();
                let ids: Vec<Uuid> = score_of.keys().copied().collect();
                let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT i.id, k.key");
                qb.push(SEARCH_FROM);
                push_where(&mut qb);
                qb.push(" AND i.id = ANY(").push_bind(ids).push(")");
                let mut ranked: Vec<(Uuid, String)> =
                    qb.build_query_as().fetch_all(&mut *conn).await?;
                ranked.sort_by(|a, b| {
                    score_of[&b.0]
                        .partial_cmp(&score_of[&a.0])
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.1.cmp(&b.1))
                });
                let mut survivors: Vec<Uuid> = Vec::new();
                let mut i = 0;
                while survivors.len() < need && i < ranked.len() {
                    let end = (i + batch).min(ranked.len());
                    let todo: Vec<Uuid> = ranked[i..end]
                        .iter()
                        .map(|r| r.0)
                        .filter(|id| !checked.contains_key(id))
                        .collect();
                    if !todo.is_empty() {
                        let texts: Vec<(Uuid, String, String)> = sqlx::query_as(
                            "SELECT id, subject, description FROM issues WHERE id = ANY($1)",
                        )
                        .bind(&todo)
                        .fetch_all(&mut *conn)
                        .await?;
                        for (id, subject, description) in texts {
                            let fields = [
                                (TextField::Subject, subject),
                                (TextField::Description, description),
                            ]
                            .into_iter()
                            .map(|(f, t)| {
                                let cp = CpText::new(&t);
                                let m = evidence::match_field(terms, &t, &cp);
                                (f, t, cp, m)
                            })
                            .collect();
                            checked.insert(id, Texts { fields });
                        }
                    }
                    survivors.extend(
                        ranked[i..end]
                            .iter()
                            .filter(|r| checked.get(&r.0).is_some_and(Texts::matched))
                            .map(|r| r.0),
                    );
                    i = end;
                }
                if survivors.len() >= need || exhausted {
                    break survivors;
                }
                if k >= MAX_SEARCH_CANDIDATES {
                    truncated = true;
                    break survivors;
                }
                k = (k * 4).min(MAX_SEARCH_CANDIDATES);
            };
            let more = survivors.len() as i64 > offset + limit;
            let page: Vec<Uuid> = survivors
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect();
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(select.as_str());
            push_where(&mut qb);
            qb.push(" AND i.id = ANY(")
                .push_bind(page.clone())
                .push(")");
            let fetched = qb.build().fetch_all(&mut *conn).await?;
            let mut by_id: HashMap<Uuid, PgRow> = HashMap::new();
            for r in fetched {
                by_id.insert(r.try_get("id")?, r);
            }
            (
                page.iter().filter_map(|id| by_id.remove(id)).collect(),
                more,
            )
        }
        _ => {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(select.as_str());
            push_where(&mut qb);
            // Keyset: live paging, newest first (§4.5).
            if let Some((t, id)) = after {
                qb.push(" AND (i.updated_at, i.id) < (")
                    .push_bind(t)
                    .push(", ")
                    .push_bind(id)
                    .push(")");
            }
            qb.push(" ORDER BY i.updated_at DESC, i.id DESC LIMIT ")
                .push_bind(limit + 1);
            let mut rows = qb.build().fetch_all(&mut *conn).await?;
            let more = rows.len() as i64 > limit;
            rows.truncate(limit as usize);
            (rows, more)
        }
    };

    let mut items = Vec::with_capacity(rows.len());
    let mut cf_roles: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (n, r) in rows.iter().enumerate() {
        let mut it = item_from_row(r, &sel)?;
        if !cf_sel.is_empty() {
            let (project_id, tracker_id): (Uuid, Uuid) =
                (r.try_get("project_id")?, r.try_get("tracker_id")?);
            if let std::collections::hash_map::Entry::Vacant(e) = cf_roles.entry(project_id) {
                e.insert(db::role_ids_for(&mut *conn, caller, project_id).await?);
            }
            it.custom_fields = Some(
                custom_field_values(
                    &mut *conn,
                    caller,
                    &cf_roles[&project_id],
                    CfTarget {
                        issue_id: it.id,
                        tracker_id,
                        project_id,
                    },
                    Some(cf_sel.as_slice()),
                )
                .await?,
            );
        }
        if text.is_some() {
            it.rank = Some((offset as usize + n + 1) as u32);
            if let Some(t) = checked.get(&it.id) {
                let matched: Vec<&FieldText> = t.fields.iter().filter(|f| f.3.is_some()).collect();
                it.matched = Some(MatchInfo {
                    fields: matched.iter().map(|f| f.0.as_str().to_string()).collect(),
                    strength: matched
                        .iter()
                        .filter_map(|f| f.3.as_ref().map(|m| m.strength))
                        .max()
                        .unwrap_or(Strength::Fallback),
                });
            }
        }
        items.push(it);
    }

    let evidence_truncated = if text.is_some() && explain != Explain::None {
        attach_evidence(&mut items, &checked, explain, limit as usize)
    } else {
        false
    };

    let next_cursor = match (more, text, rows.last()) {
        (true, None, Some(last)) => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                Position::Keyset {
                    t: last.try_get("sort_at")?,
                    i: last.try_get("id")?,
                },
            )
            .await?,
        ),
        (true, Some(_), _) => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                Position::Rank {
                    o: (offset + limit) as u32,
                },
            )
            .await?,
        ),
        _ => None,
    };
    Ok(SearchResult {
        outcome: "ok".into(),
        complete: !truncated,
        truncated,
        consistency: match text {
            Some(_) => Consistency {
                status: "eventual".into(),
                reason: None,
                waited_ms: None,
                retry_after_ms: None,
            },
            None => Consistency {
                status: "not_applicable".into(),
                reason: Some("sql_only".into()),
                waited_ms: None,
                retry_after_ms: None,
            },
        },
        items,
        next_cursor,
        evidence_truncated,
        meta: today.map(|t| SearchMeta {
            today: t.to_string(),
        }),
    })
}

/// Snippets within the response budget: every item's best snippet first,
/// then the others; a snippet that does not fit is left out and reported
/// (design 03 §3.5). Citations only for snippets that are returned. Returns
/// `evidence_truncated`.
fn attach_evidence(
    items: &mut [SearchItem],
    checked: &HashMap<Uuid, Texts>,
    explain: Explain,
    limit: usize,
) -> bool {
    // Per item: (field index, window), best first.
    let mut plans: Vec<Vec<(usize, Window)>> = Vec::with_capacity(items.len());
    for it in items.iter() {
        let Some(t) = checked.get(&it.id) else {
            plans.push(Vec::new());
            continue;
        };
        let mut plan: Vec<(usize, Window)> = Vec::new();
        for (fi, (_, _, cp, m)) in t.fields.iter().enumerate() {
            let Some(m) = m else { continue };
            match explain {
                Explain::Brief => {
                    if let Some(w) = evidence::windows(cp, m, BRIEF_WINDOW, 1).first() {
                        plan.push((fi, *w));
                    }
                }
                _ => plan.extend(
                    evidence::windows(cp, m, FULL_WINDOW, FULL_WINDOWS_PER_FIELD)
                        .into_iter()
                        .map(|w| (fi, w)),
                ),
            }
        }
        if explain == Explain::Brief {
            // One snippet: primary first, most distinct terms, subject before description.
            let strength = |fi: usize| t.fields[fi].3.as_ref().map(|m| m.strength);
            plan.sort_by(|a, b| {
                strength(b.0)
                    .cmp(&strength(a.0))
                    .then(b.1.score.cmp(&a.1.score))
                    .then(a.0.cmp(&b.0))
                    .then(a.1.start.cmp(&b.1.start))
            });
            plan.truncate(1);
        }
        plans.push(plan);
    }
    let mut budget = match explain {
        Explain::Brief => limit * BRIEF_BUDGET_PER_ITEM,
        _ => FULL_BUDGET,
    };
    let mut truncated = false;
    let mut keep: Vec<Vec<bool>> = plans.iter().map(|p| vec![false; p.len()]).collect();
    for first in [true, false] {
        for (ii, plan) in plans.iter().enumerate() {
            for (pi, (_, w)) in plan.iter().enumerate() {
                if (pi == 0) != first {
                    continue;
                }
                let len = w.end - w.start;
                if len <= budget {
                    budget -= len;
                    keep[ii][pi] = true;
                } else {
                    truncated = true;
                }
            }
        }
    }
    for (ii, plan) in plans.iter().enumerate() {
        let Some(t) = checked.get(&items[ii].id) else {
            continue;
        };
        let issue_id = items[ii].id;
        items[ii].evidence = plan
            .iter()
            .enumerate()
            .filter(|(pi, _)| keep[ii][*pi])
            .filter_map(|(_, (fi, w))| {
                let (field, raw, cp, m) = &t.fields[*fi];
                let m = m.as_ref()?;
                Some(SearchEvidence {
                    field: field.as_str().into(),
                    // No search_rev yet (design 03 §7 step 8): always re-checked.
                    evidence_status: "rechecked".into(),
                    text: cp.slice(w.start, w.end),
                    highlights: evidence::highlights_in(m, (w.start, w.end))
                        .into_iter()
                        .map(|(s, e)| Highlight {
                            start: s,
                            end: e,
                            strength: m.strength,
                        })
                        .collect(),
                    citation_id: citations::cite(
                        Source::IssueField {
                            issue_id,
                            field: *field,
                        },
                        raw,
                        w.start,
                        w.end,
                    ),
                })
            })
            .collect();
    }
    truncated
}

// ---------------------------------------------------------------- get

#[derive(FromRow)]
struct DetailRow {
    id: Uuid,
    current_key: String,
    project_id: Uuid,
    tracker_id: Uuid,
    status_id: Uuid,
    author_id: Uuid,
    created_on_behalf_of_id: Option<Uuid>,
    assigned_to_id: Option<Uuid>,
    is_private: bool,
    subject: String,
    description: String,
    project: String,
    tracker: String,
    status: String,
    status_category: String,
    author: String,
    assignee: Option<String>,
    parent: Option<String>,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    lock_version: i32,
}

pub async fn get(app: &App, caller: AuthContext, key: &str) -> Result<IssueDetail> {
    let mut tx = read_tx(app).await?;
    let (_, r) = get_in(&mut tx, caller, key).await?;
    tx.commit().await?;
    Ok(r)
}

/// The full current detail and the issue id (shared with `issue_view`).
pub(crate) async fn get_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
) -> Result<(Uuid, IssueDetail)> {
    let key = key.trim().to_uppercase();
    let not_found = || api(ErrorCode::NotFound, format!("no issue {key}"));
    // Keys are PROJECT-123; anything else cannot exist (and must not reach SQL
    // as text PostgreSQL rejects, which would look different from not_found).
    let well_formed = crate::describe::well_formed_key(&key);
    // The caller's scope comes first, whether or not the key exists, so a
    // missing key does not answer measurably faster than an invisible one.
    let scope = db::load_scope(&mut *conn, caller).await?;
    if !well_formed {
        return Err(not_found());
    }
    let row: Option<DetailRow> = sqlx::query_as(
        "SELECT i.id, ck.key AS current_key, i.project_id, i.tracker_id, i.status_id, i.author_id,
                i.created_on_behalf_of_id,
                i.assigned_to_id, i.is_private, i.subject, i.description,
                p.name AS project, t.name AS tracker, s.name AS status, s.category AS status_category,
                au.name AS author, a.name AS assignee, pk.key AS parent,
                i.start_date, i.due_date, i.created_at, i.updated_at, i.lock_version
         FROM issue_keys rk
         JOIN issues i ON i.id = rk.issue_id
         JOIN issue_keys ck ON ck.issue_id = i.id AND ck.is_current
         JOIN projects p ON p.id = i.project_id
         JOIN trackers t ON t.id = i.tracker_id
         JOIN issue_statuses s ON s.id = i.status_id
         JOIN principals au ON au.id = i.author_id
         LEFT JOIN principals a ON a.id = i.assigned_to_id
         LEFT JOIN issue_keys pk ON pk.issue_id = i.parent_id AND pk.is_current
         WHERE rk.key = $1 AND i.archived_at IS NULL",
    )
    .bind(&key)
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or_else(not_found)?;

    let facts = IssueFacts {
        project_id: row.project_id,
        tracker_id: row.tracker_id,
        is_private: row.is_private,
        author_id: row.author_id,
        assigned_to_id: row.assigned_to_id,
    };
    // Invisible and missing look the same, so keys cannot be probed.
    if !scope.can_see(&facts) {
        return Err(not_found());
    }
    let visible = scope.predicate();
    // The parent is only named when the caller can see it.
    let parent = match &row.parent {
        None => None,
        Some(pk) => {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
                "SELECT EXISTS (SELECT 1 FROM issues i JOIN issue_keys k ON k.issue_id = i.id WHERE k.key = ",
            );
            qb.push_bind(pk.clone()).push(" AND ");
            push_pred(&mut qb, "i", &visible);
            qb.push(")");
            let ok: bool = qb.build_query_scalar().fetch_one(&mut *conn).await?;
            ok.then(|| pk.clone())
        }
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(BRIEF_SELECT);
    qb.push("i.parent_id = ").push_bind(row.id).push(" AND ");
    push_pred(&mut qb, "i", &visible);
    qb.push(" ORDER BY k.number");
    let children = rows_to_briefs(qb.build_query_as().fetch_all(&mut *conn).await?);

    let role_ids = db::role_ids_for(&mut *conn, caller, row.project_id).await?;
    // Same answer as describe's issue actions (§5.2): workflow roles, the
    // transition conditions, edit permission and (agents) the delegation.
    let actor = crate::perm::actor(&mut *conn, caller).await?;
    let allowed_statuses =
        match crate::perm::project_perms(&mut *conn, &actor, row.project_id).await? {
            None => Vec::new(),
            Some(pp) => {
                let core = crate::describe::IssueCore {
                    id: row.id,
                    key: row.current_key.clone(),
                    project_id: row.project_id,
                    tracker_id: row.tracker_id,
                    status_id: row.status_id,
                    author_id: row.author_id,
                    created_on_behalf_of_id: row.created_on_behalf_of_id,
                    assigned_to_id: row.assigned_to_id,
                    is_private: row.is_private,
                    lock_version: row.lock_version,
                };
                crate::describe::allowed_transitions(&mut *conn, &actor, &pp, &core)
                    .await?
                    .into_iter()
                    .map(|(_, name)| name)
                    .collect()
            }
        };
    let custom_fields = custom_field_values(
        &mut *conn,
        caller,
        &role_ids,
        CfTarget {
            issue_id: row.id,
            tracker_id: row.tracker_id,
            project_id: row.project_id,
        },
        None,
    )
    .await?;

    Ok((
        row.id,
        IssueDetail {
            requested_key: (row.current_key != key).then_some(key.clone()),
            key: row.current_key,
            subject: row.subject,
            description: row.description,
            project: row.project,
            tracker: row.tracker,
            status: row.status,
            status_category: row.status_category,
            author: row.author,
            assignee: row.assignee,
            is_private: row.is_private,
            parent,
            children,
            start_date: row.start_date,
            due_date: row.due_date,
            created_at: row.created_at,
            updated_at: row.updated_at,
            lock_version: row.lock_version,
            allowed_statuses,
            custom_fields,
        },
    ))
}

#[derive(FromRow)]
struct CfRow {
    name: String,
    ftype: String,
    v_num: Option<f64>,
    v_date: Option<NaiveDate>,
    v_text: Option<String>,
    v_ref: Option<Uuid>,
    v_bool: Option<bool>,
    option_label: Option<String>,
    option_active: Option<bool>,
    principal_name: Option<String>,
}

/// Redmine `Issue#visible_custom_field_values`: fields available for the
/// issue (tracker ∩ project) that the caller may read, with their values.
/// Values of fields that no longer apply stay stored but are not shown.
/// `roles` are the caller's effective roles in the issue's project
/// (`db::role_ids_for`), the same rule filtering uses
/// (`custom_fields::readable_projects`).
/// Which issue [`custom_field_values`] reads.
#[derive(Clone, Copy)]
pub(crate) struct CfTarget {
    pub issue_id: Uuid,
    pub tracker_id: Uuid,
    pub project_id: Uuid,
}

/// `only`: just these field names (`fields: ["cf:…"]`); other values are not read.
pub(crate) async fn custom_field_values(
    conn: &mut PgConnection,
    caller: AuthContext,
    roles: &[Uuid],
    row: CfTarget,
    only: Option<&[String]>,
) -> Result<Vec<CustomFieldValue>> {
    let me = db::effective(&mut *conn, caller).await?;
    let admin = me.is_admin;
    // Agents: visible = true fields only (contract 02 §3.5); the delegator's
    // roles decide workflow, not what the agent may read.
    let roles: &[Uuid] = if me.agent.is_some() { &[] } else { roles };
    let rows: Vec<CfRow> = sqlx::query_as(
        "SELECT d.name, d.ftype, cf.v_num::float8 AS v_num, cf.v_date, cf.v_text, cf.v_ref, cf.v_bool,
                o.label AS option_label, o.active AS option_active, pr.name AS principal_name
         FROM field_defs d
         JOIN field_trackers ft ON ft.field_id = d.id AND ft.tracker_id = $2
         LEFT JOIN issue_cf cf ON cf.issue_id = $1 AND cf.field_id = d.id
         LEFT JOIN field_options o ON d.ftype = 'list' AND o.id = cf.v_ref
         LEFT JOIN principals pr ON d.ftype = 'user' AND pr.id = cf.v_ref
         WHERE d.index_state <> 'dropping'
           AND (d.is_for_all OR EXISTS (SELECT 1 FROM field_projects fp
                                        WHERE fp.field_id = d.id AND fp.project_id = $3))
           AND (d.visible OR $4 OR EXISTS (
                 SELECT 1 FROM field_roles fr
                 WHERE fr.field_id = d.id AND fr.role_id = ANY($5)))
           AND ($6::text[] IS NULL OR d.name = ANY($6))
         ORDER BY d.position, d.name",
    )
    .bind(row.issue_id)
    .bind(row.tracker_id)
    .bind(row.project_id)
    .bind(admin)
    .bind(roles)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            use serde_json::json;
            let value = match r.ftype.as_str() {
                "int" => r.v_num.map(|n| json!(n as i64)),
                "float" => r.v_num.map(|n| json!(n)),
                "date" => r.v_date.map(|d| json!(d.to_string())),
                "bool" => r.v_bool.map(|b| json!(b)),
                "list" => r.v_ref.map(
                    |id| json!({"id": id, "label": r.option_label, "active": r.option_active}),
                ),
                "user" => r
                    .v_ref
                    .map(|id| json!({"id": id, "name": r.principal_name})),
                _ => r.v_text.map(|t| json!(t)),
            };
            CustomFieldValue {
                name: r.name,
                field_type: r.ftype,
                value,
            }
        })
        .collect())
}
