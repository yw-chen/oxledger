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
use crate::search::Wait;
use crate::search::{IxFilter, Layout, SearchIndex};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::filter::{Cmp, Filter, Test, Value as FValue};
use ox_core::{IssueFacts, Pred};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub struct App {
    pub pool: PgPool,
    pub index: SearchIndex,
    /// The wiki index (design 06 §3.14), followed like `index`.
    pub wiki: crate::wiki_search::WikiIndex,
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
    /// Free text: BM25F over the issue keys (×5), subject (×3), description
    /// (×1), visible text custom fields (×0.7) and public notes (×0.5);
    /// Chinese works without word segmentation. A word (or the whole query)
    /// equal to a concept label also searches the concept's other labels
    /// (`match.expanded`).
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
    /// Filter AST v2 (design 05 §3.2) over built-in fields (project, tracker,
    /// priority, status, status_category, assignee, author, fixed_version,
    /// is_private, start_date, due_date, created_at, updated_at,
    /// estimated_hours, overdue) and custom fields (`cf:<id>`), e.g.
    /// {"and": [{"field": "status_category", "op": "in", "value": ["todo", "doing"]},
    /// {"field": "assignee", "op": "eq", "value": {"me": true}},
    /// {"field": "due_date", "op": "within", "value": {"range": "next_n_days", "n": 7}}]}.
    /// ANDed with the other parameters. Errors: `invalid_filter` with
    /// `details.path` and `details.reason` (unknown_field, unknown_value,
    /// too_complex, unsupported_op, invalid_value, invalid_shape,
    /// project_scope_required, me_unavailable).
    #[serde(rename = "where")]
    #[schemars(with = "Option<filter::v2::WhereInput>")]
    pub where_: Option<serde_json::Value>,
    /// Order by up to 3 fields (then id): updated_at, created_at, due_date,
    /// start_date, priority, status, key, estimated_hours, cf:<id>. Default:
    /// updated_at desc without `q`, relevance with `q` (`rank` is still the
    /// relevance position when `sort` is given).
    pub sort: Option<Vec<filter::v2::SortInput>>,
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
    /// Of key, subject, description, cf_text (visible text custom fields),
    /// notes (public notes), in that order.
    pub fields: Vec<String>,
    /// primary: a full term matched (an expanded label's word counts);
    /// fallback: only single CJK characters or identifier parts.
    pub strength: Strength,
    /// Design 05 §3.9: concept labels added to the query that occur in
    /// this issue's text (a hidden label shows as the concept's name).
    /// Absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<crate::concepts::Expansion>,
}

/// A snippet of the current text and its citation (design 03 §3.6).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchEvidence {
    /// subject | description | note (a public note; `note_id`). Keys and
    /// custom field values match without a snippet.
    pub field: String,
    /// The note the snippet is from (`field = note`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<Uuid>,
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
    pub meta: SearchMeta,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchMeta {
    /// The date `overdue` and relative dates were computed with (R10, org
    /// time zone), YYYY-MM-DD.
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
    /// `markdown`, or `plain` for a description written before M2 and not
    /// rewritten since (design 06 R-M9). An editor that rewrites a plain
    /// description sends `format: "markdown"` to convert it.
    pub description_format: String,
    /// `GET /issues/{key}` (design 06 R-M10): the description as sanitized
    /// HTML, references resolved for the caller. The only description field
    /// to put in the page as HTML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_html: Option<String>,
    /// `GET /issues/{key}`: the description is an agent's (R-M3); the editor
    /// warns before a person rewrites it (design 06 §6.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_by_agent: Option<bool>,
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
    /// Decimal string such as "1.5" (design 05 §3.10.3); null when not set.
    pub estimated_hours: Option<String>,
    /// Contract 05 §3.5.4: hours of the time entries on this issue that the
    /// caller can see (others' hidden entries are not counted), such as "6.00";
    /// "0" when none. Absent when the caller cannot read time in the project
    /// (no `view_time_entries`, module off) and always for agents (§4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_hours: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Pass back as `expected_lock_version` when updating (M1).
    pub lock_version: i32,
    /// Statuses the caller may move this issue to (workflow), current first.
    pub allowed_statuses: Vec<String>,
    /// Custom fields that apply to this issue (tracker and project) and that the
    /// caller may read, in display order; `value` is null when empty.
    pub custom_fields: Vec<CustomFieldValue>,
    /// Contract 05 §3.4: the Triage state; null when the issue never
    /// entered the inbox.
    pub triage: Option<crate::triage::IssueTriage>,
    /// Contract 05 §3.6.3: rule ids of open `accept_and_mark` marks (the
    /// issue broke rules published after it was written); the next write
    /// that passes the rules clears them. Empty when none.
    pub form_rule_marks: Vec<String>,
    /// Contract 05 §3.11.1: whether the caller watches this issue. People
    /// only (agents never watch, §4.6); absent for agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<crate::notifications::WatchInfo>,
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
    /// A user's login (what `@login` mentions write, design 06 §6.2, gap
    /// G48); absent for groups and everything else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

/// A custom field as `resolve(kind: "custom_field")` describes it (design
/// 03 §2.4): what a filter on it needs.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CustomFieldInfo {
    pub id: i32,
    pub name: String,
    /// int | float | date | string | text | list | user | bool | link
    #[serde(rename = "type")]
    pub ftype: String,
    /// Unit of the stored number (e.g. "hour"), when the field has one: send values in it.
    pub unit: Option<String>,
    /// Filter operators the type takes.
    pub operators: Vec<&'static str>,
    /// List fields: the options (filter by label; inactive ones still match old values).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<FieldOptionInfo>,
    /// Pass it in filter conditions to fail with `field_definition_changed` instead of
    /// silently filtering by a changed definition.
    pub definition_version: i32,
}

