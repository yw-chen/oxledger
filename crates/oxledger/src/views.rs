// SPDX-License-Identifier: GPL-2.0-or-later
//! Saved views (design 05 §2.3, §3.3; M1.2): personal and project views of
//! kind `list` or `board`, the built-in views, running a saved or unsaved
//! definition, group and lane counts.
//!
//! A view stores how to query, sort and draw (K3), never results. It always
//! runs with the opener's visibility and readable fields, through the one
//! search path ([`service::search_with`]), so the Filter AST v2 compiler,
//! keyset cursors and the 2 s work budget are the search's. Counts and
//! totals (R-VD6, B-7) are computed in SQL over the opener's visible issues,
//! capped at 999, in their own 2 s budget; out of time they are `null`.
//!
//! People use the REST endpoints (browser session); agents read project and
//! built-in views through MCP `view_list` / `view_run` with their own read
//! scope and never see a person's personal views (B-4).

use crate::auth::AuthContext;
use crate::commands;
use crate::db::{self, push_pred};
use crate::describe;
use crate::error::{ApiError, ErrorCode, api, classify};
use crate::filter::{self, v2};
use crate::perm::{self, MANAGE_PUBLIC_VIEWS};
use crate::service::{self, App, CfTarget, Extra, SearchItem, SearchParams, read_tx};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::Pred;
use ox_core::filter::{Cmp, FieldType, Filter, Test, Value as FValue};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// R-V4: personal views per owner, project views per project.
pub const MAX_PERSONAL: i64 = 100;
pub const MAX_PROJECT: i64 = 50;
pub const MAX_NAME_CHARS: usize = 60;
/// The stored definition (0021 CHECK).
pub const MAX_DEF_BYTES: usize = 32 * 1024;
pub const MAX_COLUMNS: usize = 12;
pub const MAX_CARD_FIELDS: usize = 8;
pub const MAX_LANES: usize = 30;
pub const MAX_LANE_VALUES: usize = 100;
pub const MAX_LANE_TITLE_CHARS: usize = 30;
pub const MAX_Q_CHARS: usize = 500;
/// R-VD6: counts stop at 999 (`capped` beyond).
pub const COUNT_CAP: i64 = 999;
/// R-VD6: the counts and totals of one run share this budget.
pub const COUNT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
pub const DEFAULT_DONE_WINDOW: i64 = 14;
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 100;
const EVENTS_PAGE: i64 = 50;

pub const ACTION_CREATE: &str = "view_create";
pub const ACTION_UPDATE: &str = "view_update";
pub const ACTION_DELETE: &str = "view_delete";
pub const ACTION_COPY: &str = "view_copy";

/// Built-in columns of a list (and card fields of a board), besides `cf:<id>`.
pub const COLUMNS: &[&str] = &[
    "key",
    "subject",
    "project",
    "tracker",
    "status",
    "status_category",
    "assignee",
    "author",
    "priority",
    "fixed_version",
    "is_private",
    "start_date",
    "due_date",
    "created_at",
    "updated_at",
    "estimated_hours",
    "spent_hours",
    "triage",
];
/// Built-in `group_by` keys (list, cf: list, user and bool types).
pub const GROUP_BY: &[&str] = &[
    "status",
    "assignee",
    "priority",
    "tracker",
    "fixed_version",
    "project",
];
/// Built-in `lanes.by` keys (board, cf: list and user types).
pub const LANES_BY: &[&str] = &[
    "status",
    "status_category",
    "assignee",
    "priority",
    "fixed_version",
];
pub const SWIMLANES_BY: &[&str] = &["assignee", "priority", "fixed_version", "project"];
/// R-VD7: `spent_hours` sums only the time the opener can see (§3.5.4).
pub const TOTALS: &[&str] = &["estimated_hours", "spent_hours"];
const DEFAULT_LIST_COLUMNS: &[&str] = &[
    "key",
    "subject",
    "status",
    "assignee",
    "priority",
    "updated_at",
];
const DEFAULT_CARD_FIELDS: &[&str] = &["key", "subject", "assignee", "priority", "due_date"];

// ---------------------------------------------------------------- published shapes

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectRef {
    pub id: Uuid,
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PersonRef {
    pub id: Uuid,
    pub name: String,
    /// `user`, `group` or `agent`.
    pub kind: String,
}

/// One entry of `GET /views` and MCP `view_list`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewSummary {
    /// A uuid, or a built-in id: `builtin:assigned_to_me`,
    /// `builtin:reported_by_me`, `builtin:involving_me`,
    /// `builtin:project_issues:<project id>`, `builtin:project_board:<project id>`.
    pub id: String,
    /// `builtin`, `personal` or `project`.
    pub scope: String,
    /// `list` or `board`.
    pub kind: String,
    pub name: String,
    /// The caller may change it (built-in: never).
    pub editable: bool,
    /// The project it is bound to (project views, project built-ins, and
    /// personal views bound to one project).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    /// A personal view bound to a project the caller can no longer see
    /// (left it, or it became private): `project` is left out and a run is
    /// `invalid_view` (reason `project_unavailable`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_unavailable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<PersonRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewList {
    pub items: Vec<ViewSummary>,
    pub next_cursor: Option<String>,
}

/// `GET /views/{id}` and the result of every view write.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewDetail {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    /// A personal view bound to a project the caller can no longer see
    /// (left it, or it became private): `project` is left out and a run is
    /// `invalid_view` (reason `project_unavailable`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_unavailable: bool,
    /// The normalized definition (design 05 §3.3): ids instead of names;
    /// `me`, `next` and relative dates kept dynamic.
    pub def: Json,
    pub editable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_version: Option<i32>,
    /// Personal: the owner; project: who created it (not an owner).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<PersonRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<PersonRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// `POST /views` (Idempotency-Key).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewView {
    /// `personal` or `project` (needs manage_public_views in an active project).
    pub scope: String,
    /// Project id or key: required for `project`; optional for `personal`
    /// (binds the view to that project; leave out for a cross-project view).
    #[serde(default)]
    pub project_id: Option<String>,
    /// `list` or `board`.
    pub kind: String,
    /// 1–60 characters, unique per owner (personal) or project (project).
    pub name: String,
    /// The definition (design 05 §3.3); `def.kind` may be left out.
    pub def: Json,
}

/// `PATCH /views/{id}` (Idempotency-Key): `def` replaces the whole definition.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewPatch {
    pub lock_version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub def: Option<Json>,
}

/// `POST /views/{id}/copy` (Idempotency-Key): an independent new view.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewCopy {
    /// `personal` (default) or `project` (manage_public_views).
    #[serde(default)]
    pub scope: Option<String>,
    /// Default: the source's project (none for the cross-project built-ins).
    #[serde(default)]
    pub project_id: Option<String>,
    /// Default: the source's name + "（副本）".
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewDeleted {
    pub id: Uuid,
    pub lock_version: i32,
}

/// Narrowing for this run only (not saved).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunOverride {
    /// Free text; replaces the view's own `query.q`.
    #[serde(default)]
    pub q: Option<String>,
    /// A Filter AST v2 ANDed with the view's conditions.
    #[serde(default)]
    pub where_and: Option<Json>,
}

/// `POST /views/{id}/run`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunInput {
    #[serde(default)]
    pub cursor: Option<String>,
    /// List: items per page; board: items per lane (default 50, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Board: only this lane (its next page with `cursor`). A board cursor
    /// always comes with its lane.
    #[serde(default)]
    pub lane: Option<String>,
    /// Board: only this swimlane value (an id, or `none` for 未設定).
    #[serde(default)]
    pub swimlane: Option<String>,
    #[serde(default, rename = "override")]
    pub override_: Option<RunOverride>,
}

/// `POST /views/preview`: an unsaved definition, checked as it would be
/// saved with `scope` and `project_id`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewInput {
    /// The definition; `def.kind` is required here.
    pub def: Json,
    /// `personal` (default) or `project`: which save rules apply (R-VD3).
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub swimlane: Option<String>,
    #[serde(default, rename = "override")]
    pub override_: Option<RunOverride>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewHead {
    pub id: String,
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ColumnHead {
    pub key: String,
    pub label: String,
}

/// R-VD6: a count of visible issues, at most 999 (`capped`: more than that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Count {
    pub value: i64,
    pub capped: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GroupHead {
    /// The group's value (an id, a category, true/false), null = not set.
    pub value: Json,
    pub label: String,
    /// null: not counted (out of time, or the run has free text).
    pub count: Option<Count>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Total {
    /// Decimal string with two decimals; null when not computed.
    pub value: Option<String>,
    /// false: not computed (out of time, or the run has free text).
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Coverage {
    /// Free text only: more matches than the search looks at (8000).
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunItem {
    pub key: String,
    pub id: Uuid,
    pub lock_version: i32,
    /// List with `group_by`: the item's group value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<Json>,
    /// Board with swimlanes: the item's swimlane value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swimlane: Option<Json>,
    /// List: one value per column (not `key`), plus `overdue`. People and
    /// groups `{id, name, kind}`; status `{id, name, category}`; tracker,
    /// priority, version `{id, name}`; project `{id, key, name}`; list
    /// custom fields `{id, label, active}`; others plain; null = not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Map<String, Json>>,
    /// Board: the card fields (not `key`), plus `overdue`; shapes as `values`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Map<String, Json>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Wip {
    pub max: i64,
    /// null when the lane was not counted.
    pub exceeded: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LaneOut {
    pub id: String,
    pub title: String,
    /// The lane's values (ids, categories; null = 未設定).
    pub values: Vec<Json>,
    pub count: Option<Count>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wip: Option<Wip>,
    pub items: Vec<RunItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SwimlaneHead {
    pub value: Json,
    pub label: String,
}

/// What a board does not show (R-VD6: counts are of the visible results,
/// so nothing leaves the board silently).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Unmapped {
    /// Lanes by status: statuses in no lane (their issues are not shown,
    /// as in Jira).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_count: Option<i64>,
    /// Visible matches (done window and swimlane applied) in no lane: a
    /// value no lane lists (a status, an inactive option, a closed version,
    /// a person past the lane limit) or a custom field that does not apply
    /// to the issue. null when not counted (out of time, or free text).
    pub issues: Option<Count>,
    /// Lanes made from values stopped at the lane limit (30); the issues of
    /// the values left out are in `issues`.
    pub lanes_truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunMeta {
    /// The organization's date (relative dates).
    pub today: String,
}

/// `POST /views/{id}/run`, `POST /views/preview`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewRun {
    pub view: ViewHead,
    /// List: the columns shown (custom fields the caller cannot read are left out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnHead>>,
    /// List with `group_by`: the groups of this page's items, in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<GroupHead>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<RunItem>>,
    /// List with `totals`: over every visible match, not only this page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<BTreeMap<String, Total>>,
    /// List: the next page. Board: always null (each lane has its own).
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lanes: Option<Vec<LaneOut>>,
    /// Board with swimlanes: the swimlane values of the items returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swimlanes: Option<Vec<SwimlaneHead>>,
    /// Board: what no lane shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<Unmapped>,
    pub meta: RunMeta,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewEvent {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    /// `created`, `updated`, `deleted`.
    pub kind: String,
    pub actor: PersonRef,
    /// `{before, after}` (each `{name, def}` or null); `"[已遮蔽]"` once redacted.
    pub payload: Json,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewEvents {
    pub items: Vec<ViewEvent>,
    pub next_cursor: Option<String>,
}

/// MCP `view_list`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewListParams {
    /// Only this project's views (key such as "AUTH", or id).
    #[serde(default)]
    pub project: Option<String>,
}

/// MCP `view_run`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewRunParams {
    /// A view id from view_list.
    pub view: String,
    /// List: items per page; board: per lane (default 50, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
    /// Board: only this lane (needed with a cursor).
    #[serde(default)]
    pub lane: Option<String>,
    /// Columns of each item, as search's `fields`.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentLane {
    pub id: String,
    pub title: String,
    /// Visible to you only; not a total of the project.
    pub count: Option<Count>,
    pub items: Vec<SearchItem>,
    pub next_cursor: Option<String>,
}

/// MCP `view_run`: list → `items` (search's compact lines); board → `lanes`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentViewRun {
    pub view: ViewHead,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<SearchItem>>,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lanes: Option<Vec<AgentLane>>,
    /// Board: what no lane shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<Unmapped>,
    pub meta: RunMeta,
}

// ---------------------------------------------------------------- errors

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such view")
}

fn invalid_view(path: &str, reason: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidView, msg)
        .with_details(json!({"path": path, "reason": reason}))
        .into()
}

fn invalid_value(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn forbidden(reason: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": reason}))
        .into()
}

fn bad_arg(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

/// An `invalid_filter` with its `details.path` moved under `to`; with
/// `unavailable`, `unknown_field` becomes `field_unavailable` (R-VD4: a
/// saved condition on a field the opener cannot read stops the run).
fn repath(e: anyhow::Error, from: &str, to: &str, unavailable: bool) -> anyhow::Error {
    let Some(a) = classify(&e) else {
        return e;
    };
    if a.code != ErrorCode::InvalidFilter {
        return e;
    }
    let mut a = a.clone();
    if let Some(d) = a.details.as_mut() {
        if let Some(rest) = d["path"].as_str().and_then(|p| p.strip_prefix(from)) {
            d["path"] = json!(format!("{to}{rest}"));
        }
        if unavailable && d["reason"] == "unknown_field" {
            d["reason"] = json!("field_unavailable");
            a.message =
                "this view has a condition on a field you cannot read, so it cannot run".into();
        }
    }
    a.into()
}

fn field_unavailable(path: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidFilter,
        "this view uses a field you cannot read, so it cannot run",
    )
    .with_details(json!({"path": path, "reason": "field_unavailable"}))
    .into()
}

// ---------------------------------------------------------------- the definition

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    List,
    Board,
}

impl Kind {
    fn parse(s: &str) -> Option<Kind> {
        match s {
            "list" => Some(Kind::List),
            "board" => Some(Kind::Board),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Kind::List => "list",
            Kind::Board => "board",
        }
    }
}

#[derive(Debug, Clone)]
struct LaneDef {
    id: String,
    title: String,
    values: Vec<Json>,
    wip_max: Option<i64>,
}

#[derive(Debug, Clone)]
struct Lanes {
    by: String,
    items: Option<Vec<LaneDef>>,
    show_empty: bool,
}

/// A definition after the shape checks (no database).
#[derive(Debug, Clone)]
struct Def {
    kind: Kind,
    where_: Option<Json>,
    q: Option<String>,
    columns: Vec<String>,
    sort: Option<Vec<v2::SortInput>>,
    group_by: Option<String>,
    totals: Vec<String>,
    lanes: Option<Lanes>,
    swimlanes: Option<String>,
    card_fields: Vec<String>,
    done_window_days: i64,
}

impl Def {
    fn to_json(&self) -> Json {
        let query = json!({"where": self.where_, "q": self.q});
        let sort = self.sort.as_ref().map(|s| {
            s.iter()
                .map(|k| {
                    json!({"field": k.field, "dir": if k.dir == v2::SortDir::Desc { "desc" } else { "asc" }})
                })
                .collect::<Vec<_>>()
        });
        match self.kind {
            Kind::List => json!({
                "kind": "list", "query": query, "columns": self.columns, "sort": sort,
                "group_by": self.group_by, "totals": self.totals,
            }),
            Kind::Board => {
                let lanes = self.lanes.as_ref().map(|l| {
                    json!({
                        "by": l.by,
                        "items": l.items.as_ref().map(|items| items.iter().map(|i| {
                            let mut o = json!({"id": i.id, "title": i.title, "values": i.values});
                            if let Some(w) = i.wip_max {
                                o["wip_max"] = json!(w);
                            }
                            o
                        }).collect::<Vec<_>>()),
                        "show_empty": l.show_empty,
                    })
                });
                json!({
                    "kind": "board", "query": query, "lanes": lanes,
                    "swimlanes": self.swimlanes.as_ref().map(|b| json!({"by": b})),
                    "card_fields": self.card_fields, "sort": sort,
                    "done_window_days": self.done_window_days,
                })
            }
        }
    }

    fn hash(&self) -> String {
        let d = Sha256::digest(
            [
                b"oxledger/view-def/v1\n".as_slice(),
                self.to_json().to_string().as_bytes(),
            ]
            .concat(),
        );
        d[..16].iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn obj<'a>(v: &'a Json, path: &str, allowed: &[&str]) -> Result<&'a Map<String, Json>> {
    let o = v
        .as_object()
        .ok_or_else(|| invalid_view(path, "invalid_shape", format!("{path} must be an object")))?;
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid_view(
            &format!("{path}.{k}"),
            "unknown_field",
            format!(
                "{path}: unknown key {k:?} (allowed: {})",
                allowed.join(", ")
            ),
        ));
    }
    Ok(o)
}

