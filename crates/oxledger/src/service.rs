// SPDX-License-Identifier: GPL-2.0-or-later
//! Read-side use cases shared by MCP (and REST in M1): search, get, resolve.
//! Every query goes through the caller's per-request scope.

use crate::db::{self, Caller, push_pred};
use crate::error::{ApiError, ErrorCode, api};
use crate::filter::{self, FilterInput};
use crate::search::SearchIndex;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::workflow::{Transition, allowed_statuses};
use ox_core::{IssueFacts, Pred};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

pub struct App {
    pub pool: PgPool,
    pub index: SearchIndex,
}

/// A bad read-tool parameter (`invalid_argument`, contract 02 §4.4).
pub(crate) fn user_err(msg: impl Into<String>) -> anyhow::Error {
    api(ErrorCode::InvalidArgument, msg)
}

const MAX_LIMIT: u32 = 100;
const DEFAULT_LIMIT: u32 = 20;
const SEARCH_CANDIDATES: usize = 500;
/// With free text, the ranked candidate list grows 500 → 2000 → 8000 until
/// enough candidates survive the SQL re-check (visibility drift, status and
/// custom field filters live only in SQL). Beyond this the result is truncated.
const MAX_SEARCH_CANDIDATES: usize = 8000;

// ---------------------------------------------------------------- types

#[derive(Debug, Default, Deserialize, JsonSchema)]
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
    /// Custom field filter, e.g. {"and": [{"field": "Severity", "op": "in", "value": ["High", "Critical"]},
    /// {"field": "Estimate (h)", "op": "between", "value": [2, 8]}]}. Combined with the other filters by AND.
    #[schemars(with = "Option<FilterInput>")]
    pub filter: Option<serde_json::Value>,
    /// Max items, default 20, max 100.
    pub limit: Option<u32>,
    /// Opaque cursor from a previous `next_cursor`.
    pub cursor: Option<String>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[sqlx(skip)]
    #[serde(skip)]
    #[schemars(skip)]
    pub id: Uuid,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResult {
    pub items: Vec<IssueBrief>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Only with `q`: the server stopped after ranking 8000 candidates without
    /// finding everything this page needs. More matches may exist (even if there
    /// is no `next_cursor`); narrow `q` or the filters.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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
    caller: Caller,
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
    let scope = db::load_scope(&mut *app.pool.acquire().await?, caller).await?;
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
                 WHERE p.status = 'active' AND l.label IS NOT NULL
               UNION ALL SELECT a.entity_kind, a.entity_id, COALESCE(pr.name, t.name, s.name, pp.name), a.label
                 FROM aliases a
                 LEFT JOIN projects pr ON a.entity_kind = 'project' AND pr.id = a.entity_id AND pr.id = ANY($2)
                 LEFT JOIN trackers t ON a.entity_kind = 'tracker' AND t.id = a.entity_id
                 LEFT JOIN issue_statuses s ON a.entity_kind = 'status' AND s.id = a.entity_id
                 LEFT JOIN principals pp ON a.entity_kind = 'principal' AND pp.id = a.entity_id
                 WHERE COALESCE(pr.name, t.name, s.name, pp.name) IS NOT NULL
             ) AS x(kind, id, name, label)
             WHERE {cmp} AND ($3::text IS NULL OR x.kind = $3)
             ORDER BY kind, name LIMIT 20"
        );
        let found: Vec<Candidate> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(t)
            .bind(&visible_projects)
            .bind(kind)
            .fetch_all(&app.pool)
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
                .fetch_all(&app.pool)
                .await?
        }
        Some("status") => {
            sqlx::query_scalar("SELECT name FROM issue_statuses ORDER BY position, name")
                .fetch_all(&app.pool)
                .await?
        }
        Some("project") => {
            sqlx::query_scalar(
                "SELECT key || ' ' || name FROM projects WHERE id = ANY($1) ORDER BY key",
            )
            .bind(&visible_projects)
            .fetch_all(&app.pool)
            .await?
        }
        _ => Vec::new(),
    };
    Ok(ResolveResult {
        candidates: Vec::new(),
        valid_values,
    })
}