#[derive(Debug, Serialize, JsonSchema, FromRow)]
pub struct FieldOptionInfo {
    pub id: Uuid,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ResolveResult {
    pub candidates: Vec<Candidate>,
    /// Only with `kind: "custom_field"`: the matching fields the caller may read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub custom_fields: Vec<CustomFieldInfo>,
    /// When nothing matched: valid values for the requested kind.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub valid_values: Vec<String>,
}

// ---------------------------------------------------------------- resolve

const KINDS: [&str; 5] = ["project", "tracker", "status", "principal", "custom_field"];

/// Name/alias lookup. `kind` = project | tracker | status | principal (user
/// or group) | custom_field (only with this kind).
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
    if kind == Some("custom_field") {
        return resolve_custom_field(conn, caller, &scope, text.trim()).await;
    }
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
            "SELECT DISTINCT kind, id, name, label AS matched, login FROM (
               SELECT 'project' AS kind, p.id, p.name, l.label, NULL::text FROM projects p
                 CROSS JOIN LATERAL (VALUES (p.key), (p.name)) AS l(label)
                 WHERE p.id = ANY($2)
               UNION ALL SELECT 'tracker', id, name, name, NULL FROM trackers
               UNION ALL SELECT 'status', id, name, name, NULL FROM issue_statuses
               UNION ALL SELECT 'principal', p.id, p.name, l.label,
                                CASE WHEN p.kind = 'user' THEN p.login END
                 FROM principals p
                 CROSS JOIN LATERAL (VALUES (p.name), (p.login)) AS l(label)
                 WHERE p.status = 'active' AND p.kind IN ('user', 'group') AND l.label IS NOT NULL
               UNION ALL SELECT a.entity_kind, a.entity_id, COALESCE(pr.name, t.name, s.name, pp.name), a.label,
                                CASE WHEN pp.kind = 'user' THEN pp.login END
                 FROM aliases a
                 LEFT JOIN projects pr ON a.entity_kind = 'project' AND pr.id = a.entity_id AND pr.id = ANY($2)
                 LEFT JOIN trackers t ON a.entity_kind = 'tracker' AND t.id = a.entity_id
                 LEFT JOIN issue_statuses s ON a.entity_kind = 'status' AND s.id = a.entity_id
                 LEFT JOIN principals pp ON a.entity_kind = 'principal' AND pp.id = a.entity_id AND pp.kind IN ('user', 'group')
                 WHERE COALESCE(pr.name, t.name, s.name, pp.name) IS NOT NULL
             ) AS x(kind, id, name, label, login)
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
                custom_fields: Vec::new(),
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
        custom_fields: Vec::new(),
        valid_values,
    })
}

/// `resolve(kind: "custom_field")` (design 03 §7 step 11): exact name
/// (case-insensitive) first, else substring; only fields the caller may
/// read somewhere. Nothing → the readable field names as `valid_values`.
async fn resolve_custom_field(
    conn: &mut PgConnection,
    caller: AuthContext,
    scope: &ox_core::Scope,
    text: &str,
) -> Result<ResolveResult> {
    let fields = filter::readable_fields(&mut *conn, caller, scope).await?;
    let t = text.to_lowercase();
    let mut hits: Vec<&crate::custom_fields::FieldRow> = fields
        .iter()
        .map(|(f, _)| f)
        .filter(|f| f.name.to_lowercase() == t)
        .collect();
    if hits.is_empty() && !t.is_empty() {
        hits = fields
            .iter()
            .map(|(f, _)| f)
            .filter(|f| f.name.to_lowercase().contains(&t))
            .take(20)
            .collect();
    }
    let mut out = Vec::with_capacity(hits.len());
    for f in hits {
        let ty = f.field_type()?;
        let options: Vec<FieldOptionInfo> = if f.ftype == "list" {
            sqlx::query_as(
                "SELECT id, label, active FROM field_options WHERE field_id = $1 ORDER BY position, label",
            )
            .bind(f.id)
            .fetch_all(&mut *conn)
            .await?
        } else {
            Vec::new()
        };
        out.push(CustomFieldInfo {
            id: f.id,
            name: f.name.clone(),
            ftype: f.ftype.clone(),
            unit: f.unit.clone(),
            operators: filter::operators(ty),
            options,
            definition_version: f.definition_version,
        });
    }
    let valid_values = if out.is_empty() {
        fields.iter().map(|(f, _)| f.name.clone()).collect()
    } else {
        Vec::new()
    };
    Ok(ResolveResult {
        candidates: Vec::new(),
        custom_fields: out,
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
            "SELECT DISTINCT ON (p.name, p.id) 'principal' AS kind, p.id, p.name, l.label AS matched,
                    CASE WHEN p.kind = 'user' THEN p.login END AS login
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
        custom_fields: Vec::new(),
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

/// Where a candidate's text comes from (design 05 §3.9 index fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    Key,
    Subject,
    Description,
    Cf,
    Note(Uuid),
}

impl Src {
    /// The `match.fields` name.
    fn match_name(self) -> &'static str {
        match self {
            Src::Key => "key",
            Src::Subject => "subject",
            Src::Description => "description",
            Src::Cf => "cf_text",
            Src::Note(_) => "notes",
        }
    }

    /// The citation source unit (03 §3.2); keys and custom field values
    /// have none yet, so they never give a snippet.
    fn source(self, issue_id: Uuid) -> Option<(Source, &'static str, Option<Uuid>)> {
        match self {
            Src::Subject => Some((
                Source::IssueField {
                    issue_id,
                    field: TextField::Subject,
                },
                "subject",
                None,
            )),
            Src::Description => Some((
                Source::IssueField {
                    issue_id,
                    field: TextField::Description,
                },
                "description",
                None,
            )),
            Src::Note(note_id) => Some((
                Source::IssueNote { issue_id, note_id },
                "note",
                Some(note_id),
            )),
            Src::Key | Src::Cf => None,
        }
    }
}

/// `match.fields` in the order of the §3.9 table.
const MATCH_ORDER: [&str; 5] = ["key", "subject", "description", "cf_text", "notes"];

/// One text of a candidate: where from, the raw text, its code points and
/// what the query matches in it.
type FieldText = (Src, String, CpText, Option<FieldMatch>);

/// The current text of one candidate.
struct Texts {
    /// The stored texts (evidence and citations quote them).
    fields: Vec<FieldText>,
    /// The index document's `search_rev` equals the current text's (03 §3.4
    /// step 4): the index matched exactly this text.
    current: bool,
    /// The query matches the current text as the layout indexes it (the
    /// plain text of Markdown in a Current generation, design 06 R-M11).
    rematched: bool,
}

impl Texts {
    /// Still a hit: the index saw this very text (`search_rev` equal, the
    /// re-match can be skipped), or the current text matches again.
    fn survives(&self) -> bool {
        self.current || self.rematched
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
    let waited = consistency_wait(app, caller, &p).await;
    {
        use crate::metrics::Outcome;
        let ms = std::time::Duration::from_millis;
        match &waited {
            Ok(Some(Wait::Satisfied { waited_ms })) => app
                .index
                .metrics
                .consistency(Outcome::Satisfied, Some(ms(*waited_ms))),
            Ok(Some(Wait::Lagging { waited_ms })) => app
                .index
                .metrics
                .consistency(Outcome::Lagging, Some(ms(*waited_ms))),
            Err(e)
                if e.downcast_ref::<ApiError>()
                    .is_some_and(|a| a.code == ErrorCode::ConsistencyEpochChanged) =>
            {
                app.index.metrics.consistency(Outcome::EpochChanged, None)
            }
            _ => {}
        }
    }
    let waited = waited?;
    if let Some(Wait::Lagging { waited_ms }) = waited {
        // An invalid request is an input error, not "wait and retry": run it
        // once (results dropped) so its errors come back first.
        let mut tx = read_tx(app).await?;
        let today = filter::v2::clock(&mut tx).await?.today;
        // An index that cannot serve yet is not an input error: still lagging.
        if let Err(e) = search_in(&mut tx, app, caller, p.clone()).await
            && e.downcast_ref::<ApiError>()
                .is_none_or(|a| a.code != ErrorCode::SearchUnavailable)
        {
            return Err(e);
        }
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
            meta: SearchMeta {
                today: today.to_string(),
            },
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

pub(crate) const LAG_RETRY_MS: u64 = 500;

/// The SQL work budget of one request (03 §5.4 item 4, Q-S7 default 2 s).
pub const QUERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// A timed-out S2 switches to S0 only with at least this much left.
const SWITCH_MIN: std::time::Duration = std::time::Duration::from_millis(300);

/// Tests: override the budget (ms; 0 = default), slow S2 down, and see the
/// shape the last filter search used `(shape, switched)`.
#[cfg(feature = "test-support")]
pub static BUDGET_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "test-support")]
pub static S2_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "test-support")]
pub static LAST_SHAPE: std::sync::Mutex<Option<(filter::Shape, bool)>> =
    std::sync::Mutex::new(None);