fn present<'a>(o: &'a Map<String, Json>, k: &str) -> Option<&'a Json> {
    o.get(k).filter(|v| !v.is_null())
}

fn strings(v: &Json, path: &str) -> Result<Vec<String>> {
    v.as_array()
        .ok_or_else(|| invalid_view(path, "invalid_shape", format!("{path} must be an array")))?
        .iter()
        .enumerate()
        .map(|(i, x)| {
            x.as_str().map(|s| s.trim().to_string()).ok_or_else(|| {
                invalid_view(
                    &format!("{path}[{i}]"),
                    "invalid_shape",
                    format!("{path}[{i}] must be a string"),
                )
            })
        })
        .collect()
}

fn int_in(v: &Json, path: &str, lo: i64, hi: i64) -> Result<i64> {
    match v.as_i64() {
        Some(n) if (lo..=hi).contains(&n) => Ok(n),
        _ => Err(invalid_view(
            path,
            "invalid_value",
            format!("{path} is an integer from {lo} to {hi}"),
        )),
    }
}

/// The shape checks (design 05 §3.3, 01 §16.5: unknown keys are refused).
/// `kind` is the view's kind when the caller already knows it.
fn parse_shape(v: &Json, kind: Option<Kind>) -> Result<Def> {
    if v.to_string().len() > MAX_DEF_BYTES {
        return Err(invalid_view(
            "def",
            "too_large",
            format!("a view definition is at most {MAX_DEF_BYTES} bytes"),
        ));
    }
    let top = v
        .as_object()
        .ok_or_else(|| invalid_view("def", "invalid_shape", "def must be an object"))?;
    let given = match present(top, "kind") {
        None => None,
        Some(k) => Some(k.as_str().and_then(Kind::parse).ok_or_else(|| {
            invalid_view("def.kind", "invalid_value", "def.kind is list or board")
        })?),
    };
    let kind = match (kind, given) {
        (Some(a), Some(b)) if a != b => {
            return Err(invalid_view(
                "def.kind",
                "kind_mismatch",
                "def.kind differs from the view's kind (a view keeps its kind; create a new one)",
            ));
        }
        (Some(a), _) | (None, Some(a)) => a,
        (None, None) => {
            return Err(invalid_view(
                "def.kind",
                "invalid_shape",
                "def.kind (list or board) is required",
            ));
        }
    };
    let allowed: &[&str] = match kind {
        Kind::List => &["kind", "query", "columns", "sort", "group_by", "totals"],
        Kind::Board => &[
            "kind",
            "query",
            "lanes",
            "swimlanes",
            "card_fields",
            "sort",
            "done_window_days",
        ],
    };
    let top = obj(v, "def", allowed)?;
    let (where_, q) = match present(top, "query") {
        None => (None, None),
        Some(qv) => {
            let qo = obj(qv, "def.query", &["where", "q"])?;
            let q = match present(qo, "q") {
                None => None,
                Some(x) => {
                    let s = x.as_str().ok_or_else(|| {
                        invalid_view("def.query.q", "invalid_shape", "def.query.q is a string")
                    })?;
                    let s = s.trim();
                    if s.chars().count() > MAX_Q_CHARS {
                        return Err(invalid_view(
                            "def.query.q",
                            "invalid_value",
                            format!("def.query.q is at most {MAX_Q_CHARS} characters"),
                        ));
                    }
                    (!s.is_empty()).then(|| s.to_string())
                }
            };
            (present(qo, "where").cloned(), q)
        }
    };
    let sort = match present(top, "sort") {
        None => None,
        Some(s) => {
            let keys: Vec<v2::SortInput> = serde_json::from_value(s.clone()).map_err(|e| {
                invalid_view(
                    "def.sort",
                    "invalid_shape",
                    format!("def.sort: [{{field, dir}}] ({e})"),
                )
            })?;
            (!keys.is_empty()).then_some(keys)
        }
    };
    let mut def = Def {
        kind,
        where_,
        q,
        columns: Vec::new(),
        sort,
        group_by: None,
        totals: Vec::new(),
        lanes: None,
        swimlanes: None,
        card_fields: Vec::new(),
        done_window_days: DEFAULT_DONE_WINDOW,
    };
    match kind {
        Kind::List => {
            def.columns = match present(top, "columns") {
                None => DEFAULT_LIST_COLUMNS.iter().map(|s| s.to_string()).collect(),
                Some(c) => strings(c, "def.columns")?,
            };
            if def.columns.is_empty() || def.columns.len() > MAX_COLUMNS {
                return Err(invalid_view(
                    "def.columns",
                    "invalid_value",
                    format!("def.columns has 1 to {MAX_COLUMNS} keys"),
                ));
            }
            def.group_by = match present(top, "group_by") {
                None => None,
                Some(g) => Some(
                    g.as_str()
                        .ok_or_else(|| {
                            invalid_view(
                                "def.group_by",
                                "invalid_shape",
                                "def.group_by is a string",
                            )
                        })?
                        .trim()
                        .to_string(),
                ),
            };
            def.totals = match present(top, "totals") {
                None => Vec::new(),
                Some(t) => strings(t, "def.totals")?,
            };
        }
        Kind::Board => {
            let lv = present(top, "lanes").ok_or_else(|| {
                invalid_view(
                    "def.lanes",
                    "lanes_required",
                    "a board needs lanes {by, items?}",
                )
            })?;
            let lo = obj(lv, "def.lanes", &["by", "items", "show_empty"])?;
            let by = present(lo, "by")
                .and_then(Json::as_str)
                .ok_or_else(|| {
                    invalid_view("def.lanes.by", "invalid_shape", "def.lanes.by is required")
                })?
                .trim()
                .to_string();
            let show_empty = match present(lo, "show_empty") {
                None => true,
                Some(b) => b.as_bool().ok_or_else(|| {
                    invalid_view(
                        "def.lanes.show_empty",
                        "invalid_shape",
                        "show_empty is true or false",
                    )
                })?,
            };
            let items = match present(lo, "items") {
                None => None,
                Some(arr) => {
                    let arr = arr.as_array().ok_or_else(|| {
                        invalid_view(
                            "def.lanes.items",
                            "invalid_shape",
                            "def.lanes.items is an array",
                        )
                    })?;
                    if arr.is_empty() || arr.len() > MAX_LANES {
                        return Err(invalid_view(
                            "def.lanes.items",
                            "invalid_value",
                            format!("a board has 1 to {MAX_LANES} lanes"),
                        ));
                    }
                    let mut out: Vec<LaneDef> = Vec::new();
                    for (n, it) in arr.iter().enumerate() {
                        let path = format!("def.lanes.items[{n}]");
                        let io = obj(it, &path, &["id", "title", "values", "wip_max"])?;
                        let id = match present(io, "id") {
                            None => format!("l{}", n + 1),
                            Some(x) => {
                                let s = x.as_str().unwrap_or("").trim().to_string();
                                let ok = !s.is_empty()
                                    && s.len() <= 40
                                    && s.bytes().all(|b| {
                                        b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
                                    });
                                if !ok {
                                    return Err(invalid_view(
                                        &format!("{path}.id"),
                                        "invalid_value",
                                        "a lane id is 1–40 of A–Z, a–z, 0–9, _ and -",
                                    ));
                                }
                                s
                            }
                        };
                        if out.iter().any(|l| l.id == id) {
                            return Err(invalid_view(
                                &format!("{path}.id"),
                                "duplicate_value",
                                "two lanes have the same id",
                            ));
                        }
                        let title = present(io, "title")
                            .and_then(Json::as_str)
                            .map(str::trim)
                            .unwrap_or("")
                            .to_string();
                        let chars = title.chars().count();
                        if chars == 0
                            || chars > MAX_LANE_TITLE_CHARS
                            || title.chars().any(char::is_control)
                        {
                            return Err(invalid_view(
                                &format!("{path}.title"),
                                "invalid_value",
                                format!(
                                    "a lane title is 1 to {MAX_LANE_TITLE_CHARS} characters, no control characters"
                                ),
                            ));
                        }
                        let values = match present(io, "values") {
                            Some(Json::Array(vs))
                                if !vs.is_empty() && vs.len() <= MAX_LANE_VALUES =>
                            {
                                vs.clone()
                            }
                            _ => {
                                return Err(invalid_view(
                                    &format!("{path}.values"),
                                    "invalid_value",
                                    format!("a lane has 1 to {MAX_LANE_VALUES} values"),
                                ));
                            }
                        };
                        let wip_max = present(io, "wip_max")
                            .map(|w| int_in(w, &format!("{path}.wip_max"), 1, 999))
                            .transpose()?;
                        out.push(LaneDef {
                            id,
                            title,
                            values,
                            wip_max,
                        });
                    }
                    Some(out)
                }
            };
            def.lanes = Some(Lanes {
                by,
                items,
                show_empty,
            });
            def.swimlanes = match present(top, "swimlanes") {
                None => None,
                Some(s) => {
                    let so = obj(s, "def.swimlanes", &["by"])?;
                    present(so, "by")
                        .map(|b| {
                            b.as_str().map(|s| s.trim().to_string()).ok_or_else(|| {
                                invalid_view(
                                    "def.swimlanes.by",
                                    "invalid_shape",
                                    "def.swimlanes.by is a string",
                                )
                            })
                        })
                        .transpose()?
                }
            };
            def.card_fields = match present(top, "card_fields") {
                None => DEFAULT_CARD_FIELDS.iter().map(|s| s.to_string()).collect(),
                Some(c) => strings(c, "def.card_fields")?,
            };
            if def.card_fields.is_empty() || def.card_fields.len() > MAX_CARD_FIELDS {
                return Err(invalid_view(
                    "def.card_fields",
                    "invalid_value",
                    format!("def.card_fields has 1 to {MAX_CARD_FIELDS} keys"),
                ));
            }
            def.done_window_days = match present(top, "done_window_days") {
                None => DEFAULT_DONE_WINDOW,
                Some(d) => int_in(d, "def.done_window_days", 1, 365)?,
            };
        }
    }
    Ok(def)
}

/// A readable custom field by `cf:<id>` or `cf:<name>`.
fn find_cf<'a>(
    fields: &'a [(custom_fields_row::FieldRow, Pred)],
    key: &str,
) -> Option<&'a (custom_fields_row::FieldRow, Pred)> {
    let rest = key.strip_prefix("cf:")?.trim();
    match rest.parse::<i32>() {
        Ok(id) => fields.iter().find(|(f, _)| f.id == id),
        Err(_) => {
            let l = rest.to_lowercase();
            fields.iter().find(|(f, _)| f.name.to_lowercase() == l)
        }
    }
}

mod custom_fields_row {
    pub use crate::custom_fields::FieldRow;
}

type Fields = Vec<(custom_fields_row::FieldRow, Pred)>;

/// Where a definition is being saved: the project it is bound to and
/// whether it is project-public (R-VD3).
#[derive(Debug, Clone, Copy)]
struct SaveCtx {
    project: Option<Uuid>,
    public: bool,
}

/// One column, card field, sort, group or lane key: built-in, or a readable
/// custom field (normalized to `cf:<id>`). `cf_types` limits the custom
/// field types; a restricted field in a public view is refused.
fn norm_key(
    fields: &Fields,
    key: &str,
    builtin: &[&str],
    cf_types: Option<&[FieldType]>,
    ctx: SaveCtx,
    path: &str,
) -> Result<String> {
    if builtin.contains(&key) {
        return Ok(key.to_string());
    }
    let unknown = || {
        invalid_view(
            path,
            "unknown_field",
            format!(
                "{path}: unknown key {key:?} (built-in: {}; custom fields: cf:<id>)",
                builtin.join(", ")
            ),
        )
    };
    let (row, _) = find_cf(fields, key).ok_or_else(unknown)?;
    let ty = row.field_type()?;
    if cf_types.is_some_and(|t| !t.contains(&ty)) {
        return Err(unknown());
    }
    if ctx.public && !row.visible {
        return Err(invalid_view(
            path,
            "restricted_field",
            format!(
                "{path}: a project view cannot use a restricted field (everyone in the project sees the view)"
            ),
        ));
    }
    Ok(format!("cf:{}", row.id))
}

/// Visits every condition object of a normalized `where`.
fn each_cond(v: &Json, f: &mut impl FnMut(&Json)) {
    if let Some(ps) = v
        .get("and")
        .or_else(|| v.get("or"))
        .and_then(Json::as_array)
    {
        for p in ps {
            each_cond(p, f);
        }
    } else if v.get("field").is_some() {
        f(v);
    }
}

/// R-VD3 on a normalized `where`: no restricted field in a public view; in
/// a view bound to a project, `project` conditions name only that project.
fn check_where(fields: &Fields, w: &Json, ctx: SaveCtx) -> Result<()> {
    let mut err: Option<anyhow::Error> = None;
    each_cond(w, &mut |c| {
        if err.is_some() {
            return;
        }
        let field = c["field"].as_str().unwrap_or("");
        if ctx.public
            && field.starts_with("cf:")
            && find_cf(fields, field).is_some_and(|(r, _)| !r.visible)
        {
            err = Some(invalid_view(
                "def.query.where",
                "restricted_field",
                "a project view cannot filter on a restricted field (everyone in the project sees the view)",
            ));
        }
        if field == "project"
            && let Some(p) = ctx.project
        {
            let mine = p.to_string();
            let ok = match &c["value"] {
                Json::String(s) => *s == mine,
                Json::Array(xs) => xs.iter().all(|x| x.as_str() == Some(mine.as_str())),
                _ => true,
            };
            if !ok {
                err = Some(invalid_view(
                    "def.query.where",
                    "project_scope",
                    "a view of a project only searches that project",
                ));
            }
        }
    });
    err.map_or(Ok(()), Err)
}