/// Resolves a filter value to exactly one id of `kind`, or explains why not.
pub(crate) async fn resolve_one(app: &App, caller: Caller, kind: &str, text: &str) -> Result<Uuid> {
    let r = resolve(app, caller, text, Some(kind)).await?;
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

pub async fn search(app: &App, caller: Caller, p: SearchParams) -> Result<SearchResult> {
    let scope = db::load_scope(&mut *app.pool.acquire().await?, caller).await?;
    let visible = scope.predicate();
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
    let offset: i64 = match p.cursor.as_deref() {
        None => 0,
        Some(c) => c.parse().map_err(|_| {
            api(
                ErrorCode::InvalidCursor,
                "invalid cursor; search again without it",
            )
        })?,
    };

    let mut filters: Vec<Pred> = Vec::new();
    let mut sql_extra: Vec<(&str, Uuid)> = Vec::new();
    if let Some(v) = p.project.as_deref() {
        filters.push(Pred::ProjectIs(
            resolve_one(app, caller, "project", v).await?,
        ));
    }
    if let Some(v) = p.tracker.as_deref() {
        filters.push(Pred::TrackerIn(vec![
            resolve_one(app, caller, "tracker", v).await?,
        ]));
    }
    if let Some(v) = p.status.as_deref() {
        sql_extra.push((
            "i.status_id = ",
            resolve_one(app, caller, "status", v).await?,
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
        Some("me") => match caller.0 {
            Some(me) => filters.push(Pred::AssigneeIn(vec![me])),
            None => return Err(user_err("assignee=me needs an authenticated caller")),
        },
        Some(v) => filters.push(Pred::AssigneeIn(vec![
            resolve_one(app, caller, "principal", v).await?,
        ])),
    }

    let custom = match &p.filter {
        None => None,
        Some(v) => Some(filter::resolve(app, caller, &scope, &filter::parse_input(v)?).await?),
    };

    let mut all = vec![visible];
    all.extend(filters);
    let pred = Pred::And(all).simplify();

    let build = |ids: Option<Vec<Uuid>>| {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(BRIEF_SELECT);
        push_pred(&mut qb, "i", &pred);
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
        // Custom fields live only in PostgreSQL.
        if let Some(f) = &custom {
            qb.push(" AND ");
            filter::push_filter(&mut qb, "i", f);
        }
        match ids {
            Some(ids) => {
                qb.push(" AND i.id = ANY(").push_bind(ids).push(")");
            }
            None => {
                qb.push(" ORDER BY i.updated_at DESC, i.id DESC LIMIT ")
                    .push_bind(limit + 1)
                    .push(" OFFSET ")
                    .push_bind(offset);
            }
        }
        qb
    };

    let mut truncated = false;
    let mut items = match p.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        None => rows_to_briefs(build(None).build_query_as().fetch_all(&app.pool).await?),
        // Free text: rank in the index (already filtered by visibility + the
        // index-side filters), then re-check every candidate in SQL — the index
        // may lag the database, and some filters exist only in SQL. Grow the
        // candidate list until this page and one lookahead item survive.
        // Best effort: each round asks the index again, so a concurrent index
        // update can shift the ranking between rounds.
        Some(q) => {
            let need = (offset + limit + 1) as usize;
            let mut k = SEARCH_CANDIDATES;
            let mut items = loop {
                let hits = app.index.search(q, &pred, k)?;
                let exhausted = hits.len() < k;
                let score_of: std::collections::HashMap<Uuid, f32> =
                    hits.iter().map(|h| (h.issue_id, h.score)).collect();
                let ids: Vec<Uuid> = score_of.keys().copied().collect();
                let mut items = rows_to_briefs(
                    build(Some(ids))
                        .build_query_as()
                        .fetch_all(&app.pool)
                        .await?,
                );
                for i in &mut items {
                    i.score = score_of.get(&i.id).copied();
                }
                if items.len() >= need || exhausted {
                    break items;
                }
                if k >= MAX_SEARCH_CANDIDATES {
                    truncated = true;
                    break items;
                }
                k = (k * 4).min(MAX_SEARCH_CANDIDATES);
            };
            items.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.key.cmp(&b.key))
            });
            items
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize + 1)
                .collect()
        }
    };
    let next_cursor = (items.len() as i64 > limit).then(|| (offset + limit).to_string());
    items.truncate(limit as usize);
    Ok(SearchResult {
        items,
        next_cursor,
        truncated,
    })
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

pub async fn get(app: &App, caller: Caller, key: &str) -> Result<IssueDetail> {
    let key = key.trim().to_uppercase();
    let not_found = || api(ErrorCode::NotFound, format!("no issue {key}"));
    let row: Option<DetailRow> = sqlx::query_as(
        "SELECT i.id, ck.key AS current_key, i.project_id, i.tracker_id, i.status_id, i.author_id,
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
    .fetch_optional(&app.pool)
    .await?;
    let row = row.ok_or_else(not_found)?;

    let scope = db::load_scope(&mut *app.pool.acquire().await?, caller).await?;
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
            let ok: bool = qb.build_query_scalar().fetch_one(&app.pool).await?;
            ok.then(|| pk.clone())
        }
    };
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(BRIEF_SELECT);
    qb.push("i.parent_id = ").push_bind(row.id).push(" AND ");
    push_pred(&mut qb, "i", &visible);
    qb.push(" ORDER BY k.number");
    let children = rows_to_briefs(qb.build_query_as().fetch_all(&app.pool).await?);

    let role_ids =
        db::role_ids_for(&mut *app.pool.acquire().await?, caller, row.project_id).await?;
    let transitions: Vec<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT tracker_id, role_id, old_status_id, new_status_id FROM workflow_transitions
         WHERE tracker_id = $1 AND old_status_id = $2",
    )
    .bind(row.tracker_id)
    .bind(row.status_id)
    .fetch_all(&app.pool)
    .await?;
    let transitions: Vec<Transition> = transitions
        .into_iter()
        .map(
            |(tracker_id, role_id, old_status_id, new_status_id)| Transition {
                tracker_id,
                role_id,
                old_status_id,
                new_status_id,
            },
        )
        .collect();
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM issue_statuses ORDER BY position, name")
            .fetch_all(&app.pool)
            .await?;
    let mut order: Vec<Uuid> = vec![row.status_id];
    order.extend(statuses.iter().map(|s| s.0).filter(|s| *s != row.status_id));
    let allowed = allowed_statuses(
        &transitions,
        &role_ids,
        row.tracker_id,
        row.status_id,
        &order,
    );
    let allowed_statuses = allowed
        .iter()
        .filter_map(|id| statuses.iter().find(|s| s.0 == *id).map(|s| s.1.clone()))
        .collect();
    let custom_fields = custom_field_values(app, caller, &role_ids, &row).await?;

    Ok(IssueDetail {
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
    })
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
async fn custom_field_values(
    app: &App,
    caller: Caller,
    roles: &[Uuid],
    row: &DetailRow,
) -> Result<Vec<CustomFieldValue>> {
    let admin = db::is_admin(&mut *app.pool.acquire().await?, caller).await?;
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
         ORDER BY d.position, d.name",
    )
    .bind(row.id)
    .bind(row.tracker_id)
    .bind(row.project_id)
    .bind(admin)
    .bind(roles)
    .fetch_all(&app.pool)
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