struct Budget {
    end: std::time::Instant,
    total: std::time::Duration,
}

impl Budget {
    fn start() -> Budget {
        #[cfg(feature = "test-support")]
        let total = match BUDGET_MS.load(std::sync::atomic::Ordering::SeqCst) {
            0 => QUERY_BUDGET,
            ms => std::time::Duration::from_millis(ms),
        };
        #[cfg(not(feature = "test-support"))]
        let total = QUERY_BUDGET;
        Budget {
            end: std::time::Instant::now() + total,
            total,
        }
    }

    fn remaining(&self) -> std::time::Duration {
        self.end
            .saturating_duration_since(std::time::Instant::now())
    }

    /// Runs one statement under the remaining budget inside a savepoint
    /// (the read transaction stays usable after a cancel). `None`: the
    /// budget ran out (57014), nothing left running.
    async fn run(
        &self,
        conn: &mut PgConnection,
        shape: filter::Shape,
        mut qb: QueryBuilder<Postgres>,
        cap: Option<std::time::Duration>,
    ) -> Result<Option<Vec<PgRow>>> {
        let mut left = self.remaining();
        if let Some(c) = cap {
            left = left.min(c);
        }
        let left = left.as_millis() as u64;
        if left == 0 {
            return Ok(None);
        }
        // The caller's own timeout comes back afterwards, not the session
        // default (ROLLBACK TO restores it by itself; RELEASE keeps ours).
        let before: String = sqlx::query_scalar("SELECT current_setting('statement_timeout')")
            .fetch_one(&mut *conn)
            .await?;
        sqlx::query("SAVEPOINT ox_budget")
            .execute(&mut *conn)
            .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = {left}"
        )))
        .execute(&mut *conn)
        .await?;
        let r = async {
            #[cfg(feature = "test-support")]
            {
                let d = S2_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst);
                if d > 0 && shape == filter::Shape::S2 {
                    sqlx::query("SELECT pg_sleep($1)")
                        .bind(d as f64 / 1000.0)
                        .execute(&mut *conn)
                        .await?;
                }
            }
            let _ = shape;
            qb.build().fetch_all(&mut *conn).await
        }
        .await;
        match r {
            Ok(rows) => {
                sqlx::query("RELEASE SAVEPOINT ox_budget")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SELECT set_config('statement_timeout', $1, true)")
                    .bind(&before)
                    .execute(&mut *conn)
                    .await?;
                Ok(Some(rows))
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
                sqlx::query("ROLLBACK TO SAVEPOINT ox_budget")
                    .execute(&mut *conn)
                    .await?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn query_timeout(b: &Budget) -> anyhow::Error {
    ApiError::new(
        ErrorCode::QueryTimeout,
        "the field conditions are too broad to finish in time; narrow them",
    )
    .with_details(serde_json::json!({
        "deadline_ms": b.total.as_millis() as u64,
        "suggestions": ["add_project_filter", "narrow_date_range", "add_text_query"],
    }))
    .into()
}

/// 03 §5.4 item 2: probe each condition up to T + 1 index entries; any
/// condition at or below T (an exact count) → S0, all above → S2. A probe
/// that runs out of budget keeps S0 (the main query then decides).
async fn choose_shape(
    conn: &mut PgConnection,
    conds: &[(ox_core::filter::Field, ox_core::filter::Test)],
    budget: &Budget,
) -> Result<filter::Shape> {
    for (field, test) in conds {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("");
        filter::push_probe(&mut qb, *field, test);
        let Some(rows) = budget.run(&mut *conn, filter::Shape::S0, qb, None).await? else {
            return Ok(filter::Shape::S0);
        };
        let n: i64 = rows.first().map(|r| r.try_get(0)).transpose()?.unwrap_or(0);
        if n < filter::PROBE_CAP {
            return Ok(filter::Shape::S0);
        }
    }
    Ok(filter::Shape::S2)
}

/// No generation can serve yet, or the sync loop stopped (03 §4.11).
pub(crate) fn search_unavailable() -> anyhow::Error {
    ApiError::new(
        ErrorCode::SearchUnavailable,
        "the search index is not available right now; try again later",
    )
    .with_retry_after(5000)
    .into()
}
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
    let Some((x, wait)) = consistency_target(
        &app.pool,
        p.wait_ms,
        p.on_lag.as_deref(),
        p.consistent_after.as_deref(),
    )
    .await?
    else {
        return Ok(None);
    };
    if !p.q.as_deref().is_some_and(|q| !q.trim().is_empty()) {
        // SQL reads its own commits (03 §4.4): not_applicable, no wait.
        return Ok(None);
    }
    Ok(Some(
        app.index.wait_for(caller.principal_id(), x, wait).await,
    ))
}

/// Checks `wait_ms`, `on_lag` and a `consistent_after` token (03 §4.11)
/// for any index that follows the outbox (issues, the wiki): the token's
/// xid and how long to wait for it, `None` without a token.
pub(crate) async fn consistency_target(
    pool: &PgPool,
    wait_ms: Option<u32>,
    on_lag: Option<&str>,
    token: Option<&str>,
) -> Result<Option<(u64, std::time::Duration)>> {
    let wait_ms = wait_ms.unwrap_or(DEFAULT_WAIT_MS);
    if wait_ms > MAX_WAIT_MS {
        return Err(user_err(format!(
            "wait_ms {wait_ms}: use 0..={MAX_WAIT_MS}"
        )));
    }
    match on_lag {
        None | Some("return") | Some("error") => {}
        Some(other) => return Err(user_err(format!("on_lag {other:?}: use return or error"))),
    }
    let Some(t) = token else {
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
        .fetch_one(pool)
        .await?;
    if epoch != current {
        return Err(api(
            ErrorCode::ConsistencyEpochChanged,
            "the search data was reset since this token; search again without consistent_after \
             (the write itself is not affected)",
        ));
    }
    Ok(Some((x, std::time::Duration::from_millis(wait_ms as u64))))
}

/// What a saved view adds to a search (design 05 §3.3): conditions already
/// resolved, a sort replacing `p.sort`, and what the cursor is bound to.
#[derive(Default)]
pub(crate) struct Extra {
    /// The view's own conditions (its `where`, `override.where_and`): ANDed
    /// with the request's, and looked at by probing (03 §5.4) like `where`.
    pub chosen: Vec<Filter>,
    /// Scope conditions (project, lane, swimlane, done window): ANDed, not probed.
    pub fixed: Vec<Filter>,
    /// Replaces `p.sort`: the ORDER BY columns and their normalized form.
    pub sort: Option<(Vec<filter::v2::SortCol>, serde_json::Value)>,
    /// Bound into the cursor's query hash (`None`: nothing added).
    pub hash: Option<serde_json::Value>,
}

async fn search_in(
    conn: &mut PgConnection,
    app: &App,
    caller: AuthContext,
    p: SearchParams,
) -> Result<SearchResult> {
    search_with(conn, app, caller, p, Extra::default()).await
}

/// [`search_in`] with a view's additions, in the caller's transaction.
pub(crate) async fn search_with(
    conn: &mut PgConnection,
    app: &App,
    caller: AuthContext,
    p: SearchParams,
    extra: Extra,
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
    // The cursor's owner and integrity first (another person's cursor is
    // `invalid_cursor` before any parameter is looked at); its query below.
    let opened = match p.cursor.as_deref() {
        None => None,
        Some(c) => Some(cursor::unseal_position(&app.pool, c, caller.principal_id()).await?),
    };
    // design 05 §3.2: the legacy parameters become v2 conditions, ANDed with
    // the v1 `filter` and `where`, compiled by the one filter compiler.
    let clock = filter::v2::clock(&mut *conn).await?;
    let mut legacy: Vec<Filter> = Vec::new();
    let mut project_id = None;
    if let Some(v) = p.project.as_deref() {
        let id = resolve_one(&mut *conn, caller, "project", v).await?;
        project_id = Some(id);
        legacy.push(filter::builtin_cond(
            filter::PROJECT_FIELD,
            Test::Cmp(Cmp::Eq, FValue::Ref(id)),
        )?);
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
        legacy.push(filter::builtin_cond(
            filter::VERSION_FIELD,
            Test::Cmp(Cmp::Eq, FValue::Ref(id)),
        )?);
    }
    // §17.15 overdue: computed per query, never indexed. `false` filters nothing.
    if p.overdue == Some(true) {
        legacy.push(filter::v2::overdue(clock.today, true)?);
    }
    if let Some(v) = p.tracker.as_deref() {
        let id = resolve_one(&mut *conn, caller, "tracker", v).await?;
        legacy.push(filter::builtin_cond(
            filter::TRACKER_FIELD,
            Test::Cmp(Cmp::Eq, FValue::Ref(id)),
        )?);
    }
    if let Some(v) = p.status.as_deref() {
        let id = resolve_one(&mut *conn, caller, "status", v).await?;
        legacy.push(filter::builtin_cond(
            filter::STATUS_FIELD,
            Test::Cmp(Cmp::Eq, FValue::Ref(id)),
        )?);
    }
    let categories: Option<Vec<&str>> = match p.status_category.as_deref() {
        None => None,
        Some("open") => Some(vec!["todo", "doing"]),
        Some(c @ ("todo" | "doing" | "done")) => Some(vec![c]),
        Some(other) => {
            return Err(user_err(format!(
                "status_category {other:?}: use open, todo, doing or done"
            )));
        }
    };
    if let Some(c) = categories {
        let test = match c.as_slice() {
            [one] => Test::Cmp(Cmp::Eq, FValue::Text(one.to_string())),
            _ => Test::In(c.iter().map(|x| FValue::Text(x.to_string())).collect()),
        };
        legacy.push(filter::builtin_cond(filter::CATEGORY_FIELD, test)?);
    }
    match p.assignee.as_deref() {
        None => {}
        Some("none") => legacy.push(filter::builtin_cond(filter::ASSIGNEE_FIELD, Test::IsNull)?),
        // `me` as in v2 (Redmine): me and my groups.
        Some("me") => match caller.user_id() {
            Some(_) => {
                let mine: Vec<FValue> = scope
                    .principal_ids
                    .iter()
                    .copied()
                    .map(FValue::Ref)
                    .collect();
                legacy.push(filter::builtin_cond(
                    filter::ASSIGNEE_FIELD,
                    match mine.as_slice() {
                        [one] => Test::Cmp(Cmp::Eq, one.clone()),
                        _ => Test::In(mine),
                    },
                )?);
            }
            None => return Err(user_err("assignee=me needs an authenticated caller")),
        },
        Some(v) => {
            let id = resolve_one(&mut *conn, caller, "principal", v).await?;
            legacy.push(filter::builtin_cond(
                filter::ASSIGNEE_FIELD,
                Test::Cmp(Cmp::Eq, FValue::Ref(id)),
            )?);
        }
    }

    let custom = match &p.filter {
        None => None,
        Some(v) => {
            Some(filter::resolve(&mut *conn, caller, &scope, &filter::parse_input(v)?).await?)
        }
    };
    let wherev2 = match &p.where_ {
        None => None,
        Some(v) => {
            Some(filter::v2::resolve(&mut *conn, caller, &scope, clock, v, project_id).await?)
        }
    };
    let sort = match (extra.sort, &p.sort) {
        (Some(s), _) => Some(s),
        (None, None) => None,
        (None, Some(keys)) => {
            let fields = if keys.iter().any(|k| k.field.trim().starts_with("cf:")) {
                filter::readable_fields(&mut *conn, caller, &scope).await?
            } else {
                Vec::new()
            };
            Some(filter::v2::resolve_sort(&fields, keys)?)
        }
    };
    let sort_cols: &[filter::v2::SortCol] = sort.as_ref().map(|s| s.0.as_slice()).unwrap_or(&[]);

    // The cursor is bound to who asked, what was asked and the order (§4.5).
    let order = match (text.is_some(), sort.is_some()) {
        (_, true) => "sort",
        (true, false) => "rank",
        (false, false) => "updated_desc",
    };
    let mut hashed = serde_json::json!({
        "q": text, "project": &p.project, "tracker": &p.tracker, "status": &p.status,
        "status_category": &p.status_category, "assignee": &p.assignee, "filter": &p.filter,
        "version": &p.version, "overdue": &p.overdue,
        "where": wherev2.as_ref().map(|w| &w.normalized),
        "sort": sort.as_ref().map(|s| &s.1),
        "order": order,
    });
    if let Some(h) = extra.hash {
        hashed["extra"] = h;
    }
    let qhash = cursor::query_hash(&hashed);
    let pos = match opened {
        None => None,
        Some((h, _)) if h != qhash => return Err(cursor::mismatch()),
        Some((_, pos)) => Some(pos),
    };
    let mut after: Option<(DateTime<Utc>, Uuid)> = None;
    let mut after_sort: Option<(Vec<Option<String>>, Uuid)> = None;
    let mut offset: i64 = 0;
    match (pos, text.is_some(), sort.is_some()) {
        (None, _, _) => {}
        (Some(Position::Keyset { t, i }), false, false) => after = Some((t, i)),
        (Some(Position::Sort { k, i }), false, true) if k.len() == sort_cols.len() => {
            after_sort = Some((k, i))
        }
        (Some(Position::Rank { o }), true, _) => offset = o as i64,
        _ => {
            return Err(api(
                ErrorCode::InvalidCursor,
                "invalid cursor; search again without it",
            ));
        }
    }

    // The user's filters (probing, 03 §5.4, looks at these only).
    let mut chosen: Vec<Filter> = Vec::new();
    chosen.extend(custom);
    chosen.extend(wherev2.map(|w| w.filter));
    chosen.extend(extra.chosen);
    let probe = match chosen.as_slice() {
        [] => None,
        [one] => filter::probe_conditions(one),
        many => filter::probe_conditions(&Filter::And(
            many.iter()
                .flat_map(|f| match f {
                    Filter::And(ps) => ps.clone(),
                    other => vec![other.clone()],
                })
                .collect(),
        )),
    };
    let mut parts = legacy;
    parts.extend(extra.fixed);
    parts.extend(chosen);
    let combined = match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Filter::And(parts)),
    };
    // §17.15 fast fields and the visibility-style conditions narrow the
    // index candidates; SQL still decides.
    let ix = combined
        .as_ref()
        .map(filter::index_prefilter)
        .unwrap_or(IxFilter::All);
    let mut all = vec![visible];
    if let Some(f) = &combined {
        all.push(filter::pred_prefilter(f));
    }
    let pred = Pred::And(all).simplify();

    let push_where_shaped = |qb: &mut QueryBuilder<Postgres>, shape: filter::Shape| {
        push_pred(qb, "i", &pred);
        if let Some(f) = &combined {
            qb.push(" AND ");
            filter::push_filter_shaped(qb, "i", f, shape);
        }
    };
    let push_where = |qb: &mut QueryBuilder<Postgres>| push_where_shaped(qb, filter::Shape::S0);
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
    let select_qb = || {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(select.as_str());
        for (n, c) in sort_cols.iter().enumerate() {
            qb.push(", (");
            c.push(&mut qb);
            qb.push(format!(")::text AS sk{n}"));
        }
        qb.push(SEARCH_FROM);
        qb
    };

    let mut truncated = false;
    let mut checked: HashMap<Uuid, Texts> = HashMap::new();
    // Design 05 §3.9: the concept labels added to `q` (current layout only).
    let mut expansions: Vec<crate::concepts::Expansion> = Vec::new();
    // With `q` and `sort`: each item's relevance rank among all matches.
    let mut rank_of: HashMap<Uuid, u32> = HashMap::new();
    let (rows, more): (Vec<PgRow>, bool) = match (text, &terms) {
        (Some(q), Some(_)) => {
            // Free text: rank in the index (already filtered by visibility +
            // the index-side filters), re-check every candidate in SQL — the
            // index may lag the database, and some filters exist only in SQL —
            // then compare the current text in batches (design 03 §3.4): a
            // candidate whose text no longer matches is dropped whatever
            // `explain` says. Grow the candidate list until this page and one
            // lookahead item survive (with `sort`: until every match is
            // known, since any of them may sort first). Best effort: each
            // round asks the index again, so a concurrent index update can
            // shift the ranking.
            let need = if sort.is_some() {
                usize::MAX
            } else {
                (offset + limit + 1) as usize
            };
            let batch = 2 * limit as usize;
            let mut k = SEARCH_CANDIDATES;
            // One view for the whole search (03 §4.4): the one a satisfied
            // token was checked against, or newer; never switched half-way.
            let view = match app.index.view() {
                Some(v) if !app.index.metrics.sync_stalled() => v,
                _ => return Err(search_unavailable()),
            };
            // A legacy generation (before M1.7, still serving until its
            // rebuild switches) is queried and re-checked the old way:
            // subject and description only, no expansion (03 §4.8).
            let layout = view.layout();
            let (eterms, keys) = match layout {
                Layout::RawText | Layout::Current => {
                    expansions = crate::concepts::expansions(&mut *conn, q).await?;
                    let labels: Vec<String> = expansions.iter().map(|e| e.added.clone()).collect();
                    (
                        Terms::with_expansions(q, &labels, crate::search::EXPANSION_WEIGHT),
                        crate::search::key_terms(q),
                    )
                }
                Layout::Legacy => (Terms::new(q), Vec::new()),
            };
            let terms = &eterms;
            let weighted = terms.weighted();
            let survivors = loop {
                let hits = view
                    .search_weighted(&weighted, &keys, &pred, &ix, k)
                    .map_err(|e| {
                        tracing::error!("search index failed: {e:#}");
                        search_unavailable()
                    })?;
                let exhausted = hits.len() < k;
                let score_of: HashMap<Uuid, f32> =
                    hits.iter().map(|h| (h.issue_id, h.score)).collect();
                let rev_of: HashMap<Uuid, Option<[u8; 32]>> =
                    hits.iter().map(|h| (h.issue_id, h.search_rev)).collect();
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
                        // A Current generation indexes the plain text of
                        // Markdown (design 06 R-M11): the rev and the
                        // re-match use that text; evidence and citations
                        // quote the stored text as it is (03 §3.5).
                        let texts: Vec<(Uuid, String, String, String)> = sqlx::query_as(
                            "SELECT id, subject, description, description_format
                             FROM issues WHERE id = ANY($1)",
                        )
                        .bind(&todo)
                        .fetch_all(&mut *conn)
                        .await?;
                        // The other indexed sources as they are now, with
                        // the index's own rules (public notes, visible
                        // text fields that apply): a retracted or redacted
                        // note, a hidden field drop out here even before
                        // the index sees the change (T-M1-62, 63).
                        let mut extras = if layout.extended() {
                            crate::search::load_extras(&mut *conn, &todo, Layout::RawText).await?
                        } else {
                            HashMap::new()
                        };
                        let mut indexed = if layout.plain_text() {
                            crate::search::load_extras(&mut *conn, &todo, layout).await?
                        } else {
                            HashMap::new()
                        };
                        for (id, subject, description, format) in texts {
                            let x = extras.remove(&id).unwrap_or_default();
                            let (fields, current, rematched) = if layout.plain_text() {
                                let plain = layout.text(description.clone(), &format);
                                let px = indexed.remove(&id).unwrap_or_default();
                                let current = rev_of.get(&id).copied().flatten()
                                    == Some(crate::search::doc_rev(layout, &subject, &plain, &px));
                                let rematched =
                                    candidate_texts(terms, &keys, subject.clone(), plain, px)
                                        .iter()
                                        .any(|f| f.3.is_some());
                                (
                                    candidate_texts(terms, &keys, subject, description, x),
                                    current,
                                    rematched,
                                )
                            } else {
                                let current = rev_of.get(&id).copied().flatten()
                                    == Some(crate::search::doc_rev(
                                        layout,
                                        &subject,
                                        &description,
                                        &x,
                                    ));
                                let fields = candidate_texts(terms, &keys, subject, description, x);
                                let rematched = fields.iter().any(|f| f.3.is_some());
                                (fields, current, rematched)
                            };
                            checked.insert(
                                id,
                                Texts {
                                    fields,
                                    current,
                                    rematched,
                                },
                            );
                        }
                    }
                    survivors.extend(
                        ranked[i..end]
                            .iter()
                            .filter(|r| checked.get(&r.0).is_some_and(Texts::survives))
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
            if sort.is_some() {
                // Every match is known: order them by the sort keys in SQL
                // (rank stays the relevance position).
                for (n, id) in survivors.iter().enumerate() {
                    rank_of.insert(*id, (n + 1) as u32);
                }
                let mut qb = select_qb();
                push_where(&mut qb);
                qb.push(" AND i.id = ANY(").push_bind(survivors).push(")");
                push_order(&mut qb, sort_cols);
                qb.push(" OFFSET ")
                    .push_bind(offset)
                    .push(" LIMIT ")
                    .push_bind(limit + 1);
                let mut rows = qb.build().fetch_all(&mut *conn).await?;
                let more = rows.len() as i64 > limit;
                rows.truncate(limit as usize);
                (rows, more)
            } else {
                let more = survivors.len() as i64 > offset + limit;
                let page: Vec<Uuid> = survivors
                    .into_iter()
                    .skip(offset as usize)
                    .take(limit as usize)
                    .collect();
                let mut qb = select_qb();
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
        }
        _ => {
            // 03 §5.4: one work budget for probes, the first shape and at
            // most one switch; out of budget → query_timeout, no partial page.
            let deadline = Budget::start();
            let mut shape = match probe {
                Some(conds) => choose_shape(&mut *conn, &conds, &deadline).await?,
                None => filter::Shape::S0,
            };
            let mut switched = false;
            let mut rows = loop {
                let mut qb = select_qb();
                push_where_shaped(&mut qb, shape);
                if sort.is_some() {
                    if let Some((k, id)) = &after_sort {
                        push_after(&mut qb, sort_cols, k, *id);
                    }
                    push_order(&mut qb, sort_cols);
                    qb.push(" LIMIT ").push_bind(limit + 1);
                } else {
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
                }
                // A first S2 gets 60% of what is left, so a switch to S0
                // still has budget (03 §5.4: no fresh budget after it).
                let cap = (shape == filter::Shape::S2 && !switched)
                    .then(|| deadline.remaining().mul_f32(0.6));
                match deadline.run(&mut *conn, shape, qb, cap).await? {
                    Some(rows) => break rows,
                    // S2 ran out: switch once to S0 if enough budget is left.
                    None if shape == filter::Shape::S2
                        && !switched
                        && deadline.remaining() >= SWITCH_MIN =>
                    {
                        shape = filter::Shape::S0;
                        switched = true;
                    }
                    None => return Err(query_timeout(&deadline)),
                }
            };
            #[cfg(feature = "test-support")]
            if let Ok(mut l) = LAST_SHAPE.lock() {
                *l = Some((shape, switched));
            }
            let more = rows.len() as i64 > limit;
            rows.truncate(limit as usize);
            (rows, more)
        }
    };

    // Per expansion, its label's full terms that the query itself lacks:
    // an item shows the expansion when one of them matched in its text.
    let expanded_terms: Vec<(crate::concepts::Expansion, Vec<String>)> = match text {
        Some(q) if !expansions.is_empty() => {
            let own: HashSet<String> = crate::search::query_terms(q)
                .into_iter()
                .map(|(t, _)| t)
                .collect();
            expansions
                .iter()
                .map(|e| {
                    let ts = crate::search::query_terms(&e.added)
                        .into_iter()
                        .filter(|(t, w)| *w >= 1.0 && !own.contains(t))
                        .map(|(t, _)| t)
                        .collect();
                    (e.clone(), ts)
                })
                .collect()
        }
        _ => Vec::new(),
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
            it.rank = Some(
                rank_of
                    .get(&it.id)
                    .copied()
                    .unwrap_or((offset as usize + n + 1) as u32),
            );
            if let Some(t) = checked.get(&it.id) {
                let matched: Vec<&FieldText> = t.fields.iter().filter(|f| f.3.is_some()).collect();
                let seen: HashSet<&str> = matched
                    .iter()
                    .filter_map(|f| f.3.as_ref())
                    .flat_map(|m| m.terms())
                    .collect();
                it.matched = Some(MatchInfo {
                    fields: MATCH_ORDER
                        .iter()
                        .filter(|n| matched.iter().any(|f| f.0.match_name() == **n))
                        .map(|n| n.to_string())
                        .collect(),
                    strength: matched
                        .iter()
                        .filter_map(|f| f.3.as_ref().map(|m| m.strength))
                        .max()
                        .unwrap_or(Strength::Fallback),
                    expanded: expanded_terms
                        .iter()
                        .filter(|(_, ts)| ts.iter().any(|t| seen.contains(t.as_str())))
                        .map(|(e, _)| e.clone())
                        // Hidden labels show as the concept's name: one
                        // entry per distinct (from, via, label).
                        .fold(Vec::new(), |mut acc: Vec<crate::concepts::Expansion>, e| {
                            if !acc
                                .iter()
                                .any(|x| x.from == e.from && x.via == e.via && x.label == e.label)
                            {
                                acc.push(e);
                            }
                            acc
                        }),
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
        (true, None, Some(last)) if sort.is_some() => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                Position::Sort {
                    k: (0..sort_cols.len())
                        .map(|n| last.try_get::<Option<String>, _>(format!("sk{n}").as_str()))
                        .collect::<Result<Vec<_>, _>>()?,
                    i: last.try_get("id")?,
                },
            )
            .await?,
        ),
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
        meta: SearchMeta {
            today: clock.today.to_string(),
        },
    })
}

/// ORDER BY the sort columns (missing values last in both directions,
/// design 05 §3.2), then `i.id` in the last key's direction (keyset-stable).
fn push_order(qb: &mut QueryBuilder<Postgres>, cols: &[filter::v2::SortCol]) {
    qb.push(" ORDER BY ");
    for c in cols {
        if c.nullable {
            qb.push("(");
            c.push(qb);
            qb.push(") IS NULL, ");
        }
        c.push(qb);
        qb.push(if c.desc { " DESC, " } else { " ASC, " });
    }
    let desc = cols.last().is_some_and(|c| c.desc);
    qb.push(if desc { "i.id DESC" } else { "i.id ASC" });
}

/// ` AND (row strictly after the cursor row)` in [`push_order`]'s order:
/// a lexicographic OR-chain over (missing flag, value) per column and `id`.
fn push_after(
    qb: &mut QueryBuilder<Postgres>,
    cols: &[filter::v2::SortCol],
    last: &[Option<String>],
    id: Uuid,
) {
    enum Comp<'a> {
        /// `(expr) IS NULL`, ascending (missing last).
        Flag(&'a filter::v2::SortCol, bool),
        Val(&'a filter::v2::SortCol, Option<&'a str>),
        Id(bool),
    }
    let mut comps: Vec<Comp> = Vec::new();
    for (c, v) in cols.iter().zip(last) {
        if c.nullable {
            comps.push(Comp::Flag(c, v.is_none()));
        }
        comps.push(Comp::Val(c, v.as_deref()));
    }
    comps.push(Comp::Id(cols.last().is_some_and(|c| c.desc)));
    let push_val = |qb: &mut QueryBuilder<Postgres>, c: &filter::v2::SortCol, v: &str| {
        qb.push_bind(v.to_string()).push(format!("::{}", c.cast));
    };
    let eq = |qb: &mut QueryBuilder<Postgres>, comp: &Comp| match comp {
        Comp::Flag(c, null) => {
            qb.push("((");
            c.push(qb);
            qb.push(") IS NULL) = ").push_bind(*null);
        }
        Comp::Val(c, None) => {
            qb.push("(");
            c.push(qb);
            qb.push(") IS NULL");
        }
        Comp::Val(c, Some(v)) => {
            qb.push("(");
            c.push(qb);
            qb.push(") = ");
            push_val(qb, c, v);
        }
        Comp::Id(_) => unreachable!("id is last"),
    };
    let gt = |qb: &mut QueryBuilder<Postgres>, comp: &Comp| match comp {
        Comp::Flag(c, null) => {
            qb.push("((");
            c.push(qb);
            qb.push(") IS NULL) > ").push_bind(*null);
        }
        Comp::Val(_, None) => {
            qb.push("FALSE");
        }
        Comp::Val(c, Some(v)) => {
            qb.push("(");
            c.push(qb);
            qb.push(if c.desc { ") < " } else { ") > " });
            push_val(qb, c, v);
        }
        Comp::Id(desc) => {
            qb.push(if *desc { "i.id < " } else { "i.id > " })
                .push_bind(id);
        }
    };
    qb.push(" AND (");
    for j in 0..comps.len() {
        if j > 0 {
            qb.push(" OR ");
        }
        qb.push("(");
        for c in &comps[..j] {
            eq(qb, c);
            qb.push(" AND ");
        }
        gt(qb, &comps[j]);
        qb.push(")");
    }
    qb.push(")");
}

/// A candidate's texts for the re-check (§3.4) in snippet order: subject,
/// description, then the public notes, keys and custom field values that
/// match (sources that do not match are not kept).
fn candidate_texts(
    terms: &Terms,
    keys: &[String],
    subject: String,
    description: String,
    x: crate::search::Extras,
) -> Vec<FieldText> {
    let text = |src: Src, t: String| {
        let cp = CpText::new(&t);
        let m = evidence::match_field(terms, &t, &cp);
        (src, t, cp, m)
    };
    let mut out = vec![
        text(Src::Subject, subject),
        text(Src::Description, description),
    ];
    for (id, body) in x.notes {
        let f = text(Src::Note(id), body);
        if f.3.is_some() {
            out.push(f);
        }
    }
    for k in x.keys {
        let lower = k.to_lowercase();
        if keys.contains(&lower) {
            let cp = CpText::new(&k);
            let m = FieldMatch::whole(&lower, cp.len());
            out.push((Src::Key, k, cp, Some(m)));
        }
    }
    for (_, v) in x.cf {
        let f = text(Src::Cf, v);
        if f.3.is_some() {
            out.push(f);
        }
    }
    out
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
        for (fi, (src, _, cp, m)) in t.fields.iter().enumerate() {
            let Some(m) = m else { continue };
            if src.source(it.id).is_none() {
                continue;
            }
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
                let (src, raw, cp, m) = &t.fields[*fi];
                let m = m.as_ref()?;
                let (source, field, note_id) = src.source(issue_id)?;
                Some(SearchEvidence {
                    field: field.into(),
                    note_id,
                    // 03 §3.4 step 4: the index ranked this very text, or
                    // an older one (the snippet is from the current text).
                    evidence_status: if t.current { "current" } else { "rechecked" }.into(),
                    text: cp.slice(w.start, w.end),
                    highlights: evidence::highlights_in(m, (w.start, w.end))
                        .into_iter()
                        .map(|(s, e)| Highlight {
                            start: s,
                            end: e,
                            strength: m.strength,
                        })
                        .collect(),
                    citation_id: citations::cite(source, raw, w.start, w.end),
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
    description_format: String,
    project: String,
    tracker: String,
    status: String,
    status_category: String,
    author: String,
    assignee: Option<String>,
    parent: Option<String>,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
    estimated_hours: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    lock_version: i32,
}

pub async fn get(app: &App, caller: AuthContext, key: &str) -> Result<IssueDetail> {
    let mut tx = read_tx(app).await?;
    let (id, mut r) = get_in(&mut tx, caller, key).await?;
    // Design 06 R-M10: the web app's HTML (this route is for people only).
    let project: Uuid = sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let by_agent = crate::markdown::description_by_agent(&mut tx, id).await?;
    let html = crate::markdown::render_for(
        &mut tx,
        caller,
        project,
        &[crate::markdown::Text {
            text: &r.description,
            src: crate::markdown::Source {
                format: crate::commands::TextFormat::from_db(&r.description_format),
                agent: by_agent,
                container: id,
            },
        }],
    )
    .await?;
    r.description_html = html.into_iter().next();
    r.description_by_agent = Some(by_agent);
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
                i.assigned_to_id, i.is_private, i.subject, i.description, i.description_format,
                p.name AS project, t.name AS tracker, s.name AS status, s.category AS status_category,
                au.name AS author, a.name AS assignee, pk.key AS parent,
                i.start_date, i.due_date, trim_scale(i.estimated_hours)::text AS estimated_hours,
                i.created_at, i.updated_at, i.lock_version
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
    let (allowed_statuses, can_triage) =
        match crate::perm::project_perms(&mut *conn, &actor, row.project_id).await? {
            None => (Vec::new(), false),
            Some(pp) => {
                let can_triage = pp.writable()
                    && pp.allowed_any(crate::perm::TRIAGE_ISSUES)
                    && actor.delegation_allows(row.project_id, crate::perm::OP_TRIAGE_DECIDE, None);
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
                let names = crate::describe::allowed_transitions(&mut *conn, &actor, &pp, &core)
                    .await?
                    .into_iter()
                    .map(|(_, name)| name)
                    .collect();
                (names, can_triage)
            }
        };
    let triage = crate::triage::issue_state(&mut *conn, row.id, can_triage).await?;
    let form_rule_marks =
        crate::forms::open_marks(&mut *conn, caller, row.id, row.project_id).await?;
    let spent_hours = match caller.user_id() {
        Some(me) => {
            crate::time_reports::issue_spent(&mut *conn, me, row.project_id, row.id).await?
        }
        None => None,
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
    let watch = match caller.user_id() {
        Some(me) => Some(crate::notifications::watch_info(&mut *conn, me, row.id).await?),
        None => None,
    };

    Ok((
        row.id,
        IssueDetail {
            requested_key: (row.current_key != key).then_some(key.clone()),
            key: row.current_key,
            subject: row.subject,
            description: row.description,
            description_format: row.description_format,
            description_html: None,
            description_by_agent: None,
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
            estimated_hours: row.estimated_hours,
            spent_hours,
            created_at: row.created_at,
            updated_at: row.updated_at,
            lock_version: row.lock_version,
            allowed_statuses,
            custom_fields,
            triage,
            form_rule_marks,
            watch,
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