/// The definition resolved and checked for saving (design 05 §3.3): names
/// become ids, keys are known and readable, R-VD3 holds. Same checks for a
/// preview.
async fn normalize(
    conn: &mut PgConnection,
    caller: AuthContext,
    input: &Json,
    kind: Option<Kind>,
    ctx: SaveCtx,
) -> Result<Def> {
    let mut def = parse_shape(input, kind)?;
    let scope = db::load_scope(&mut *conn, caller).await?;
    let clock = v2::clock(&mut *conn).await?;
    let fields: Fields = filter::readable_fields(&mut *conn, caller, &scope).await?;
    if let Some(w) = def.where_.take() {
        let r = v2::resolve(&mut *conn, caller, &scope, clock, &w, ctx.project)
            .await
            .map_err(|e| repath(e, "where", "def.query.where", false))?;
        check_where(&fields, &r.normalized, ctx)?;
        let mut w = r.normalized;
        // R-VD3 / R-T3: a project's view names its project (when the
        // condition fits the limits; the run adds it in any case).
        if let Some(p) = ctx.project {
            let pc = json!({"field": "project", "op": "eq", "value": p.to_string()});
            let with = match &w {
                Json::Object(o) if o.contains_key("and") => {
                    let mut parts = o["and"].as_array().cloned().unwrap_or_default();
                    if parts.contains(&pc) {
                        None
                    } else {
                        parts.insert(0, pc);
                        Some(json!({"and": parts}))
                    }
                }
                _ if w == pc => None,
                _ => Some(json!({"and": [pc, w.clone()]})),
            };
            if let Some(with) = with
                && v2::resolve(&mut *conn, caller, &scope, clock, &with, ctx.project)
                    .await
                    .is_ok()
            {
                w = with;
            }
        }
        def.where_ = Some(w);
    } else if let Some(p) = ctx.project {
        def.where_ = Some(json!({"field": "project", "op": "eq", "value": p.to_string()}));
    }
    if let Some(keys) = &def.sort {
        let (_, norm) = v2::resolve_sort(&fields, keys).map_err(|e| {
            let path = classify(&e)
                .and_then(|a| a.details)
                .and_then(|d| d["path"].as_str().map(str::to_string))
                .unwrap_or_else(|| "sort".to_string());
            invalid_view(
                &format!("def.{path}"),
                "unknown_field",
                format!("def.{path}: cannot sort by this key"),
            )
        })?;
        for (n, k) in keys.iter().enumerate() {
            let key = k.field.trim();
            if key.starts_with("cf:") {
                norm_key(
                    &fields,
                    key,
                    &[],
                    None,
                    ctx,
                    &format!("def.sort[{n}].field"),
                )?;
            }
        }
        def.sort = Some(serde_json::from_value(norm)?);
    }
    let dedupe = |keys: &[String], path: &str| -> Result<()> {
        for (i, k) in keys.iter().enumerate() {
            if keys[..i].contains(k) {
                return Err(invalid_view(
                    &format!("{path}[{i}]"),
                    "duplicate_value",
                    format!("{path}: {k} is listed twice"),
                ));
            }
        }
        Ok(())
    };
    match def.kind {
        Kind::List => {
            let mut cols = Vec::new();
            for (i, c) in def.columns.iter().enumerate() {
                cols.push(norm_key(
                    &fields,
                    c,
                    COLUMNS,
                    None,
                    ctx,
                    &format!("def.columns[{i}]"),
                )?);
            }
            dedupe(&cols, "def.columns")?;
            def.columns = cols;
            if let Some(g) = &def.group_by {
                def.group_by = Some(norm_key(
                    &fields,
                    g,
                    GROUP_BY,
                    Some(&[FieldType::List, FieldType::User, FieldType::Bool]),
                    ctx,
                    "def.group_by",
                )?);
            }
            for (i, t) in def.totals.iter().enumerate() {
                norm_key(
                    &fields,
                    t,
                    TOTALS,
                    Some(&[]),
                    ctx,
                    &format!("def.totals[{i}]"),
                )?;
            }
            dedupe(&def.totals, "def.totals")?;
        }
        Kind::Board => {
            let mut cards = Vec::new();
            for (i, c) in def.card_fields.iter().enumerate() {
                cards.push(norm_key(
                    &fields,
                    c,
                    COLUMNS,
                    None,
                    ctx,
                    &format!("def.card_fields[{i}]"),
                )?);
            }
            dedupe(&cards, "def.card_fields")?;
            def.card_fields = cards;
            if let Some(s) = &def.swimlanes {
                def.swimlanes = Some(norm_key(
                    &fields,
                    s,
                    SWIMLANES_BY,
                    Some(&[]),
                    ctx,
                    "def.swimlanes.by",
                )?);
            }
            let lanes = def.lanes.as_mut().expect("parse_shape: board lanes");
            lanes.by = norm_key(
                &fields,
                &lanes.by,
                LANES_BY,
                Some(&[FieldType::List, FieldType::User]),
                ctx,
                "def.lanes.by",
            )?;
            let by = lanes.by.clone();
            match &mut lanes.items {
                None if by == "status" => {
                    return Err(invalid_view(
                        "def.lanes.items",
                        "lanes_required",
                        "lanes by status list their statuses (lanes.items)",
                    ));
                }
                None if by == "fixed_version" && ctx.project.is_none() => {
                    return Err(invalid_view(
                        "def.lanes.items",
                        "lanes_required",
                        "lanes by fixed_version without a project list their versions (lanes.items)",
                    ));
                }
                None => {}
                Some(items) => {
                    let nullable =
                        !matches!(by.as_str(), "status" | "status_category" | "priority");
                    let mut seen: Vec<Json> = Vec::new();
                    for (n, item) in items.iter_mut().enumerate() {
                        let path = format!("def.lanes.items[{n}].values");
                        let mut out: Vec<Json> = Vec::new();
                        let concrete: Vec<Json> = item
                            .values
                            .iter()
                            .filter(|v| !v.is_null())
                            .cloned()
                            .collect();
                        if concrete.len() < item.values.len() && !nullable {
                            return Err(invalid_view(
                                &path,
                                "invalid_value",
                                format!("{path}: {by} is never empty (no null lane)"),
                            ));
                        }
                        if !concrete.is_empty() {
                            let c = json!({"field": by, "op": "in", "value": concrete});
                            let r = v2::resolve(&mut *conn, caller, &scope, clock, &c, ctx.project)
                                .await
                                .map_err(|e| repath(e, "where.value", &path, false))
                                .map_err(|e| repath(e, "where", &path, false))?;
                            match &r.normalized["value"] {
                                Json::Array(xs) => out.extend(xs.iter().cloned()),
                                x => out.push(x.clone()),
                            }
                        }
                        if concrete.len() < item.values.len() {
                            out.push(Json::Null);
                        }
                        for v in &out {
                            if seen.contains(v) {
                                return Err(invalid_view(
                                    &path,
                                    "duplicate_value",
                                    format!("{path}: {v} is in two lanes"),
                                ));
                            }
                            seen.push(v.clone());
                        }
                        item.values = out;
                    }
                }
            }
        }
    }
    let size = def.to_json().to_string().len();
    if size > MAX_DEF_BYTES {
        return Err(invalid_view(
            "def",
            "too_large",
            format!("a view definition is at most {MAX_DEF_BYTES} bytes"),
        ));
    }
    Ok(def)
}

// ---------------------------------------------------------------- built-in views

#[derive(Debug, Clone, PartialEq, Eq)]
enum Builtin {
    AssignedToMe,
    ReportedByMe,
    InvolvingMe,
    ProjectIssues(Uuid),
    ProjectBoard(Uuid),
}

impl Builtin {
    fn id(&self) -> String {
        match self {
            Builtin::AssignedToMe => "builtin:assigned_to_me".into(),
            Builtin::ReportedByMe => "builtin:reported_by_me".into(),
            Builtin::InvolvingMe => "builtin:involving_me".into(),
            Builtin::ProjectIssues(p) => format!("builtin:project_issues:{p}"),
            Builtin::ProjectBoard(p) => format!("builtin:project_board:{p}"),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Builtin::AssignedToMe => "指派給我",
            Builtin::ReportedByMe => "我建立的",
            Builtin::InvolvingMe => "與我相關",
            Builtin::ProjectIssues(_) => "議題",
            Builtin::ProjectBoard(_) => "看板",
        }
    }

    fn project(&self) -> Option<Uuid> {
        match self {
            Builtin::ProjectIssues(p) | Builtin::ProjectBoard(p) => Some(*p),
            _ => None,
        }
    }

    /// The fixed definition (R-VD5: read only; copy it to change it).
    fn def(&self) -> Json {
        let open = json!({"field": "status_category", "op": "in", "value": ["todo", "doing"]});
        let me_assigned = json!({"field": "assignee", "op": "in", "value": [{"me": true}]});
        let me_author = json!({"field": "author", "op": "eq", "value": {"me": true}});
        let newest = json!([{"field": "updated_at", "dir": "desc"}]);
        match self {
            Builtin::AssignedToMe => json!({
                "kind": "list", "query": {"where": {"and": [me_assigned, open]}, "q": null},
                "columns": ["key", "subject", "project", "status", "priority", "due_date"],
                "sort": [{"field": "due_date", "dir": "asc"}, {"field": "priority", "dir": "desc"}],
                "group_by": null, "totals": [],
            }),
            Builtin::ReportedByMe => json!({
                "kind": "list", "query": {"where": {"and": [me_author, open]}, "q": null},
                "columns": ["key", "subject", "project", "status", "assignee", "updated_at"],
                "sort": newest, "group_by": null, "totals": [],
            }),
            // The run adds "or I wrote to it in the last 30 days" (not a
            // Filter AST field, so not part of the definition).
            Builtin::InvolvingMe => json!({
                "kind": "list", "query": {"where": {"or": [me_assigned, me_author]}, "q": null},
                "columns": ["key", "subject", "project", "status", "assignee", "updated_at"],
                "sort": newest, "group_by": null, "totals": [],
            }),
            Builtin::ProjectIssues(p) => json!({
                "kind": "list",
                "query": {"where": {"and": [
                    {"field": "project", "op": "eq", "value": p.to_string()}, open]}, "q": null},
                "columns": ["key", "subject", "tracker", "status", "assignee", "priority",
                            "due_date", "updated_at"],
                "sort": newest, "group_by": null, "totals": [],
            }),
            Builtin::ProjectBoard(p) => json!({
                "kind": "board",
                "query": {"where": {"field": "project", "op": "eq", "value": p.to_string()}, "q": null},
                "lanes": {"by": "status_category", "items": [
                    {"id": "todo", "title": "待處理", "values": ["todo"]},
                    {"id": "doing", "title": "進行中", "values": ["doing"]},
                    {"id": "done", "title": "完成", "values": ["done"]}], "show_empty": true},
                "swimlanes": null,
                "card_fields": DEFAULT_CARD_FIELDS,
                "sort": null,
                "done_window_days": DEFAULT_DONE_WINDOW,
            }),
        }
    }
}

enum ViewRef {
    Builtin(Builtin),
    /// A project built-in named by key (resolved against visibility).
    BuiltinProject {
        board: bool,
        project: String,
    },
    Row(Uuid),
}

fn parse_id(s: &str) -> Result<ViewRef> {
    let s = s.trim();
    if let Ok(id) = Uuid::parse_str(s) {
        return Ok(ViewRef::Row(id));
    }
    match s {
        "builtin:assigned_to_me" => return Ok(ViewRef::Builtin(Builtin::AssignedToMe)),
        "builtin:reported_by_me" => return Ok(ViewRef::Builtin(Builtin::ReportedByMe)),
        "builtin:involving_me" => return Ok(ViewRef::Builtin(Builtin::InvolvingMe)),
        _ => {}
    }
    if let Some(p) = s.strip_prefix("builtin:project_issues:") {
        return Ok(ViewRef::BuiltinProject {
            board: false,
            project: p.to_string(),
        });
    }
    if let Some(p) = s.strip_prefix("builtin:project_board:") {
        return Ok(ViewRef::BuiltinProject {
            board: true,
            project: p.to_string(),
        });
    }
    Err(not_found())
}

// ---------------------------------------------------------------- loading

/// A view the caller may read.
#[derive(Debug, Clone)]
struct Loaded {
    id: String,
    builtin: Option<Builtin>,
    scope: String,
    kind: Kind,
    name: String,
    project: Option<Uuid>,
    def: Json,
    lock_version: Option<i32>,
    owner_id: Option<Uuid>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    updated_by: Option<Uuid>,
}

type ViewRow = (
    Uuid,
    String,
    Uuid,
    Option<Uuid>,
    String,
    String,
    Json,
    i32,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<Uuid>,
);

const ROW_SELECT: &str = "SELECT id, scope, owner_id, project_id, kind, name, def, lock_version,
        created_at, updated_at, updated_by FROM view_defs WHERE deleted_at IS NULL AND id = $1";

fn loaded_from_row(r: ViewRow) -> Result<Loaded> {
    Ok(Loaded {
        id: r.0.to_string(),
        builtin: None,
        scope: r.1,
        owner_id: Some(r.2),
        project: r.3,
        kind: Kind::parse(&r.4).ok_or_else(|| anyhow::anyhow!("view kind {}", r.4))?,
        name: r.5,
        def: r.6,
        lock_version: Some(r.7),
        created_at: Some(r.8),
        updated_at: Some(r.9),
        updated_by: r.10,
    })
}

/// R-VD1, R-VD2, R-VD10: personal views only for their owner (agents never),
/// project views where the project is visible; anything else `not_found`.
async fn load(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: &str,
    lock: bool,
) -> Result<Loaded> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    match parse_id(id)? {
        ViewRef::Builtin(b) => Ok(builtin_loaded(b)),
        ViewRef::BuiltinProject { board, project } => {
            let p = describe::visible_project(&mut *conn, caller, &project)
                .await
                .map_err(|_| not_found())?;
            Ok(builtin_loaded(if board {
                Builtin::ProjectBoard(p)
            } else {
                Builtin::ProjectIssues(p)
            }))
        }
        ViewRef::Row(id) => {
            let sql = if lock {
                format!("{ROW_SELECT} FOR UPDATE")
            } else {
                ROW_SELECT.to_string()
            };
            let row: Option<ViewRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
            let l = loaded_from_row(row.ok_or_else(not_found)?)?;
            let ok = match l.scope.as_str() {
                "personal" => caller.user_id().is_some() && l.owner_id == caller.user_id(),
                _ => l
                    .project
                    .is_some_and(|p| scope.project_ids().any(|x| x == p)),
            };
            if !ok {
                return Err(not_found());
            }
            Ok(l)
        }
    }
}

fn builtin_loaded(b: Builtin) -> Loaded {
    let def = b.def();
    Loaded {
        id: b.id(),
        scope: "builtin".into(),
        kind: Kind::parse(def["kind"].as_str().unwrap_or("list")).unwrap_or(Kind::List),
        name: b.name().into(),
        project: b.project(),
        def,
        lock_version: None,
        owner_id: None,
        created_at: None,
        updated_at: None,
        updated_by: None,
        builtin: Some(b),
    }
}

/// manage_public_views in an active project (R-VD2).
async fn can_manage(conn: &mut PgConnection, caller: AuthContext, project: Uuid) -> Result<bool> {
    if caller.user_id().is_none() {
        return Ok(false);
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    Ok(perm::project_perms(&mut *conn, &actor, project)
        .await?
        .is_some_and(|pp| pp.status == "active" && pp.allowed_any(MANAGE_PUBLIC_VIEWS)))
}

/// The caller can see the project (its key and name may be shown).
async fn project_visible(conn: &mut PgConnection, caller: AuthContext, id: Uuid) -> Result<bool> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    Ok(scope.project_ids().any(|x| x == id))
}

/// R-VD10: a personal view bound to a project the caller cannot see any
/// more does not run (its name is not shown either).
async fn require_bound_project(
    conn: &mut PgConnection,
    caller: AuthContext,
    l: &Loaded,
) -> Result<()> {
    if let Some(p) = l.project
        && l.scope == "personal"
        && !project_visible(&mut *conn, caller, p).await?
    {
        return Err(invalid_view(
            "project_id",
            "project_unavailable",
            "this view is bound to a project you can no longer see; copy its conditions into a new view",
        ));
    }
    Ok(())
}

async fn project_ref(conn: &mut PgConnection, id: Uuid) -> Result<ProjectRef> {
    let (key, name): (String, String) =
        sqlx::query_as("SELECT key, name FROM projects WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    Ok(ProjectRef { id, key, name })
}

async fn person_ref(conn: &mut PgConnection, id: Option<Uuid>) -> Result<Option<PersonRef>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let r: Option<(String, String)> =
        sqlx::query_as("SELECT name, kind FROM principals WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(r.map(|(name, kind)| PersonRef { id, name, kind }))
}

async fn detail(conn: &mut PgConnection, caller: AuthContext, l: &Loaded) -> Result<ViewDetail> {
    let editable = match l.scope.as_str() {
        "personal" => true,
        "project" => can_manage(&mut *conn, caller, l.project.unwrap_or_default()).await?,
        _ => false,
    };
    let (project, project_unavailable) = match l.project {
        Some(p) if project_visible(&mut *conn, caller, p).await? => {
            (Some(project_ref(&mut *conn, p).await?), false)
        }
        Some(_) => (None, true),
        None => (None, false),
    };
    Ok(ViewDetail {
        id: l.id.clone(),
        scope: l.scope.clone(),
        kind: l.kind.as_str().into(),
        name: l.name.clone(),
        project,
        project_unavailable,
        def: l.def.clone(),
        editable,
        lock_version: l.lock_version,
        created_by: person_ref(&mut *conn, l.owner_id).await?,
        created_at: l.created_at,
        updated_by: person_ref(&mut *conn, l.updated_by).await?,
        updated_at: l.updated_at,
    })
}

/// `GET /views/{id}`.
pub async fn get(app: &App, caller: AuthContext, id: &str) -> Result<ViewDetail> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let l = load(&mut tx, caller, id, false).await?;
    let d = detail(&mut tx, caller, &l).await?;
    tx.commit().await?;
    Ok(d)
}

// ---------------------------------------------------------------- listing

fn page_cursor(offset: usize) -> String {
    crate::auth::encode(json!({"o": offset}).to_string().as_bytes())
}

fn page_offset(c: Option<&str>) -> Result<usize> {
    let Some(c) = c else {
        return Ok(0);
    };
    let bad = || api(ErrorCode::InvalidCursor, "not a view list cursor");
    let b = crate::auth::decode_bytes(c.trim()).ok_or_else(bad)?;
    let v: Json = serde_json::from_slice(&b).map_err(|_| bad())?;
    v["o"]
        .as_u64()
        .map(|o| o as usize)
        .filter(|o| *o <= 100_000)
        .ok_or_else(bad)
}

/// `GET /views?project=&scope=&cursor=&limit=` (people) and MCP `view_list`
/// (agents: built-ins without `me` and the project views of delegated
/// projects; never personal views, B-4, R-VD9).
pub async fn list(
    app: &App,
    caller: AuthContext,
    project: Option<&str>,
    scope_filter: Option<&str>,
    cursor: Option<&str>,
    limit: Option<u32>,
) -> Result<ViewList> {
    let want = scope_filter.map(str::trim).unwrap_or("all");
    if !["all", "builtin", "personal", "project"].contains(&want) {
        return Err(bad_arg(
            "scope",
            "scope is all, builtin, personal or project",
        ));
    }
    let offset = page_offset(cursor)?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let mut tx = read_tx(app).await?;
    let scope = db::load_scope(&mut tx, caller).await?;
    let agent = caller.user_id().is_none();
    let pid = match project {
        None => None,
        Some(p) => Some(
            describe::visible_project(&mut tx, caller, p)
                .await
                .map_err(|_| api(ErrorCode::NotFound, "no such project"))?,
        ),
    };
    let visible: Vec<Uuid> = match pid {
        Some(p) => vec![p],
        None => scope.project_ids().collect(),
    };
    let mut refs: HashMap<Uuid, ProjectRef> = HashMap::new();
    for p in &visible {
        refs.insert(*p, project_ref(&mut tx, *p).await?);
    }
    let mut items: Vec<ViewSummary> = Vec::new();
    if matches!(want, "all" | "builtin") {
        let mut bs: Vec<Builtin> = Vec::new();
        match pid {
            Some(p) => bs.extend([Builtin::ProjectIssues(p), Builtin::ProjectBoard(p)]),
            None if agent => {
                let mut ps: Vec<&ProjectRef> = refs.values().collect();
                ps.sort_by(|a, b| a.key.cmp(&b.key));
                for p in ps {
                    bs.extend([Builtin::ProjectIssues(p.id), Builtin::ProjectBoard(p.id)]);
                }
            }
            None => bs.extend([
                Builtin::AssignedToMe,
                Builtin::ReportedByMe,
                Builtin::InvolvingMe,
            ]),
        }
        for b in bs {
            let kind = b.def()["kind"].as_str().unwrap_or("list").to_string();
            items.push(ViewSummary {
                id: b.id(),
                scope: "builtin".into(),
                kind,
                name: b.name().into(),
                editable: false,
                project: b.project().and_then(|p| refs.get(&p).cloned()),
                project_unavailable: false,
                lock_version: None,
                updated_by: None,
                updated_at: None,
            });
        }
    }
    type Row = (
        Uuid,
        String,
        Option<Uuid>,
        String,
        String,
        i32,
        DateTime<Utc>,
        Option<Uuid>,
    );
    if !agent && matches!(want, "all" | "personal") {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, scope, project_id, kind, name, lock_version, updated_at, updated_by
             FROM view_defs WHERE scope = 'personal' AND owner_id = $1 AND deleted_at IS NULL
               AND ($2::uuid IS NULL OR project_id = $2)
             ORDER BY name_key, id",
        )
        .bind(caller.user_id())
        .bind(pid)
        .fetch_all(&mut *tx)
        .await?;
        for r in rows {
            // `refs` holds every visible project (or the one asked for,
            // which the query keeps to): anything else is not shown.
            let project = r.2.and_then(|p| refs.get(&p).cloned());
            let project_unavailable = r.2.is_some() && project.is_none();
            items.push(ViewSummary {
                id: r.0.to_string(),
                scope: r.1,
                kind: r.3,
                name: r.4,
                editable: true,
                project,
                project_unavailable,
                lock_version: Some(r.5),
                updated_by: person_ref(&mut tx, r.7).await?,
                updated_at: Some(r.6),
            });
        }
    }
    if matches!(want, "all" | "project") {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT v.id, v.scope, v.project_id, v.kind, v.name, v.lock_version, v.updated_at, v.updated_by
             FROM view_defs v JOIN projects p ON p.id = v.project_id
             WHERE v.scope = 'project' AND v.deleted_at IS NULL AND v.project_id = ANY($1)
             ORDER BY p.key, v.name_key, v.id",
        )
        .bind(&visible)
        .fetch_all(&mut *tx)
        .await?;
        let mut manage: HashMap<Uuid, bool> = HashMap::new();
        for r in rows {
            let p = r.2.unwrap_or_default();
            let editable = match manage.get(&p) {
                Some(b) => *b,
                None => {
                    let b = can_manage(&mut tx, caller, p).await?;
                    manage.insert(p, b);
                    b
                }
            };
            items.push(ViewSummary {
                id: r.0.to_string(),
                scope: r.1,
                kind: r.3,
                name: r.4,
                editable,
                project: refs.get(&p).cloned(),
                project_unavailable: false,
                lock_version: Some(r.5),
                updated_by: person_ref(&mut tx, r.7).await?,
                updated_at: Some(r.6),
            });
        }
    }
    tx.commit().await?;
    let more = items.len() > offset + limit;
    let items: Vec<ViewSummary> = items.into_iter().skip(offset).take(limit).collect();
    Ok(ViewList {
        items,
        next_cursor: more.then(|| page_cursor(offset + limit)),
    })
}

// ---------------------------------------------------------------- writes

fn write_hash(v: &Json) -> [u8; 32] {
    Sha256::digest([b"oxledger/view/v1\n".as_slice(), v.to_string().as_bytes()].concat()).into()
}

async fn name_and_key(conn: &mut PgConnection, name: &str, path: &str) -> Result<(String, String)> {
    let (n, k): (String, String) =
        sqlx::query_as("SELECT normalize($1, NFC), casefold(normalize($1, NFC))")
            .bind(name.trim())
            .fetch_one(&mut *conn)
            .await?;
    let chars = n.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS || n.chars().any(char::is_control) {
        return Err(invalid_value(
            path,
            "view_name_invalid",
            format!("a view name is 1 to {MAX_NAME_CHARS} characters, no control characters"),
        ));
    }
    Ok((n, k))
}

async fn taken(
    conn: &mut PgConnection,
    scope: &str,
    owner: Uuid,
    project: Option<Uuid>,
    key: &str,
    except: Option<Uuid>,
) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM view_defs WHERE deleted_at IS NULL AND name_key = $4 AND scope = $1
           AND (($1 = 'personal' AND owner_id = $2) OR ($1 = 'project' AND project_id = $3))
           AND id IS DISTINCT FROM $5",
    )
    .bind(scope)
    .bind(owner)
    .bind(project)
    .bind(key)
    .bind(except)
    .fetch_optional(&mut *conn)
    .await?)
}

fn name_taken(id: Uuid) -> anyhow::Error {
    ApiError::new(
        ErrorCode::ViewNameTaken,
        "another view of the same scope has that name",
    )
    .with_details(json!({"view_id": id}))
    .into()
}

async fn begin(app: &App) -> Result<sqlx::Transaction<'static, Postgres>> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

fn busy(e: anyhow::Error) -> anyhow::Error {
    if commands::is_lock_timeout(&e) {
        return api(
            ErrorCode::TemporarilyUnavailable,
            "the view is busy; send the same request again",
        );
    }
    e
}

/// A replay of a used Idempotency-Key: the view as it is now.
async fn replay(conn: &mut PgConnection, caller: AuthContext, r: &Json) -> Result<ViewDetail> {
    let id = r["view_id"].as_str().ok_or_else(not_found)?;
    let l = load(&mut *conn, caller, id, false).await?;
    detail(&mut *conn, caller, &l).await
}

/// The project a write names (visible, else `not_found`).
async fn target_project(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Option<&str>,
) -> Result<Option<Uuid>> {
    match project {
        None => Ok(None),
        Some(p) => Ok(Some(
            describe::visible_project(&mut *conn, caller, p)
                .await
                .map_err(|_| api(ErrorCode::NotFound, "no such project"))?,
        )),
    }
}

/// Insert after every check (creation and copy).
#[allow(clippy::too_many_arguments)]
async fn insert(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: Uuid,
    request_id: Uuid,
    scope: &str,
    project: Option<Uuid>,
    kind: Kind,
    name: &str,
    def_input: &Json,
    copied_from: Option<&str>,
) -> Result<Uuid> {
    if !["personal", "project"].contains(&scope) {
        return Err(invalid_value(
            "scope",
            "invalid_scope",
            "scope is personal or project",
        ));
    }
    let public = scope == "project";
    if public {
        let Some(p) = project else {
            return Err(invalid_value(
                "project_id",
                "project_required",
                "a project view needs project_id",
            ));
        };
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(p)
            .execute(&mut *conn)
            .await?;
        if !can_manage(&mut *conn, caller, p).await? {
            return Err(forbidden(
                "manage_public_views",
                "project views need manage_public_views in an active project",
            ));
        }
    }
    let (name, key) = name_and_key(&mut *conn, name, "name").await?;
    // R-V4, counted under a per-owner / per-project lock.
    let (lock_key, max) = if public {
        (
            format!("views:project:{}", project.unwrap_or_default()),
            MAX_PROJECT,
        )
    } else {
        (format!("views:personal:{who}"), MAX_PERSONAL)
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&lock_key)
        .execute(&mut *conn)
        .await?;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM view_defs WHERE deleted_at IS NULL AND scope = $1
           AND (($1 = 'personal' AND owner_id = $2) OR ($1 = 'project' AND project_id = $3))",
    )
    .bind(scope)
    .bind(who)
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    if n >= max {
        return Err(invalid_value(
            "scope",
            "view_limit",
            if public {
                format!("a project has at most {MAX_PROJECT} project views")
            } else {
                format!("you have at most {MAX_PERSONAL} personal views")
            },
        ));
    }
    let def = normalize(
        &mut *conn,
        caller,
        def_input,
        Some(kind),
        SaveCtx { project, public },
    )
    .await?;
    if let Some(id) = taken(&mut *conn, scope, who, project, &key, None).await? {
        return Err(name_taken(id));
    }
    let def_json = def.to_json();
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO view_defs (scope, owner_id, project_id, kind, name, name_key, def, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $2) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(scope)
    .bind(who)
    .bind(project)
    .bind(kind.as_str())
    .bind(&name)
    .bind(&key)
    .bind(&def_json)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = id else {
        let other = taken(&mut *conn, scope, who, project, &key, None)
            .await?
            .unwrap_or_default();
        return Err(name_taken(other));
    };
    let mut payload = json!({"before": null, "after": {"name": name, "def": def_json}});
    if let Some(src) = copied_from {
        payload["copied_from"] = json!(src);
    }
    event(&mut *conn, id, "created", who, request_id, &payload).await?;
    commands::complete_request(&mut *conn, request_id, &json!({"view_id": id})).await?;
    Ok(id)
}

async fn event(
    conn: &mut PgConnection,
    view: Uuid,
    kind: &str,
    actor: Uuid,
    request_id: Uuid,
    payload: &Json,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO view_def_events (view_id, kind, actor_id, operation_request_id, payload)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(view)
    .bind(kind)
    .bind(actor)
    .bind(request_id)
    .bind(payload)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `POST /views` (Idempotency-Key).
pub async fn create(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &NewView,
) -> Result<ViewDetail> {
    let who = commands::executor(&caller)?;
    let mut tx = begin(app).await?;
    let r = async {
        let kind = Kind::parse(input.kind.trim())
            .ok_or_else(|| invalid_value("kind", "invalid_kind", "kind is list or board"))?;
        let h = write_hash(&json!({
            "scope": input.scope.trim(), "project_id": input.project_id.as_deref().map(str::trim),
            "kind": kind.as_str(), "name": input.name.trim(), "def": input.def,
        }));
        let fp = commands::fingerprint(ACTION_CREATE, &h);
        let Some(request_id) =
            commands::open_request(&mut tx, who.user_id, operation_id, ACTION_CREATE, &fp).await?
        else {
            let r = commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_CREATE, &fp)
                .await?;
            return replay(&mut tx, caller, &r).await;
        };
        commands::lock_executor(&mut tx, &who).await?;
        let project = target_project(&mut tx, caller, input.project_id.as_deref()).await?;
        let id = insert(
            &mut tx,
            caller,
            who.user_id,
            request_id,
            input.scope.trim(),
            project,
            kind,
            &input.name,
            &input.def,
            None,
        )
        .await?;
        let l = load(&mut tx, caller, &id.to_string(), false).await?;
        detail(&mut tx, caller, &l).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// `POST /views/{id}/copy` (Idempotency-Key): a new, independent view (no
/// deep merge, R-VD5).
pub async fn copy(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: &str,
    input: &ViewCopy,
) -> Result<ViewDetail> {
    let who = commands::executor(&caller)?;
    let mut tx = begin(app).await?;
    let r = async {
        let scope = input
            .scope
            .as_deref()
            .map(str::trim)
            .unwrap_or("personal")
            .to_string();
        // The request alone: a replay returns the copy it made even after
        // the source changed or went away.
        let h = write_hash(&json!({
            "source": id.trim(), "scope": scope,
            "project_id": input.project_id.as_deref().map(str::trim), "name": input.name.as_deref().map(str::trim),
        }));
        let fp = commands::fingerprint(ACTION_COPY, &h);
        let Some(request_id) =
            commands::open_request(&mut tx, who.user_id, operation_id, ACTION_COPY, &fp).await?
        else {
            let r =
                commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_COPY, &fp).await?;
            return replay(&mut tx, caller, &r).await;
        };
        commands::lock_executor(&mut tx, &who).await?;
        let src = load(&mut tx, caller, id, false).await?;
        if src.builtin == Some(Builtin::InvolvingMe) {
            return Err(forbidden(
                "not_copyable",
                "「與我相關」 includes issues you wrote to, which a saved condition cannot say; build your own view instead",
            ));
        }
        let project = match input.project_id.as_deref() {
            Some(_) => target_project(&mut tx, caller, input.project_id.as_deref()).await?,
            None => src.project,
        };
        let name = match &input.name {
            Some(n) => n.clone(),
            None => {
                let base: String = src.name.chars().take(MAX_NAME_CHARS - 4).collect();
                format!("{base}（副本）")
            }
        };
        let mut def = src.def.clone();
        // A copy into another project: the source's own project condition
        // would fail R-VD3; the server adds the new project's.
        if project != src.project
            && let Some(p) = src.project
        {
            strip_project(&mut def, p);
        }
        let new_id = insert(
            &mut tx,
            caller,
            who.user_id,
            request_id,
            &scope,
            project,
            src.kind,
            &name,
            &def,
            Some(&src.id),
        )
        .await?;
        let l = load(&mut tx, caller, &new_id.to_string(), false).await?;
        detail(&mut tx, caller, &l).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// Removes top-level `project eq p` conditions (a copy to another project).
fn strip_project(def: &mut Json, p: Uuid) {
    let pc = json!({"field": "project", "op": "eq", "value": p.to_string()});
    let w = &mut def["query"]["where"];
    if *w == pc {
        *w = Json::Null;
    } else if let Some(parts) = w.get_mut("and").and_then(Json::as_array_mut) {
        parts.retain(|c| *c != pc);
        if parts.is_empty() {
            *w = Json::Null;
        }
    }
}

/// Locks for a change of a saved view: project `FOR SHARE` (active check),
/// the project's tab settings `FOR SHARE` (R-V2; tab writes take them
/// `FOR UPDATE` first, so the order is the same), then the view `FOR UPDATE`.
/// R-VD5: built-ins never change.
async fn lock_for_write(conn: &mut PgConnection, caller: AuthContext, id: &str) -> Result<Loaded> {
    let peek = load(&mut *conn, caller, id, false).await?;
    if peek.builtin.is_some() {
        return Err(forbidden(
            "builtin_view",
            "a built-in view cannot be changed; copy it first",
        ));
    }
    if let Some(p) = peek.project
        && peek.scope == "project"
    {
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
            .bind(p)
            .execute(&mut *conn)
            .await?;
        sqlx::query("SELECT 1 FROM project_tab_settings WHERE project_id = $1 FOR SHARE")
            .bind(p)
            .execute(&mut *conn)
            .await?;
    }
    let l = load(&mut *conn, caller, id, true).await?;
    if l.scope == "project"
        && !can_manage(&mut *conn, caller, l.project.unwrap_or_default()).await?
    {
        return Err(forbidden(
            "manage_public_views",
            "changing a project view needs manage_public_views in an active project",
        ));
    }
    Ok(l)
}

async fn conflict(
    conn: &mut PgConnection,
    caller: AuthContext,
    l: &Loaded,
) -> Result<anyhow::Error> {
    let current = detail(&mut *conn, caller, l).await?;
    Ok(ApiError::new(
        ErrorCode::Conflict,
        "the view changed; here is the current one",
    )
    .with_details(json!({"current_lock_version": l.lock_version, "current": current}))
    .into())
}

/// `PATCH /views/{id}` (Idempotency-Key).
pub async fn update(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: &str,
    patch: &ViewPatch,
) -> Result<ViewDetail> {
    let who = commands::executor(&caller)?;
    let mut tx = begin(app).await?;
    let r = async {
        let h = write_hash(&json!({
            "id": id.trim(), "lock_version": patch.lock_version,
            "name": patch.name.as_deref().map(str::trim), "def": patch.def,
        }));
        let fp = commands::fingerprint(ACTION_UPDATE, &h);
        let Some(request_id) =
            commands::open_request(&mut tx, who.user_id, operation_id, ACTION_UPDATE, &fp).await?
        else {
            let r = commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_UPDATE, &fp)
                .await?;
            return replay(&mut tx, caller, &r).await;
        };
        commands::lock_executor(&mut tx, &who).await?;
        let l = lock_for_write(&mut tx, caller, id).await?;
        if l.lock_version != Some(patch.lock_version) {
            return Err(conflict(&mut tx, caller, &l).await?);
        }
        let view_id = Uuid::parse_str(&l.id)?;
        let (name, key) = match &patch.name {
            Some(n) => {
                let (n, k) = name_and_key(&mut tx, n, "name").await?;
                (n, Some(k))
            }
            None => (l.name.clone(), None),
        };
        let def = match &patch.def {
            Some(d) => normalize(
                &mut tx,
                caller,
                d,
                Some(l.kind),
                SaveCtx {
                    project: l.project,
                    public: l.scope == "project",
                },
            )
            .await?
            .to_json(),
            None => l.def.clone(),
        };
        if let Some(k) = &key
            && let Some(other) = taken(
                &mut tx,
                &l.scope,
                l.owner_id.unwrap_or_default(),
                l.project,
                k,
                Some(view_id),
            )
            .await?
        {
            return Err(name_taken(other));
        }
        if name != l.name || def != l.def {
            // A view committed with the same name since the check above:
            // the unique index says so; a savepoint keeps the transaction.
            sqlx::query("SAVEPOINT view_rename").execute(&mut *tx).await?;
            let up = sqlx::query(
                "UPDATE view_defs SET name = $2, name_key = COALESCE($3, name_key), def = $4,
                        lock_version = lock_version + 1, updated_at = now(), updated_by = $5
                 WHERE id = $1",
            )
            .bind(view_id)
            .bind(&name)
            .bind(&key)
            .bind(&def)
            .bind(who.user_id)
            .execute(&mut *tx)
            .await;
            match up {
                Ok(_) => {
                    sqlx::query("RELEASE SAVEPOINT view_rename")
                        .execute(&mut *tx)
                        .await?;
                }
                Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505") => {
                    sqlx::query("ROLLBACK TO SAVEPOINT view_rename")
                        .execute(&mut *tx)
                        .await?;
                    let other = match &key {
                        Some(k) => taken(
                            &mut tx,
                            &l.scope,
                            l.owner_id.unwrap_or_default(),
                            l.project,
                            k,
                            Some(view_id),
                        )
                        .await?
                        .unwrap_or_default(),
                        None => Uuid::nil(),
                    };
                    return Err(name_taken(other));
                }
                Err(e) => return Err(e.into()),
            }
            event(
                &mut tx,
                view_id,
                "updated",
                who.user_id,
                request_id,
                &json!({"before": {"name": l.name, "def": l.def}, "after": {"name": name, "def": def}}),
            )
            .await?;
        }
        commands::complete_request(&mut tx, request_id, &json!({"view_id": view_id})).await?;
        let l = load(&mut tx, caller, &view_id.to_string(), false).await?;
        detail(&mut tx, caller, &l).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// `DELETE /views/{id}?lock_version=` (Idempotency-Key): soft delete; a
/// view a project tab shows is `view_in_use` (R-V2).
pub async fn delete(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: &str,
    lock_version: i32,
) -> Result<ViewDeleted> {
    let who = commands::executor(&caller)?;
    let mut tx = begin(app).await?;
    let r = async {
        let h = write_hash(&json!({"id": id.trim(), "lock_version": lock_version, "delete": true}));
        let fp = commands::fingerprint(ACTION_DELETE, &h);
        let Some(request_id) =
            commands::open_request(&mut tx, who.user_id, operation_id, ACTION_DELETE, &fp).await?
        else {
            let r = commands::stored_result(&mut tx, who.user_id, operation_id, ACTION_DELETE, &fp)
                .await?;
            return Ok(ViewDeleted {
                id: r["view_id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(not_found)?,
                lock_version: r["lock_version"].as_i64().unwrap_or(0) as i32,
            });
        };
        commands::lock_executor(&mut tx, &who).await?;
        let l = lock_for_write(&mut tx, caller, id).await?;
        if l.lock_version != Some(lock_version) {
            return Err(conflict(&mut tx, caller, &l).await?);
        }
        let view_id = Uuid::parse_str(&l.id)?;
        if let Some(p) = l.project
            && l.scope == "project"
        {
            let tabs: Json =
                sqlx::query_scalar("SELECT tabs FROM project_tab_settings WHERE project_id = $1")
                    .bind(p)
                    .fetch_one(&mut *tx)
                    .await?;
            let mine = json!(view_id.to_string());
            let titles: Vec<Json> = tabs
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| t["view_id"] == mine)
                .map(|t| t["title"].clone())
                .collect();
            if !titles.is_empty() {
                return Err(ApiError::new(
                    ErrorCode::ViewInUse,
                    "a project tab shows this view; remove the tab first",
                )
                .with_details(json!({"tabs": titles}))
                .into());
            }
        }
        let lv: i32 = sqlx::query_scalar(
            "UPDATE view_defs SET deleted_at = now(), lock_version = lock_version + 1,
                    updated_at = now(), updated_by = $2
             WHERE id = $1 RETURNING lock_version",
        )
        .bind(view_id)
        .bind(who.user_id)
        .fetch_one(&mut *tx)
        .await?;
        event(
            &mut tx,
            view_id,
            "deleted",
            who.user_id,
            request_id,
            &json!({"before": {"name": l.name, "def": l.def}, "after": null}),
        )
        .await?;
        commands::complete_request(
            &mut tx,
            request_id,
            &json!({"view_id": view_id, "lock_version": lv}),
        )
        .await?;
        Ok(ViewDeleted {
            id: view_id,
            lock_version: lv,
        })
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// `GET /views/{id}/events?cursor=`: newest first; read access as the view
/// (R-VD10); a redacted event shows only the marker.
pub async fn events(
    app: &App,
    caller: AuthContext,
    id: &str,
    cursor: Option<&str>,
) -> Result<ViewEvents> {
    caller.require_human_session()?;
    let after: Option<(DateTime<Utc>, Uuid)> = cursor
        .map(|c| {
            let bad = || api(ErrorCode::InvalidCursor, "not a view events cursor");
            let b = crate::auth::decode_bytes(c.trim()).ok_or_else(bad)?;
            let v: Json = serde_json::from_slice(&b).map_err(|_| bad())?;
            let at = v["at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .ok_or_else(bad)?
                .with_timezone(&Utc);
            let id = v["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(bad)?;
            Ok::<_, anyhow::Error>((at, id))
        })
        .transpose()?;
    let mut tx = read_tx(app).await?;
    let l = load(&mut tx, caller, id, false).await?;
    let Ok(view_id) = Uuid::parse_str(&l.id) else {
        tx.commit().await?;
        return Ok(ViewEvents {
            items: Vec::new(),
            next_cursor: None,
        });
    };
    type Row = (
        Uuid,
        DateTime<Utc>,
        String,
        Uuid,
        String,
        String,
        Json,
        bool,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT e.id, e.at, e.kind, p.id, p.name, p.kind, e.payload, e.redaction_id IS NOT NULL
         FROM view_def_events e JOIN principals p ON p.id = e.actor_id
         WHERE e.view_id = $1 AND ($2::timestamptz IS NULL OR (e.at, e.id) < ($2, $3))
         ORDER BY e.at DESC, e.id DESC LIMIT $4",
    )
    .bind(view_id)
    .bind(after.map(|x| x.0))
    .bind(after.map(|x| x.1))
    .bind(EVENTS_PAGE + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let more = rows.len() as i64 > EVENTS_PAGE;
    let items: Vec<ViewEvent> = rows
        .into_iter()
        .take(EVENTS_PAGE as usize)
        .map(
            |(id, at, kind, aid, name, akind, payload, redacted)| ViewEvent {
                id,
                at,
                kind,
                actor: PersonRef {
                    id: aid,
                    name,
                    kind: akind,
                },
                payload: if redacted {
                    json!("[已遮蔽]")
                } else {
                    payload
                },
                redacted,
            },
        )
        .collect();
    let next_cursor = more.then(|| {
        let last = items.last().unwrap();
        crate::auth::encode(
            json!({"at": last.at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true), "id": last.id})
                .to_string()
                .as_bytes(),
        )
    });
    Ok(ViewEvents { items, next_cursor })
}

// ---------------------------------------------------------------- counting

/// Tests: the count budget (ms; 0 = [`COUNT_BUDGET`]) and a delay put into
/// every count statement (T-M1-14).
#[cfg(feature = "test-support")]
pub static COUNT_BUDGET_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "test-support")]
pub static COUNT_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One part of a count's WHERE clause.
enum Clause<'a> {
    /// The filter holds.
    F(&'a Filter),
    /// A custom field does not apply to the issue (or the caller cannot
    /// read it there): its gate is not true.
    NotApplies(&'a Pred),
    /// The issue is in none of these (a board's lanes: unmapped).
    NoneOf(Vec<&'a Filter>),
}

/// R-VD6: one 2 s budget for the counts and totals of a run. It starts with
/// the first count, after the run's own queries (those have the search's
/// budget), so the items never wait on counting.
struct Counter {
    end: std::sync::OnceLock<std::time::Instant>,
}

impl Counter {
    fn start() -> Counter {
        Counter {
            end: std::sync::OnceLock::new(),
        }
    }

    fn total() -> std::time::Duration {
        #[cfg(feature = "test-support")]
        let total = match COUNT_BUDGET_MS.load(std::sync::atomic::Ordering::SeqCst) {
            0 => COUNT_BUDGET,
            ms => std::time::Duration::from_millis(ms),
        };
        #[cfg(not(feature = "test-support"))]
        let total = COUNT_BUDGET;
        total
    }

    /// One statement under what is left of the budget, in a savepoint so the
    /// read transaction survives a cancel. `None`: out of time.
    async fn rows(
        &self,
        conn: &mut PgConnection,
        mut qb: QueryBuilder<Postgres>,
    ) -> Result<Option<Vec<PgRow>>> {
        let end = *self
            .end
            .get_or_init(|| std::time::Instant::now() + Self::total());
        let left = end
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64;
        if left == 0 {
            return Ok(None);
        }
        let before: String = sqlx::query_scalar("SELECT current_setting('statement_timeout')")
            .fetch_one(&mut *conn)
            .await?;
        sqlx::query("SAVEPOINT ox_count")
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
                let d = COUNT_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst);
                if d > 0 {
                    sqlx::query("SELECT pg_sleep($1)")
                        .bind(d as f64 / 1000.0)
                        .execute(&mut *conn)
                        .await?;
                }
            }
            qb.build().fetch_all(&mut *conn).await
        }
        .await;
        match r {
            Ok(rows) => {
                sqlx::query("RELEASE SAVEPOINT ox_count")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SELECT set_config('statement_timeout', $1, true)")
                    .bind(&before)
                    .execute(&mut *conn)
                    .await?;
                Ok(Some(rows))
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
                sqlx::query("ROLLBACK TO SAVEPOINT ox_count")
                    .execute(&mut *conn)
                    .await?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// ` FROM issues i WHERE <visible> AND <clauses>`.
    fn from_where(qb: &mut QueryBuilder<Postgres>, visible: &Pred, clauses: &[Clause]) {
        qb.push(" FROM issues i WHERE i.archived_at IS NULL AND ");
        push_pred(qb, "i", visible);
        for c in clauses {
            qb.push(" AND ");
            match c {
                Clause::F(f) => filter::push_filter(qb, "i", f),
                Clause::NotApplies(gate) => {
                    qb.push("(");
                    push_pred(qb, "i", gate);
                    qb.push(") IS NOT TRUE");
                }
                Clause::NoneOf(fs) if fs.is_empty() => {
                    qb.push("TRUE");
                }
                Clause::NoneOf(fs) => {
                    qb.push("(");
                    for (n, f) in fs.iter().enumerate() {
                        if n > 0 {
                            qb.push(" OR ");
                        }
                        qb.push("(");
                        filter::push_filter(qb, "i", f);
                        qb.push(")");
                    }
                    qb.push(") IS NOT TRUE");
                }
            }
        }
    }

    /// Visible issues matching every clause, up to [`COUNT_CAP`] (+1 to know).
    async fn count(
        &self,
        conn: &mut PgConnection,
        visible: &Pred,
        clauses: &[Clause<'_>],
    ) -> Result<Option<Count>> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT count(*) FROM (SELECT 1");
        Self::from_where(&mut qb, visible, clauses);
        qb.push(format!(" LIMIT {}) c", COUNT_CAP + 1));
        let Some(rows) = self.rows(&mut *conn, qb).await? else {
            return Ok(None);
        };
        let n: i64 = rows.first().map(|r| r.try_get(0)).transpose()?.unwrap_or(0);
        Ok(Some(Count {
            value: n.min(COUNT_CAP),
            capped: n > COUNT_CAP,
        }))
    }

    /// R-VD7: the hours of the time entries `me` can see (§3.5.4) on the
    /// visible matching issues (two decimals).
    async fn sum_spent(
        &self,
        conn: &mut PgConnection,
        me: Option<Uuid>,
        visible: &Pred,
        clauses: &[Clause<'_>],
    ) -> Result<Option<String>> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT (COALESCE(sum(te.hours), 0)::numeric(12, 2))::text FROM time_entries te
             WHERE te.deleted_at IS NULL AND (CASE time_visibility(te.project_id, ",
        );
        qb.push_bind(me)
            .push(") WHEN 'all' THEN true WHEN 'own' THEN te.user_id = ")
            .push_bind(me)
            .push(" ELSE false END) AND te.issue_id IN (SELECT i.id");
        Self::from_where(&mut qb, visible, clauses);
        qb.push(")");
        let Some(rows) = self.rows(&mut *conn, qb).await? else {
            return Ok(None);
        };
        Ok(rows.first().map(|r| r.try_get(0)).transpose()?)
    }

    /// The sum of `estimated_hours` (two decimals).
    async fn sum_estimated(
        &self,
        conn: &mut PgConnection,
        visible: &Pred,
        clauses: &[Clause<'_>],
    ) -> Result<Option<String>> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT (COALESCE(sum(i.estimated_hours), 0)::numeric(12, 2))::text");
        Self::from_where(&mut qb, visible, clauses);
        let Some(rows) = self.rows(&mut *conn, qb).await? else {
            return Ok(None);
        };
        Ok(rows.first().map(|r| r.try_get(0)).transpose()?)
    }
}

// ---------------------------------------------------------------- running

/// What a run needs of the view.
struct Prepared {
    head: ViewHead,
    def: Def,
    project: Option<Uuid>,
    involving: bool,
    def_hash: String,
}

fn prepared(l: &Loaded) -> Result<Prepared> {
    let def = parse_shape(&l.def, Some(l.kind))?;
    Ok(Prepared {
        head: ViewHead {
            id: l.id.clone(),
            kind: l.kind.as_str().into(),
            name: l.name.clone(),
        },
        def_hash: def.hash(),
        def,
        project: l.project,
        involving: l.builtin == Some(Builtin::InvolvingMe),
    })
}

/// A grouping key (list `group_by`, board `swimlanes.by`, board lanes).
#[derive(Debug, Clone)]
enum GroupKey {
    Builtin(String),
    Cf { id: i32, ty: FieldType, gate: Pred },
}

impl GroupKey {
    /// R-VD4 at run time: a custom field the opener cannot read stops the run.
    fn resolve(fields: &Fields, key: &str, path: &str) -> Result<GroupKey> {
        if !key.starts_with("cf:") {
            return Ok(GroupKey::Builtin(key.to_string()));
        }
        let (row, gate) = find_cf(fields, key).ok_or_else(|| field_unavailable(path))?;
        Ok(GroupKey::Cf {
            id: row.id,
            ty: row.field_type()?,
            gate: gate.clone(),
        })
    }

    fn field(&self) -> String {
        match self {
            GroupKey::Builtin(b) => b.clone(),
            GroupKey::Cf { id, .. } => format!("cf:{id}"),
        }
    }

    /// ORDER BY columns keeping each value's items together (ids and
    /// positions only, so cursors stay within `cursor::MAX_SEALED`).
    fn lead(&self) -> Vec<v2::SortCol> {
        use v2::SortCol;
        match self {
            GroupKey::Builtin(b) => match b.as_str() {
                "status" => vec![
                    SortCol::lead("s.position", None, "integer", false),
                    SortCol::lead("i.status_id", None, "uuid", false),
                ],
                "status_category" => vec![SortCol::lead(
                    "(CASE s.category WHEN 'todo' THEN 1 WHEN 'doing' THEN 2 ELSE 3 END)",
                    None,
                    "integer",
                    false,
                )],
                "assignee" => vec![SortCol::lead("i.assigned_to_id", None, "uuid", true)],
                "priority" => vec![
                    SortCol::lead("pr.position", None, "integer", true),
                    SortCol::lead("i.priority_id", None, "uuid", true),
                ],
                "tracker" => vec![
                    SortCol::lead("t.position", None, "integer", false),
                    SortCol::lead("i.tracker_id", None, "uuid", false),
                ],
                "fixed_version" => vec![
                    SortCol::lead(
                        "(SELECT v.effective_date FROM versions v WHERE v.id = i.fixed_version_id)",
                        None,
                        "date",
                        true,
                    ),
                    SortCol::lead("i.fixed_version_id", None, "uuid", true),
                ],
                _ => vec![SortCol::lead("p.key", None, "text", false)],
            },
            GroupKey::Cf { id, ty, gate, .. } => {
                let sub = |col: &str| {
                    format!(
                        "(SELECT cf.{col} FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {id})"
                    )
                };
                let g = Some(gate.clone());
                // Where the field applies first (0), then where it does not
                // (null, last): 未設定 and 不適用 stay apart.
                let mut cols = vec![SortCol::lead("0", g.clone(), "integer", true)];
                cols.extend(match ty {
                    FieldType::List => vec![
                        SortCol::lead(
                            format!(
                                "(SELECT fo.position FROM issue_cf cf JOIN field_options fo ON fo.id = cf.v_ref \
                                 WHERE cf.issue_id = i.id AND cf.field_id = {id})"
                            ),
                            g.clone(),
                            "integer",
                            true,
                        ),
                        SortCol::lead(sub("v_ref"), g, "uuid", true),
                    ],
                    FieldType::Bool => vec![SortCol::lead(sub("v_bool"), g, "boolean", true)],
                    _ => vec![SortCol::lead(sub("v_ref"), g, "uuid", true)],
                });
                cols
            }
        }
    }

    /// A `where` condition for one value (null: not set).
    fn cond(&self, values: &[Json]) -> Json {
        let field = self.field();
        let typed = |v: &Json| match self {
            GroupKey::Cf { ty, .. } if !v.is_object() => {
                let t = match ty {
                    FieldType::List => "list",
                    FieldType::User => "user",
                    _ => "bool",
                };
                json!({t: v})
            }
            _ => v.clone(),
        };
        let concrete: Vec<Json> = values.iter().filter(|v| !v.is_null()).map(typed).collect();
        let null = values.iter().any(Json::is_null);
        let some = (!concrete.is_empty()).then(|| {
            if field == "status_category" {
                json!({"field": field, "op": "in", "value": concrete})
            } else if let [one] = concrete.as_slice() {
                json!({"field": field, "op": "eq", "value": one})
            } else {
                json!({"field": field, "op": "in", "value": concrete})
            }
        });
        let none = null.then(|| json!({"field": field, "op": "is_null"}));
        match (some, none) {
            (Some(a), Some(b)) => json!({"or": [a, b]}),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => json!({"field": field, "op": "is_null"}),
        }
    }

    /// The item's value and label.
    fn value_of(&self, r: &Display, cf: &HashMap<String, Json>) -> (Json, String) {
        let opt = |id: Option<Uuid>, name: &Option<String>, none: &str| match id {
            Some(id) => (json!(id), name.clone().unwrap_or_default()),
            None => (Json::Null, none.to_string()),
        };
        match self {
            GroupKey::Builtin(b) => match b.as_str() {
                "status" => (json!(r.status_id), r.status_name.clone()),
                "status_category" => (json!(r.status_category), category_label(&r.status_category)),
                "assignee" => opt(r.assigned_to_id, &r.assignee_name, "未指派"),
                "priority" => opt(r.priority_id, &r.priority_name, "未設定"),
                "tracker" => (json!(r.tracker_id), r.tracker_name.clone()),
                "fixed_version" => opt(r.fixed_version_id, &r.version_name, "未設定"),
                _ => (json!(r.project_id), r.project_name.clone()),
            },
            GroupKey::Cf { id, ty, .. } => match cf.get(&format!("cf:{id}")) {
                // The field does not apply to the issue (tracker, project,
                // or not readable there): the same as its gate.
                None => (json!(NOT_APPLICABLE), "不適用".into()),
                Some(Json::Null) => (Json::Null, "未設定".into()),
                Some(v) => match ty {
                    FieldType::List => (
                        v["id"].clone(),
                        v["label"].as_str().unwrap_or("").to_string(),
                    ),
                    FieldType::User => (
                        v["id"].clone(),
                        v["name"].as_str().unwrap_or("").to_string(),
                    ),
                    _ => (
                        v.clone(),
                        if v == &json!(true) { "是" } else { "否" }.to_string(),
                    ),
                },
            },
        }
    }
}

/// The group value of issues a custom field does not apply to.
pub const NOT_APPLICABLE: &str = "not_applicable";

fn category_label(c: &str) -> String {
    match c {
        "todo" => "待處理",
        "doing" => "進行中",
        _ => "完成",
    }
    .to_string()
}

/// One issue's display values (fetched by id after the page is known).
#[derive(sqlx::FromRow, Debug, Clone)]
struct Display {
    id: Uuid,
    subject: String,
    project_id: Uuid,
    project_key: String,
    project_name: String,
    tracker_id: Uuid,
    tracker_name: String,
    status_id: Uuid,
    status_name: String,
    status_category: String,
    assigned_to_id: Option<Uuid>,
    assignee_name: Option<String>,
    assignee_kind: Option<String>,
    author_id: Uuid,
    author_name: String,
    author_kind: String,
    priority_id: Option<Uuid>,
    priority_name: Option<String>,
    fixed_version_id: Option<Uuid>,
    version_name: Option<String>,
    is_private: bool,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    estimated_hours: Option<String>,
    /// R-VD7: hundredths of the hours of the time entries the opener can see
    /// on the issue; `None` when the opener cannot read time in its project.
    spent_cents: Option<i64>,
    /// Contract 05 §3.4: the triage state (`snoozed` while snoozed), or
    /// `None` when the issue never entered the inbox.
    triage: Option<String>,
}

async fn displays(
    conn: &mut PgConnection,
    me: Option<Uuid>,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Display>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Display> = sqlx::query_as(
        "SELECT i.id, i.subject, i.project_id, p.key AS project_key, p.name AS project_name,
                i.tracker_id, t.name AS tracker_name, i.status_id, s.name AS status_name,
                s.category AS status_category,
                i.assigned_to_id, a.name AS assignee_name, a.kind AS assignee_kind,
                i.author_id, au.name AS author_name, au.kind AS author_kind,
                i.priority_id, pr.name AS priority_name,
                i.fixed_version_id, v.name AS version_name,
                i.is_private, i.start_date, i.due_date, i.created_at, i.updated_at,
                trim_scale(i.estimated_hours)::text AS estimated_hours,
                CASE WHEN vis.v IS NOT NULL THEN
                    (SELECT (COALESCE(sum(te.hours), 0) * 100)::bigint FROM time_entries te
                     WHERE te.issue_id = i.id AND te.deleted_at IS NULL
                       AND (vis.v = 'all' OR te.user_id = $2)) END AS spent_cents,
                (SELECT CASE WHEN tr.state = 'pending' AND tr.snoozed_until > now()
                                  AND i.updated_at <= tr.snoozed_at
                             THEN 'snoozed' ELSE tr.state END
                 FROM issue_triage tr WHERE tr.issue_id = i.id) AS triage
         FROM issues i
         LEFT JOIN LATERAL (SELECT time_visibility(i.project_id, $2) AS v) vis ON true
         JOIN projects p ON p.id = i.project_id
         JOIN trackers t ON t.id = i.tracker_id
         JOIN issue_statuses s ON s.id = i.status_id
         LEFT JOIN principals a ON a.id = i.assigned_to_id
         JOIN principals au ON au.id = i.author_id
         LEFT JOIN issue_priorities pr ON pr.id = i.priority_id
         LEFT JOIN versions v ON v.id = i.fixed_version_id
         WHERE i.id = ANY($1)",
    )
    .bind(ids)
    .bind(me)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r)).collect())
}

/// The readable custom field values of `cf_keys` (`cf:<id>`) for each issue.
async fn cf_values(
    conn: &mut PgConnection,
    caller: AuthContext,
    fields: &Fields,
    rows: &HashMap<Uuid, Display>,
    cf_keys: &[String],
) -> Result<HashMap<Uuid, HashMap<String, Json>>> {
    let mut out = HashMap::new();
    if cf_keys.is_empty() {
        return Ok(out);
    }
    let mut names: Vec<String> = Vec::new();
    let mut key_of: HashMap<String, String> = HashMap::new();
    for k in cf_keys {
        if let Some((row, _)) = find_cf(fields, k) {
            names.push(row.name.clone());
            key_of.insert(row.name.clone(), format!("cf:{}", row.id));
        }
    }
    let mut roles: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for r in rows.values() {
        if let std::collections::hash_map::Entry::Vacant(e) = roles.entry(r.project_id) {
            e.insert(db::role_ids_for(&mut *conn, caller, r.project_id).await?);
        }
        let vals = service::custom_field_values(
            &mut *conn,
            caller,
            &roles[&r.project_id],
            CfTarget {
                issue_id: r.id,
                tracker_id: r.tracker_id,
                project_id: r.project_id,
            },
            Some(names.as_slice()),
        )
        .await?;
        let mut m = HashMap::new();
        for v in vals {
            if let Some(k) = key_of.get(&v.name) {
                m.insert(k.clone(), v.value.unwrap_or(Json::Null));
            }
        }
        out.insert(r.id, m);
    }
    Ok(out)
}

fn value_of(col: &str, r: &Display, cf: &HashMap<String, Json>) -> Json {
    let person = |id: Option<Uuid>, name: &Option<String>, kind: &Option<String>| match id {
        Some(id) => json!({"id": id, "name": name, "kind": kind}),
        None => Json::Null,
    };
    match col {
        "subject" => json!(r.subject),
        "project" => json!({"id": r.project_id, "key": r.project_key, "name": r.project_name}),
        "tracker" => json!({"id": r.tracker_id, "name": r.tracker_name}),
        "status" => {
            json!({"id": r.status_id, "name": r.status_name, "category": r.status_category})
        }
        "status_category" => json!(r.status_category),
        "assignee" => person(r.assigned_to_id, &r.assignee_name, &r.assignee_kind),
        "author" => json!({"id": r.author_id, "name": r.author_name, "kind": r.author_kind}),
        "priority" => r
            .priority_id
            .map_or(Json::Null, |id| json!({"id": id, "name": r.priority_name})),
        "fixed_version" => r
            .fixed_version_id
            .map_or(Json::Null, |id| json!({"id": id, "name": r.version_name})),
        "is_private" => json!(r.is_private),
        "start_date" => json!(r.start_date.map(|d| d.to_string())),
        "due_date" => json!(r.due_date.map(|d| d.to_string())),
        "created_at" => json!(r.created_at),
        "updated_at" => json!(r.updated_at),
        "estimated_hours" => json!(r.estimated_hours),
        "spent_hours" => json!(r.spent_cents.map(crate::time_entries::fmt_hours)),
        "triage" => json!(r.triage),
        k => cf.get(k).cloned().unwrap_or(Json::Null),
    }
}

fn values_map(
    cols: &[String],
    r: &Display,
    cf: &HashMap<String, Json>,
    today: NaiveDate,
) -> Map<String, Json> {
    let mut m = Map::new();
    for c in cols {
        if c != "key" {
            m.insert(c.clone(), value_of(c, r, cf));
        }
    }
    let overdue = r.due_date.is_some_and(|d| d < today)
        && matches!(r.status_category.as_str(), "todo" | "doing");
    m.insert("overdue".into(), json!(overdue));
    m
}

fn column_label(fields: &Fields, key: &str) -> String {
    match key {
        "key" => "單號".into(),
        "subject" => "標題".into(),
        k if k.starts_with("cf:") => find_cf(fields, k)
            .map(|(r, _)| r.name.clone())
            .unwrap_or_default(),
        k => v2::label(k).to_string(),
    }
}

/// The run's resolved conditions.
struct Base {
    visible: Pred,
    clock: v2::Clock,
    fields: Fields,
    /// The view's own conditions and `override.where_and`.
    chosen: Vec<Filter>,
    /// The bound project.
    fixed: Vec<Filter>,
    q: Option<String>,
    where_and: Option<Json>,
}

async fn base(
    conn: &mut PgConnection,
    caller: AuthContext,
    p: &Prepared,
    ov: Option<&RunOverride>,
) -> Result<Base> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let clock = v2::clock(&mut *conn).await?;
    let fields: Fields = filter::readable_fields(&mut *conn, caller, &scope).await?;
    let mut chosen = Vec::new();
    if let Some(w) = &p.def.where_ {
        let r = v2::resolve(&mut *conn, caller, &scope, clock, w, p.project)
            .await
            .map_err(|e| repath(e, "where", "def.query.where", true))?;
        let f = if p.involving {
            // builtin:involving_me: assigned to me, created by me, or I wrote
            // to it in the last 30 days.
            let wrote = filter::builtin_cond(
                filter::WROTE_RECENTLY_FIELD,
                Test::In(caller.user_id().into_iter().map(FValue::Ref).collect()),
            )?;
            Filter::Or(vec![r.filter, wrote])
        } else {
            r.filter
        };
        chosen.push(f);
    }
    let mut where_and = None;
    if let Some(w) = ov.and_then(|o| o.where_and.as_ref()) {
        let r = v2::resolve(&mut *conn, caller, &scope, clock, w, p.project)
            .await
            .map_err(|e| repath(e, "where", "override.where_and", false))?;
        chosen.push(r.filter);
        where_and = Some(r.normalized);
    }
    let mut fixed = Vec::new();
    if let Some(proj) = p.project {
        fixed.push(filter::builtin_cond(
            filter::PROJECT_FIELD,
            Test::Cmp(Cmp::Eq, FValue::Ref(proj)),
        )?);
    }
    let q = match ov.and_then(|o| o.q.as_deref()).map(str::trim) {
        Some(q) if !q.is_empty() => Some(q.to_string()),
        _ => p.def.q.clone(),
    };
    Ok(Base {
        visible: scope.predicate(),
        clock,
        fields,
        chosen,
        fixed,
        q,
        where_and,
    })
}

async fn resolve_cond(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Option<Uuid>,
    clock: v2::Clock,
    c: &Json,
    path: &str,
) -> Result<Filter> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    Ok(v2::resolve(&mut *conn, caller, &scope, clock, c, project)
        .await
        .map_err(|e| repath(e, "where", path, true))?
        .filter)
}

/// The sort of a run: `lead` columns first, then the view's `sort`.
fn run_sort(
    fields: &Fields,
    lead: Vec<v2::SortCol>,
    lead_key: Option<&str>,
    def_sort: Option<&Vec<v2::SortInput>>,
) -> Result<Option<(Vec<v2::SortCol>, Json)>> {
    let mut cols = lead;
    let mut norm = Json::Null;
    if let Some(keys) = def_sort {
        let (c, n) = v2::resolve_sort(fields, keys).map_err(|e| {
            let path = classify(&e)
                .and_then(|a| a.details)
                .and_then(|d| d["path"].as_str().map(str::to_string))
                .unwrap_or_else(|| "sort".to_string());
            field_unavailable(&format!("def.{path}"))
        })?;
        cols.extend(c);
        norm = n;
    }
    Ok((!cols.is_empty()).then(|| (cols, json!({"lead": lead_key, "sort": norm}))))
}

fn limit_of(l: Option<u32>) -> u32 {
    l.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// One lane of a board, resolved.
struct LaneSpec {
    id: String,
    title: String,
    values: Vec<Json>,
    wip_max: Option<i64>,
    filter: Filter,
}

/// `POST /views/{id}/run`.
pub async fn run(app: &App, caller: AuthContext, id: &str, input: &RunInput) -> Result<ViewRun> {
    let mut tx = read_tx(app).await?;
    let l = load(&mut tx, caller, id, false).await?;
    require_bound_project(&mut tx, caller, &l).await?;
    let p = prepared(&l)?;
    let r = execute(&mut tx, app, caller, &p, input, None).await?;
    tx.commit().await?;
    Ok(r.0)
}

/// `POST /views/preview`: the same checks as saving, then a run.
pub async fn preview(app: &App, caller: AuthContext, input: &PreviewInput) -> Result<ViewRun> {
    let mut tx = read_tx(app).await?;
    let scope = input.scope.as_deref().map(str::trim).unwrap_or("personal");
    if !["personal", "project"].contains(&scope) {
        return Err(bad_arg("scope", "scope is personal or project"));
    }
    let project = target_project(&mut tx, caller, input.project_id.as_deref()).await?;
    if scope == "project" && project.is_none() {
        return Err(bad_arg("project_id", "a project view needs project_id"));
    }
    let def = normalize(
        &mut tx,
        caller,
        &input.def,
        None,
        SaveCtx {
            project,
            public: scope == "project",
        },
    )
    .await?;
    let p = Prepared {
        head: ViewHead {
            id: "preview".into(),
            kind: def.kind.as_str().into(),
            name: String::new(),
        },
        def_hash: def.hash(),
        def,
        project,
        involving: false,
    };
    let run_input = RunInput {
        cursor: input.cursor.clone(),
        limit: input.limit,
        lane: input.lane.clone(),
        swimlane: input.swimlane.clone(),
        override_: input.override_.clone(),
    };
    let r = execute(&mut tx, app, caller, &p, &run_input, None).await?;
    tx.commit().await?;
    Ok(r.0)
}

/// The run, for people (`agent_fields = None`: display values, groups,
/// totals) or agents (search's compact lines with `agent_fields`).
async fn execute(
    conn: &mut PgConnection,
    app: &App,
    caller: AuthContext,
    p: &Prepared,
    input: &RunInput,
    agent_fields: Option<Option<Vec<String>>>,
) -> Result<(ViewRun, Option<AgentViewRun>)> {
    let b = base(&mut *conn, caller, p, input.override_.as_ref()).await?;
    let limit = limit_of(input.limit);
    let agent = agent_fields.is_some();
    let fields_param = match &agent_fields {
        Some(f) => f.clone(),
        None => Some(vec!["key".into(), "lock_version".into()]),
    };
    let counter = Counter::start();
    let meta = RunMeta {
        today: b.clock.today.to_string(),
    };
    let hash_base = json!({
        "view": p.head.id, "def": p.def_hash, "where_and": b.where_and,
    });
    match p.def.kind {
        Kind::List => {
            if input.lane.is_some() || input.swimlane.is_some() {
                return Err(bad_arg("lane", "lane and swimlane are for boards"));
            }
            let group = match &p.def.group_by {
                Some(g) => Some(GroupKey::resolve(&b.fields, g, "def.group_by")?),
                None => None,
            };
            let lead = group.as_ref().map(GroupKey::lead).unwrap_or_default();
            let lead_key = group.as_ref().map(GroupKey::field);
            let sort = run_sort(&b.fields, lead, lead_key.as_deref(), p.def.sort.as_ref())?;
            let mut hash = hash_base.clone();
            hash["mode"] = json!("list");
            let res = service::search_with(
                &mut *conn,
                app,
                caller,
                SearchParams {
                    q: b.q.clone(),
                    limit: Some(limit),
                    cursor: input.cursor.clone(),
                    fields: fields_param,
                    explain: Some("none".into()),
                    ..Default::default()
                },
                Extra {
                    chosen: b.chosen.clone(),
                    fixed: b.fixed.clone(),
                    sort,
                    hash: Some(hash),
                },
            )
            .await?;
            if agent {
                let a = AgentViewRun {
                    view: p.head.clone(),
                    items: Some(res.items),
                    next_cursor: res.next_cursor,
                    lanes: None,
                    unmapped: None,
                    meta: meta.clone(),
                };
                return Ok((empty_run(p, &meta), Some(a)));
            }
            let ids: Vec<Uuid> = res.items.iter().map(|i| i.id).collect();
            let rows = displays(&mut *conn, caller.user_id(), &ids).await?;
            let columns: Vec<String> = p
                .def
                .columns
                .iter()
                .filter(|c| !c.starts_with("cf:") || find_cf(&b.fields, c).is_some())
                .cloned()
                .collect();
            let mut cf_keys: Vec<String> = columns
                .iter()
                .filter(|c| c.starts_with("cf:"))
                .cloned()
                .collect();
            if let Some(g) = &group
                && let GroupKey::Cf { .. } = g
            {
                cf_keys.push(g.field());
            }
            let cfs = cf_values(&mut *conn, caller, &b.fields, &rows, &cf_keys).await?;
            let empty = HashMap::new();
            let mut items = Vec::new();
            let mut groups: Vec<GroupHead> = Vec::new();
            for it in &res.items {
                let Some(r) = rows.get(&it.id) else {
                    continue;
                };
                let cf = cfs.get(&it.id).unwrap_or(&empty);
                let g = group.as_ref().map(|g| g.value_of(r, cf));
                if let Some((v, label)) = &g
                    && !groups.iter().any(|x| &x.value == v)
                {
                    groups.push(GroupHead {
                        value: v.clone(),
                        label: label.clone(),
                        count: None,
                    });
                }
                items.push(RunItem {
                    key: it.key.clone(),
                    id: it.id,
                    lock_version: it.lock_version.unwrap_or(0),
                    group: g.map(|x| x.0),
                    swimlane: None,
                    values: Some(values_map(&columns, r, cf, b.clock.today)),
                    card: None,
                });
            }
            let counting = b.q.is_none();
            let base_clauses: Vec<Clause> = b
                .fixed
                .iter()
                .chain(b.chosen.iter())
                .map(Clause::F)
                .collect();
            if let Some(g) = &group
                && counting
            {
                for head in groups.iter_mut() {
                    let na = match g {
                        GroupKey::Cf { gate, .. } if head.value == json!(NOT_APPLICABLE) => {
                            Some(gate)
                        }
                        _ => None,
                    };
                    let f = match na {
                        Some(_) => None,
                        None => Some(
                            resolve_cond(
                                &mut *conn,
                                caller,
                                p.project,
                                b.clock,
                                &g.cond(std::slice::from_ref(&head.value)),
                                "def.group_by",
                            )
                            .await?,
                        ),
                    };
                    let mut cl: Vec<Clause> = b
                        .fixed
                        .iter()
                        .chain(b.chosen.iter())
                        .map(Clause::F)
                        .collect();
                    match (&f, na) {
                        (Some(f), _) => cl.push(Clause::F(f)),
                        (None, Some(gate)) => cl.push(Clause::NotApplies(gate)),
                        (None, None) => {}
                    }
                    head.count = counter.count(&mut *conn, &b.visible, &cl).await?;
                }
            }
            let totals = if p.def.totals.is_empty() {
                None
            } else {
                let mut t = BTreeMap::new();
                for k in &p.def.totals {
                    let value = if counting && k == "spent_hours" {
                        counter
                            .sum_spent(&mut *conn, caller.user_id(), &b.visible, &base_clauses)
                            .await?
                    } else if counting {
                        counter
                            .sum_estimated(&mut *conn, &b.visible, &base_clauses)
                            .await?
                    } else {
                        None
                    };
                    t.insert(
                        k.clone(),
                        Total {
                            complete: value.is_some(),
                            value,
                        },
                    );
                }
                Some(t)
            };
            Ok((
                ViewRun {
                    view: p.head.clone(),
                    columns: Some(
                        columns
                            .iter()
                            .map(|c| ColumnHead {
                                key: c.clone(),
                                label: column_label(&b.fields, c),
                            })
                            .collect(),
                    ),
                    groups: group.as_ref().map(|_| groups),
                    items: Some(items),
                    totals,
                    next_cursor: res.next_cursor,
                    coverage: Some(Coverage {
                        truncated: res.truncated,
                    }),
                    lanes: None,
                    swimlanes: None,
                    unmapped: None,
                    meta,
                },
                None,
            ))
        }
        Kind::Board => {
            if input.cursor.is_some() && input.lane.is_none() {
                return Err(bad_arg("lane", "a board cursor comes with its lane"));
            }
            let lanes_def = p.def.lanes.as_ref().expect("board lanes");
            let by = GroupKey::resolve(&b.fields, &lanes_def.by, "def.lanes.by")?;
            let done = resolve_cond(
                &mut *conn,
                caller,
                p.project,
                b.clock,
                &json!({"or": [
                    {"field": "status_category", "op": "in", "value": ["todo", "doing"]},
                    {"field": "updated_at", "op": "within",
                     "value": {"range": "last_n_days", "n": p.def.done_window_days}}]}),
                "def.done_window_days",
            )
            .await?;
            let (specs, lanes_truncated) =
                lane_specs(&mut *conn, caller, p, &b, &by, &done).await?;
            let status_count = if lanes_def.by == "status" {
                let mapped: Vec<Uuid> = specs
                    .iter()
                    .flat_map(|l| l.values.iter())
                    .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                    .collect();
                let n: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM issue_statuses WHERE NOT (id = ANY($1))",
                )
                .bind(&mapped)
                .fetch_one(&mut *conn)
                .await?;
                Some(n)
            } else {
                None
            };
            let swim = match &p.def.swimlanes {
                Some(s) => Some(GroupKey::resolve(&b.fields, s, "def.swimlanes.by")?),
                None => None,
            };
            let swim_filter = match (&swim, input.swimlane.as_deref()) {
                (Some(_), Some(v)) if v.trim() == NOT_APPLICABLE => {
                    return Err(bad_arg(
                        "swimlane",
                        "the not_applicable swimlane cannot be opened alone; run the board without swimlane",
                    ));
                }
                (Some(s), Some(v)) => {
                    let v = if v.trim() == "none" {
                        Json::Null
                    } else {
                        json!(v.trim())
                    };
                    Some(
                        resolve_cond(
                            &mut *conn,
                            caller,
                            p.project,
                            b.clock,
                            &s.cond(&[v]),
                            "swimlane",
                        )
                        .await?,
                    )
                }
                (None, Some(_)) => return Err(bad_arg("swimlane", "this board has no swimlanes")),
                _ => None,
            };
            let only: Vec<&LaneSpec> = match input.lane.as_deref() {
                Some(id) => vec![
                    specs
                        .iter()
                        .find(|l| l.id == id.trim())
                        .ok_or_else(|| bad_arg("lane", "no such lane in this board"))?,
                ],
                None => specs.iter().collect(),
            };
            let counting = b.q.is_none();
            let lead = swim.as_ref().map(GroupKey::lead).unwrap_or_default();
            let lead_key = swim.as_ref().map(GroupKey::field);
            // Every lane's items first (the search's own budget), then the
            // counts (theirs, R-VD6).
            let mut found = Vec::with_capacity(only.len());
            for lane in &only {
                let sort = run_sort(
                    &b.fields,
                    lead.clone(),
                    lead_key.as_deref(),
                    p.def.sort.as_ref(),
                )?;
                let mut fixed = b.fixed.clone();
                fixed.push(lane.filter.clone());
                fixed.push(done.clone());
                if let Some(f) = &swim_filter {
                    fixed.push(f.clone());
                }
                let mut hash = hash_base.clone();
                hash["mode"] = json!("board");
                hash["lane"] = json!(lane.id);
                hash["swimlane"] = json!(input.swimlane);
                let res = service::search_with(
                    &mut *conn,
                    app,
                    caller,
                    SearchParams {
                        q: b.q.clone(),
                        limit: Some(limit),
                        cursor: if input.lane.is_some() {
                            input.cursor.clone()
                        } else {
                            None
                        },
                        fields: fields_param.clone(),
                        explain: Some("none".into()),
                        ..Default::default()
                    },
                    Extra {
                        chosen: b.chosen.clone(),
                        fixed,
                        sort,
                        hash: Some(hash),
                    },
                )
                .await?;
                found.push(res);
            }
            let board_clauses = || {
                let mut cl: Vec<Clause> = b
                    .fixed
                    .iter()
                    .chain(b.chosen.iter())
                    .map(Clause::F)
                    .collect();
                cl.push(Clause::F(&done));
                if let Some(f) = &swim_filter {
                    cl.push(Clause::F(f));
                }
                cl
            };
            let mut counts = Vec::with_capacity(only.len());
            for lane in &only {
                counts.push(if counting {
                    let mut cl = board_clauses();
                    cl.push(Clause::F(&lane.filter));
                    counter.count(&mut *conn, &b.visible, &cl).await?
                } else {
                    None
                });
            }
            let unmapped = Unmapped {
                status_count,
                issues: if counting {
                    let mut cl = board_clauses();
                    cl.push(Clause::NoneOf(specs.iter().map(|l| &l.filter).collect()));
                    counter.count(&mut *conn, &b.visible, &cl).await?
                } else {
                    None
                },
                lanes_truncated,
            };
            let mut lanes_out: Vec<LaneOut> = Vec::new();
            let mut agent_lanes: Vec<AgentLane> = Vec::new();
            let mut swim_heads: Vec<SwimlaneHead> = Vec::new();
            for ((lane, res), count) in only.iter().zip(found).zip(counts) {
                if agent {
                    agent_lanes.push(AgentLane {
                        id: lane.id.clone(),
                        title: lane.title.clone(),
                        count,
                        items: res.items,
                        next_cursor: res.next_cursor,
                    });
                    continue;
                }
                let ids: Vec<Uuid> = res.items.iter().map(|i| i.id).collect();
                let rows = displays(&mut *conn, caller.user_id(), &ids).await?;
                let cards: Vec<String> = p
                    .def
                    .card_fields
                    .iter()
                    .filter(|c| !c.starts_with("cf:") || find_cf(&b.fields, c).is_some())
                    .cloned()
                    .collect();
                let cf_keys: Vec<String> = cards
                    .iter()
                    .filter(|c| c.starts_with("cf:"))
                    .cloned()
                    .collect();
                let cfs = cf_values(&mut *conn, caller, &b.fields, &rows, &cf_keys).await?;
                let empty = HashMap::new();
                let mut items = Vec::new();
                for it in &res.items {
                    let Some(r) = rows.get(&it.id) else {
                        continue;
                    };
                    let cf = cfs.get(&it.id).unwrap_or(&empty);
                    let sv = swim.as_ref().map(|s| s.value_of(r, cf));
                    if let Some((v, label)) = &sv
                        && !swim_heads.iter().any(|x| &x.value == v)
                    {
                        swim_heads.push(SwimlaneHead {
                            value: v.clone(),
                            label: label.clone(),
                        });
                    }
                    items.push(RunItem {
                        key: it.key.clone(),
                        id: it.id,
                        lock_version: it.lock_version.unwrap_or(0),
                        group: None,
                        swimlane: sv.map(|x| x.0),
                        values: None,
                        card: Some(values_map(&cards, r, cf, b.clock.today)),
                    });
                }
                let show = input.lane.is_some()
                    || lanes_def.show_empty
                    || !items.is_empty()
                    || count.is_some_and(|c| c.value > 0);
                if !show {
                    continue;
                }
                lanes_out.push(LaneOut {
                    id: lane.id.clone(),
                    title: lane.title.clone(),
                    values: lane.values.clone(),
                    wip: lane.wip_max.map(|max| Wip {
                        max,
                        exceeded: count.map(|c| c.value > max || c.capped),
                    }),
                    count,
                    items,
                    next_cursor: res.next_cursor,
                });
            }
            if agent {
                let a = AgentViewRun {
                    view: p.head.clone(),
                    items: None,
                    next_cursor: None,
                    lanes: Some(agent_lanes),
                    unmapped: Some(unmapped),
                    meta: meta.clone(),
                };
                return Ok((empty_run(p, &meta), Some(a)));
            }
            // Not set last, as the lead order puts it.
            if let Some(i) = swim_heads.iter().position(|h| h.value.is_null()) {
                let h = swim_heads.remove(i);
                swim_heads.push(h);
            }
            Ok((
                ViewRun {
                    view: p.head.clone(),
                    columns: None,
                    groups: None,
                    items: None,
                    totals: None,
                    next_cursor: None,
                    coverage: None,
                    lanes: Some(lanes_out),
                    swimlanes: swim.as_ref().map(|_| swim_heads),
                    unmapped: Some(unmapped),
                    meta,
                },
                None,
            ))
        }
    }
}

fn empty_run(p: &Prepared, meta: &RunMeta) -> ViewRun {
    ViewRun {
        view: p.head.clone(),
        columns: None,
        groups: None,
        items: None,
        totals: None,
        next_cursor: None,
        coverage: None,
        lanes: None,
        swimlanes: None,
        unmapped: None,
        meta: meta.clone(),
    }
}

/// The lanes of a board: the saved ones, or (other than by status) one per
/// value plus 未設定 (design 05 §3.3), and whether the values stopped at
/// the lane limit. The lanes never depend on the count budget: only counts
/// may come back null.
async fn lane_specs(
    conn: &mut PgConnection,
    caller: AuthContext,
    p: &Prepared,
    b: &Base,
    by: &GroupKey,
    done: &Filter,
) -> Result<(Vec<LaneSpec>, bool)> {
    let lanes = p.def.lanes.as_ref().expect("board lanes");
    let mut truncated = false;
    let defs: Vec<LaneDef> = match &lanes.items {
        Some(items) => items.clone(),
        None => {
            let mut out: Vec<(Json, String)> = Vec::new();
            let nullable =
                !matches!(by, GroupKey::Builtin(k) if k == "status_category" || k == "priority");
            // One lane is 未設定 where the value can be empty.
            let cap = if nullable { MAX_LANES - 1 } else { MAX_LANES };
            match by {
                GroupKey::Builtin(k) if k == "status_category" => {
                    for c in ["todo", "doing", "done"] {
                        out.push((json!(c), category_label(c)));
                    }
                }
                GroupKey::Builtin(k) if k == "priority" => {
                    let rows: Vec<(Uuid, String)> = sqlx::query_as(
                        "SELECT id, name FROM issue_priorities WHERE active
                         ORDER BY position, name LIMIT $1",
                    )
                    .bind(cap as i64 + 1)
                    .fetch_all(&mut *conn)
                    .await?;
                    out.extend(rows.into_iter().map(|(id, n)| (json!(id), n)));
                }
                GroupKey::Builtin(k) if k == "fixed_version" => {
                    let rows: Vec<(Uuid, String)> = sqlx::query_as(
                        "SELECT id, name FROM versions WHERE project_id = $1 AND deleted_at IS NULL
                           AND status <> 'closed' ORDER BY effective_date NULLS LAST, name_key LIMIT $2",
                    )
                    .bind(p.project)
                    .bind(cap as i64 + 1)
                    .fetch_all(&mut *conn)
                    .await?;
                    out.extend(rows.into_iter().map(|(id, n)| (json!(id), n)));
                }
                GroupKey::Cf {
                    id,
                    ty: FieldType::List,
                    ..
                } => {
                    let rows: Vec<(Uuid, String)> = sqlx::query_as(
                        "SELECT id, label FROM field_options WHERE field_id = $1 AND active
                         ORDER BY position, label LIMIT $2",
                    )
                    .bind(id)
                    .bind(cap as i64 + 1)
                    .fetch_all(&mut *conn)
                    .await?;
                    out.extend(rows.into_iter().map(|(id, n)| (json!(id), n)));
                }
                _ => {
                    // People (assignee, user fields): the ones the visible
                    // matches on the board have (done window applied), by
                    // name. One statement, so the gate's parameters bind.
                    let mut qb: QueryBuilder<Postgres> =
                        QueryBuilder::new("SELECT x.v, pp.name FROM (SELECT DISTINCT ");
                    match by {
                        GroupKey::Cf { id, gate, .. } => {
                            qb.push("(CASE WHEN ");
                            push_pred(&mut qb, "i", gate);
                            qb.push(format!(
                                " THEN (SELECT cf.v_ref FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {id}) END)"
                            ));
                        }
                        _ => {
                            qb.push("i.assigned_to_id");
                        }
                    }
                    qb.push(" AS v");
                    let mut cl: Vec<Clause> = b
                        .fixed
                        .iter()
                        .chain(b.chosen.iter())
                        .map(Clause::F)
                        .collect();
                    cl.push(Clause::F(done));
                    Counter::from_where(&mut qb, &b.visible, &cl);
                    qb.push(format!(
                        ") x JOIN principals pp ON pp.id = x.v ORDER BY pp.name, x.v LIMIT {}",
                        cap + 1
                    ));
                    for r in qb.build().fetch_all(&mut *conn).await? {
                        let id: Uuid = r.try_get(0)?;
                        let name: String = r.try_get(1)?;
                        out.push((json!(id), name));
                    }
                }
            }
            if out.len() > cap {
                out.truncate(cap);
                truncated = true;
            }
            let mut defs: Vec<LaneDef> = out
                .into_iter()
                .map(|(v, title)| LaneDef {
                    id: v.as_str().unwrap_or_default().to_string(),
                    title: title.chars().take(MAX_LANE_TITLE_CHARS).collect(),
                    values: vec![v],
                    wip_max: None,
                })
                .collect();
            if nullable {
                defs.push(LaneDef {
                    id: "none".into(),
                    title: if by.field() == "assignee" {
                        "未指派".into()
                    } else {
                        "未設定".into()
                    },
                    values: vec![Json::Null],
                    wip_max: None,
                });
            }
            defs
        }
    };
    let mut out = Vec::with_capacity(defs.len());
    for (n, d) in defs.into_iter().enumerate() {
        let filter = resolve_cond(
            &mut *conn,
            caller,
            p.project,
            b.clock,
            &by.cond(&d.values),
            &format!("def.lanes.items[{n}].values"),
        )
        .await?;
        out.push(LaneSpec {
            id: d.id,
            title: d.title,
            values: d.values,
            wip_max: d.wip_max,
            filter,
        });
    }
    Ok((out, truncated))
}

// ---------------------------------------------------------------- MCP

/// MCP `view_list`: built-ins without `me` and the project views of the
/// agent's delegated projects (never the delegator's personal views).
pub async fn agent_list(app: &App, caller: AuthContext, p: &ViewListParams) -> Result<ViewList> {
    let mut l = list(
        app,
        caller,
        p.project.as_deref(),
        None,
        None,
        Some(MAX_LIMIT),
    )
    .await?;
    for it in &mut l.items {
        it.editable = false;
    }
    Ok(l)
}

/// MCP `view_run`: with the agent's read scope (R-VD9: `me` is
/// `me_unavailable`).
pub async fn agent_run(app: &App, caller: AuthContext, p: &ViewRunParams) -> Result<AgentViewRun> {
    let mut tx = read_tx(app).await?;
    let l = load(&mut tx, caller, &p.view, false).await?;
    require_bound_project(&mut tx, caller, &l).await?;
    let prep = prepared(&l)?;
    let input = RunInput {
        cursor: p.cursor.clone(),
        limit: p.limit,
        lane: p.lane.clone(),
        swimlane: None,
        override_: None,
    };
    let r = execute(&mut tx, app, caller, &prep, &input, Some(p.fields.clone())).await?;
    tx.commit().await?;
    r.1.ok_or_else(|| anyhow::anyhow!("agent run without agent output"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let ok = parse_shape(
            &json!({"kind": "list", "columns": ["key", "subject"], "sort": [{"field": "priority", "dir": "desc"}]}),
            None,
        )
        .unwrap();
        assert_eq!(ok.kind, Kind::List);
        let e = parse_shape(&json!({"kind": "list", "lanes": {}}), None).unwrap_err();
        assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidView);
        let e = parse_shape(
            &json!({"kind": "board", "lanes": {"by": "status"}}),
            Some(Kind::List),
        )
        .unwrap_err();
        assert_eq!(
            classify(&e).unwrap().details.as_ref().unwrap()["reason"],
            "kind_mismatch"
        );
        let e = parse_shape(&json!({"columns": ["key"]}), None).unwrap_err();
        assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidView);
        let big = "x".repeat(MAX_DEF_BYTES);
        let e = parse_shape(&json!({"kind": "list", "query": {"q": big}}), None).unwrap_err();
        assert_eq!(
            classify(&e).unwrap().details.as_ref().unwrap()["reason"],
            "too_large"
        );
        for b in [
            Builtin::AssignedToMe,
            Builtin::ReportedByMe,
            Builtin::InvolvingMe,
            Builtin::ProjectIssues(Uuid::nil()),
            Builtin::ProjectBoard(Uuid::nil()),
        ] {
            parse_shape(&b.def(), None).unwrap();
        }
    }

    #[test]
    fn ids() {
        assert!(matches!(
            parse_id("builtin:assigned_to_me").unwrap(),
            ViewRef::Builtin(Builtin::AssignedToMe)
        ));
        assert!(matches!(
            parse_id("builtin:project_board:AUTH").unwrap(),
            ViewRef::BuiltinProject { board: true, .. }
        ));
        assert!(parse_id("builtin:nope").is_err());
        assert!(parse_id("nope").is_err());
    }
}
