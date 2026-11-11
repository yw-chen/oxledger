// SPDX-License-Identifier: GPL-2.0-or-later
//! Filter AST v2 (design 05 §3.2): built-in issue fields, `me`, relative
//! dates, `ever_in` and sorting, on top of the v1 custom field filter.
//!
//! User input (`where`) is parsed here, checked against the limits, and
//! resolved — names to ids, `me` / `next` / relative dates to values for
//! this request — into the same typed [`Filter`] the v1 `filter` becomes, so
//! one SQL compiler ([`super::push_filter_shaped`]) serves both. Resolution
//! also returns the normalized form: ids instead of names, but `me`, `next`
//! and relative dates kept as dynamic values (saved views and cursors bind
//! to it).
//!
//! Errors are `invalid_filter` with `details = {path, reason}`. A custom
//! field the caller cannot read, a project they cannot see and a version of
//! such a project look exactly like ones that do not exist
//! (`unknown_field` / `unknown_value`): messages only repeat the input.

use super::{
    ASSIGNEE_FIELD, AUTHOR_FIELD, CATEGORY_FIELD, CREATED_FIELD, DUE_FIELD, ESTIMATED_FIELD,
    ON_BEHALF_FIELD, PRIORITY_FIELD, PRIVATE_FIELD, PROJECT_FIELD, START_FIELD, STATUS_FIELD,
    TRACKER_FIELD, TRIAGE_FIELD, TRIAGE_SELF_FIELD, UPDATED_FIELD, VERSION_FIELD, builtin_cond,
    operators, readable_fields, spent_cond,
};
use crate::auth::AuthContext;
use crate::commands::canonical_number;
use crate::custom_fields::{self, FieldRow};
use crate::db::push_pred;
use crate::error::{ApiError, ErrorCode};
use crate::service::resolve_one;
use anyhow::Result;
use chrono::{Datelike, Days, Months, NaiveDate};
use ox_core::filter::{Cmp, Field, FieldType, Filter, Test, Value};
use ox_core::{Pred, Scope};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

/// Nesting of and/or (design 05 §3.2: depth ≤ 3).
pub const MAX_DEPTH: usize = 3;
pub const MAX_CONDITIONS: usize = 30;
pub const MAX_IN_VALUES: usize = 100;
/// The serialized `where`, in bytes.
pub const MAX_BYTES: usize = 16 * 1024;
pub const MAX_SORT: usize = 3;
pub const MAX_REL_OFFSET: i64 = 3650;
pub const MAX_REL_N: i64 = 366;
/// Distinct names (not ids) one `where` may look up: each costs a `resolve`
/// query, so 30 × 100 different names would hold the request for seconds.
/// Repeated values and ids do not count (ids are checked in one batch).
pub const MAX_NAME_LOOKUPS: usize = 100;
/// Dates are years 1–9999 (PostgreSQL rejects some years chrono accepts).
const MIN_YEAR: i32 = 1;
const MAX_YEAR: i32 = 9999;

// ---------------------------------------------------------------- published shape

/// Filter AST v2 (design 05 §3.2): a condition, or `{"and": [...]}` /
/// `{"or": [...]}` of filters. At most 3 levels of and/or, 30 conditions,
/// 100 values per list, 100 different names (ids do not count) and 16 KiB.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum WhereInput {
    And {
        /// All of these must hold.
        and: Vec<WhereInput>,
    },
    Or {
        /// At least one of these must hold.
        or: Vec<WhereInput>,
    },
    Cond(WhereCond),
}

/// One condition. Values: ids (uuid) or names / keys / logins for project,
/// tracker, priority, status, assignee, author; `{"me": true}` (or "me") for
/// assignee and author (assignee also matches the caller's groups);
/// `{"next": true}` for fixed_version (needs exactly one project);
/// `"YYYY-MM-DD"` or `{"rel": "today", "offset": -3}` for dates; `within`
/// takes `{"range": "this_week"|"last_week"|"next_week"|"this_month"|
/// "last_month"|"last_n_days"|"next_n_days", "n"?: 1–366}`; decimal strings
/// for estimated_hours and spent_hours; status_category `todo`/`doing`/`done`/`open`;
/// custom fields `{"list": "<option id>"}`, `{"user": "<id>"}`,
/// `{"int": "12"}`, `{"date": "…"}`, … or plain values as in `filter`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct WhereCond {
    /// project, tracker, priority, status, status_category, assignee, author,
    /// fixed_version, is_private, start_date, due_date, created_at,
    /// updated_at, estimated_hours, spent_hours (people only), overdue, or
    /// cf:<custom field id>.
    pub field: String,
    pub op: WhereOp,
    /// eq/ne/lt/le/gt/ge: one value; in/not_in/ever_in: an array;
    /// between: [min, max] (inclusive, either may be null); within: a range
    /// object; is_null/not_null: omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
}

/// `ne` and `not_in` also match issues without a value; `ever_in` (status,
/// assignee, fixed_version) also matches past values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WhereOp {
    Eq,
    Ne,
    In,
    NotIn,
    Lt,
    Le,
    Gt,
    Ge,
    Between,
    Within,
    IsNull,
    NotNull,
    EverIn,
}

const OPS: [(&str, WhereOp); 13] = [
    ("eq", WhereOp::Eq),
    ("ne", WhereOp::Ne),
    ("in", WhereOp::In),
    ("not_in", WhereOp::NotIn),
    ("lt", WhereOp::Lt),
    ("le", WhereOp::Le),
    ("gt", WhereOp::Gt),
    ("ge", WhereOp::Ge),
    ("between", WhereOp::Between),
    ("within", WhereOp::Within),
    ("is_null", WhereOp::IsNull),
    ("not_null", WhereOp::NotNull),
    ("ever_in", WhereOp::EverIn),
];

impl WhereOp {
    pub fn as_str(self) -> &'static str {
        OPS.iter()
            .find(|(_, o)| *o == self)
            .map(|(s, _)| *s)
            .unwrap()
    }
}

/// One sort key (design 05 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SortInput {
    /// updated_at, created_at, due_date, start_date, priority, status, key,
    /// estimated_hours, or cf:<id> of an int, float, date, string or list
    /// field (list: by option position).
    pub field: String,
    /// asc (default) or desc. Missing values come last either way.
    #[serde(default)]
    pub dir: SortDir,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

// ---------------------------------------------------------------- the clock

/// "Today" in the organization's time zone, once per request (02 §17.3
/// R10), and the first day of the week (`org_settings.week_start`, 1 =
/// Monday … 7 = Sunday).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub today: NaiveDate,
    pub week_start: u32,
}

/// Tests: the instant "now" is taken at (time zone boundaries, T-M1-04).
#[cfg(feature = "test-support")]
pub static TEST_NOW: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>> =
    std::sync::Mutex::new(None);

pub async fn clock(conn: &mut PgConnection) -> Result<Clock> {
    #[cfg(feature = "test-support")]
    let now = TEST_NOW.lock().map(|g| *g).unwrap_or(None);
    #[cfg(not(feature = "test-support"))]
    let now: Option<chrono::DateTime<chrono::Utc>> = None;
    let (today, week_start): (NaiveDate, i16) = sqlx::query_as(
        "SELECT (COALESCE($1::timestamptz, now()) AT TIME ZONE timezone)::date, week_start
           FROM org_settings",
    )
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Clock {
        today,
        week_start: week_start.clamp(1, 7) as u32,
    })
}

pub const RANGES: [&str; 7] = [
    "this_week",
    "last_week",
    "next_week",
    "this_month",
    "last_month",
    "last_n_days",
    "next_n_days",
];

fn add_days(d: NaiveDate, n: i64) -> NaiveDate {
    if n >= 0 {
        d.checked_add_days(Days::new(n as u64)).unwrap_or(d)
    } else {
        d.checked_sub_days(Days::new(n.unsigned_abs())).unwrap_or(d)
    }
}

/// The inclusive date range of a `within` value. `last_n_days` is
/// `[today - n, today]` and `next_n_days` `[today, today + n]` (Redmine
/// `>t-` / `<t+`).
pub fn range_bounds(clock: Clock, range: &str, n: Option<i64>) -> Option<(NaiveDate, NaiveDate)> {
    let t = clock.today;
    let week = |shift: i64| {
        let back = (t.weekday().number_from_monday() + 7 - clock.week_start) % 7;
        let start = add_days(t, -(back as i64) + shift * 7);
        (start, add_days(start, 6))
    };
    let first = t.with_day(1)?;
    Some(match range {
        "this_week" => week(0),
        "last_week" => week(-1),
        "next_week" => week(1),
        "this_month" => (
            first,
            add_days(first.checked_add_months(Months::new(1))?, -1),
        ),
        "last_month" => (
            first.checked_sub_months(Months::new(1))?,
            add_days(first, -1),
        ),
        "last_n_days" => (add_days(t, -n?), t),
        "next_n_days" => (t, add_days(t, n?)),
        _ => return None,
    })
}

// ---------------------------------------------------------------- errors

fn ferr(path: &str, reason: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidFilter, msg)
        .with_details(json!({"path": path, "reason": reason}))
        .into()
}

fn too_complex(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ferr(path, "too_complex", msg)
}

fn bad_value(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ferr(path, "invalid_value", msg)
}

fn unknown_value(path: &str, x: &Json) -> anyhow::Error {
    ferr(
        path,
        "unknown_value",
        format!("{path}: unknown value {x} (use resolve to find ids)"),
    )
}

/// An error of a name lookup at `path`: "nothing matches" becomes
/// `unknown_value`; anything else keeps its code with `details.path`.
fn remap(e: anyhow::Error, path: &str, x: &Json) -> anyhow::Error {
    match e.downcast::<ApiError>() {
        Ok(a) if a.code == ErrorCode::InvalidArgument => unknown_value(path, x),
        // `versions::resolve_for_search`: an unknown name is an
        // `invalid_filter` without a reason.
        Ok(a)
            if a.code == ErrorCode::InvalidFilter
                && a.details.as_ref().and_then(|d| d.get("reason")).is_none() =>
        {
            unknown_value(path, x)
        }
        Ok(mut a) => {
            let mut d = a.details.take().unwrap_or_else(|| json!({}));
            if let Json::Object(m) = &mut d {
                m.insert("path".into(), json!(path));
            }
            a.details = Some(d);
            a.into()
        }
        Err(e) => e,
    }
}

// ---------------------------------------------------------------- parse

/// `where` after the syntax and size checks.
#[derive(Debug, Clone)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Cond {
        path: String,
        field: String,
        op: WhereOp,
        value: Option<Json>,
    },
}

fn parse(v: &Json) -> Result<Node> {
    if v.to_string().len() > MAX_BYTES {
        return Err(too_complex(
            "where",
            format!("where is larger than {} KiB", MAX_BYTES / 1024),
        ));
    }
    let mut n = 0;
    parse_at(v, "where", 0, &mut n)
}

fn parse_at(v: &Json, path: &str, depth: usize, n: &mut usize) -> Result<Node> {
    let Some(obj) = v.as_object() else {
        return Err(ferr(
            path,
            "invalid_shape",
            format!(
                "{path}: expected an object like {{\"field\": .., \"op\": .., \"value\": ..}} or {{\"and\": [..]}}"
            ),
        ));
    };
    for key in ["and", "or"] {
        if let Some(list) = obj.get(key) {
            if obj.len() != 1 {
                return Err(ferr(
                    path,
                    "invalid_shape",
                    format!("{path}: {key:?} must be the only key of its object"),
                ));
            }
            if depth + 1 > MAX_DEPTH {
                return Err(too_complex(
                    path,
                    format!("at most {MAX_DEPTH} levels of and/or"),
                ));
            }
            let Some(items) = list.as_array() else {
                return Err(ferr(
                    &format!("{path}.{key}"),
                    "invalid_shape",
                    format!("{path}.{key}: expected an array"),
                ));
            };
            let parts = items
                .iter()
                .enumerate()
                .map(|(i, x)| parse_at(x, &format!("{path}.{key}[{i}]"), depth + 1, n))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if key == "and" {
                Node::And(parts)
            } else {
                Node::Or(parts)
            });
        }
    }
    if let Some(k) = obj
        .keys()
        .find(|k| !["field", "op", "value"].contains(&k.as_str()))
    {
        return Err(ferr(
            path,
            "invalid_shape",
            format!("{path}: unknown key {k:?} (use field, op, value, or and/or)"),
        ));
    }
    *n += 1;
    if *n > MAX_CONDITIONS {
        return Err(too_complex(
            path,
            format!("at most {MAX_CONDITIONS} conditions"),
        ));
    }
    let Some(field) = obj.get("field").and_then(Json::as_str) else {
        return Err(ferr(
            path,
            "invalid_shape",
            format!("{path}: \"field\" (a string) is required"),
        ));
    };
    let op_text = obj.get("op").and_then(Json::as_str).unwrap_or("");
    let Some(op) = OPS.iter().find(|(s, _)| *s == op_text).map(|(_, o)| *o) else {
        return Err(ferr(
            &format!("{path}.op"),
            "unsupported_op",
            format!(
                "{path}.op: must be one of {}",
                OPS.map(|(s, _)| s).join(", ")
            ),
        ));
    };
    let value = obj.get("value").filter(|v| !v.is_null()).cloned();
    if let (WhereOp::In | WhereOp::NotIn | WhereOp::EverIn, Some(Json::Array(xs))) = (op, &value)
        && xs.len() > MAX_IN_VALUES
    {
        return Err(too_complex(
            &format!("{path}.value"),
            format!("at most {MAX_IN_VALUES} values in a list"),
        ));
    }
    Ok(Node::Cond {
        path: path.to_string(),
        field: field.to_string(),
        op,
        value,
    })
}

// ---------------------------------------------------------------- fields

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Project,
    Tracker,
    Priority,
    Status,
    Assignee,
    Author,
    Version,
}

impl RefKind {
    fn id(self) -> i32 {
        match self {
            RefKind::Project => PROJECT_FIELD,
            RefKind::Tracker => TRACKER_FIELD,
            RefKind::Priority => PRIORITY_FIELD,
            RefKind::Status => STATUS_FIELD,
            RefKind::Assignee => ASSIGNEE_FIELD,
            RefKind::Author => AUTHOR_FIELD,
            RefKind::Version => VERSION_FIELD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fk {
    Ref(RefKind),
    Category,
    Private,
    /// start_date / due_date.
    Date(i32),
    /// created_at / updated_at.
    Day(i32),
    Estimated,
    /// Contract 05 §3.5.4: the time the caller can see (people only).
    Spent,
    Overdue,
    /// Contract 05 §3.4: the triage state.
    Triage,
    /// A5: `triage_self_decided`.
    TriageSelf,
    /// Index into the readable custom fields.
    Cf(usize),
}

use WhereOp as O;

const REF_OPS: &[WhereOp] = &[O::Eq, O::Ne, O::In, O::NotIn];
const DATE_OPS: &[WhereOp] = &[
    O::Eq,
    O::Lt,
    O::Le,
    O::Gt,
    O::Ge,
    O::Between,
    O::Within,
    O::IsNull,
    O::NotNull,
];

/// The built-in fields of v2 with their ops (design 05 §3.2), in the order
/// `/filter-fields` lists them.
pub const BUILTIN_FIELDS: &[(&str, &str, &[WhereOp])] = &[
    ("project", "ref", REF_OPS),
    (
        "tracker",
        "ref",
        &[O::Eq, O::Ne, O::In, O::NotIn, O::IsNull],
    ),
    ("status", "ref", &[O::Eq, O::Ne, O::In, O::NotIn, O::EverIn]),
    ("status_category", "enum", &[O::Eq, O::In]),
    (
        "priority",
        "ref",
        &[O::Eq, O::Ne, O::In, O::NotIn, O::IsNull],
    ),
    (
        "assignee",
        "principal",
        &[
            O::Eq,
            O::Ne,
            O::In,
            O::NotIn,
            O::IsNull,
            O::NotNull,
            O::EverIn,
        ],
    ),
    ("author", "principal", REF_OPS),
    (
        "fixed_version",
        "ref",
        &[
            O::Eq,
            O::Ne,
            O::In,
            O::NotIn,
            O::IsNull,
            O::NotNull,
            O::EverIn,
        ],
    ),
    ("is_private", "bool", &[O::Eq]),
    ("start_date", "date", DATE_OPS),
    ("due_date", "date", DATE_OPS),
    (
        "created_at",
        "datetime",
        &[O::Lt, O::Le, O::Gt, O::Ge, O::Between, O::Within],
    ),
    (
        "updated_at",
        "datetime",
        &[O::Lt, O::Le, O::Gt, O::Ge, O::Between, O::Within],
    ),
    (
        "estimated_hours",
        "number",
        &[
            O::Eq,
            O::Ne,
            O::Lt,
            O::Le,
            O::Gt,
            O::Ge,
            O::Between,
            O::IsNull,
            O::NotNull,
        ],
    ),
    (
        "spent_hours",
        "number",
        &[O::Eq, O::Ne, O::Lt, O::Le, O::Gt, O::Ge, O::Between],
    ),
    ("overdue", "bool", &[O::Eq]),
    ("triage", "enum", &[O::Eq, O::In]),
    ("triage_self_decided", "bool", &[O::Eq]),
];

/// Values of the `triage` field (contract 05 §3.4).
pub const TRIAGE_VALUES: [&str; 6] = [
    "pending",
    "snoozed",
    "accepted",
    "declined",
    "duplicate",
    "none",
];

fn builtin_kind(name: &str) -> Option<Fk> {
    Some(match name {
        "project" => Fk::Ref(RefKind::Project),
        "tracker" => Fk::Ref(RefKind::Tracker),
        "priority" => Fk::Ref(RefKind::Priority),
        "status" => Fk::Ref(RefKind::Status),
        "status_category" => Fk::Category,
        "assignee" => Fk::Ref(RefKind::Assignee),
        "author" => Fk::Ref(RefKind::Author),
        "fixed_version" => Fk::Ref(RefKind::Version),
        "is_private" => Fk::Private,
        "start_date" => Fk::Date(START_FIELD),
        "due_date" => Fk::Date(DUE_FIELD),
        "created_at" => Fk::Day(CREATED_FIELD),
        "updated_at" => Fk::Day(UPDATED_FIELD),
        "estimated_hours" => Fk::Estimated,
        "spent_hours" => Fk::Spent,
        "overdue" => Fk::Overdue,
        "triage" => Fk::Triage,
        "triage_self_decided" => Fk::TriageSelf,
        _ => return None,
    })
}

/// Ops a custom field type takes in v2: v1's, plus `within` for dates.
pub fn cf_ops(ty: FieldType) -> Vec<&'static str> {
    let mut ops = operators(ty);
    if ty == FieldType::Date {
        ops.push("within");
    }
    ops
}

/// A `cf:<id>` (or `cf:<name>`) the caller may read: its index.
fn find_cf(fields: &[(FieldRow, Pred)], key: &str) -> Option<usize> {
    let rest = key.strip_prefix("cf:")?.trim();
    match rest.parse::<i32>() {
        Ok(id) => fields.iter().position(|(f, _)| f.id == id),
        Err(_) => {
            let l = rest.to_lowercase();
            fields.iter().position(|(f, _)| f.name.to_lowercase() == l)
        }
    }
}

// ---------------------------------------------------------------- resolve

/// A resolved `where`: the typed filter for this request and its normalized form.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub filter: Filter,
    pub normalized: Json,
}

struct Cx<'c> {
    conn: &'c mut PgConnection,
    caller: AuthContext,
    scope: &'c Scope,
    clock: Clock,
    fields: Vec<(FieldRow, Pred)>,
    /// The one project the request is limited to (for `next` and version names).
    project: Option<Uuid>,
    /// Values already resolved in this request (`<kind>#<id>` for a checked
    /// id, `<kind>:<name>` for a name), so a repeated value costs nothing.
    memo: std::collections::HashMap<String, Uuid>,
    /// Name lookups so far (see [`MAX_NAME_LOOKUPS`]).
    lookups: usize,
}

/// Resolves `where`. `project` is the single project the rest of the request
/// already limits to (the `project` parameter), if any; otherwise a
/// top-level `project eq` condition counts.
pub async fn resolve(
    conn: &mut PgConnection,
    caller: AuthContext,
    scope: &Scope,
    clock: Clock,
    input: &Json,
    project: Option<Uuid>,
) -> Result<Resolved> {
    let node = parse(input)?;
    let fields = readable_fields(&mut *conn, caller, scope).await?;
    let mut cx = Cx {
        conn,
        caller,
        scope,
        clock,
        fields,
        project,
        memo: std::collections::HashMap::new(),
        lookups: 0,
    };
    prefetch_ids(&mut cx, &node).await?;
    if cx.project.is_none() {
        cx.project = root_project(&mut cx, &node).await;
    }
    let (filter, normalized) = resolve_node(&mut cx, &node).await?;
    Ok(Resolved { filter, normalized })
}

/// Checks every id of the built-in reference conditions in one query per
/// kind and remembers the ones that exist (and that the caller may name),
/// so lists of ids cost one round trip instead of one per value.
async fn prefetch_ids(cx: &mut Cx<'_>, node: &Node) -> Result<()> {
    fn collect(node: &Node, out: &mut Vec<(RefKind, Uuid)>) {
        match node {
            Node::And(ps) | Node::Or(ps) => ps.iter().for_each(|p| collect(p, out)),
            Node::Cond {
                field,
                value: Some(v),
                ..
            } => {
                let Some(Fk::Ref(kind)) = builtin_kind(field.trim()) else {
                    return;
                };
                let one = |x: &Json, out: &mut Vec<(RefKind, Uuid)>| {
                    if let Some(id) = x.as_str().and_then(|s| Uuid::parse_str(s.trim()).ok()) {
                        out.push((kind, id));
                    }
                };
                match v {
                    Json::Array(xs) => xs.iter().for_each(|x| one(x, out)),
                    x => one(x, out),
                }
            }
            Node::Cond { .. } => {}
        }
    }
    let mut all = Vec::new();
    collect(node, &mut all);
    for kind in [
        RefKind::Project,
        RefKind::Tracker,
        RefKind::Priority,
        RefKind::Status,
        RefKind::Assignee,
        RefKind::Author,
        RefKind::Version,
    ] {
        let mut ids: Vec<Uuid> = all
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, id)| *id)
            .collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            continue;
        }
        let found: Vec<Uuid> = match kind {
            RefKind::Project => ids
                .into_iter()
                .filter(|id| cx.scope.project_ids().any(|p| p == *id))
                .collect(),
            RefKind::Version => {
                let rows: Vec<(Uuid, Uuid)> =
                    sqlx::query_as("SELECT id, project_id FROM versions WHERE id = ANY($1)")
                        .bind(&ids)
                        .fetch_all(&mut *cx.conn)
                        .await?;
                rows.into_iter()
                    .filter(|(_, p)| cx.scope.project_ids().any(|x| x == *p))
                    .map(|(id, _)| id)
                    .collect()
            }
            _ => {
                let sql = match kind {
                    RefKind::Tracker => "SELECT id FROM trackers WHERE id = ANY($1)",
                    RefKind::Priority => "SELECT id FROM issue_priorities WHERE id = ANY($1)",
                    RefKind::Status => "SELECT id FROM issue_statuses WHERE id = ANY($1)",
                    RefKind::Assignee => {
                        "SELECT id FROM principals WHERE id = ANY($1) AND kind IN ('user', 'group')"
                    }
                    _ => "SELECT id FROM principals WHERE id = ANY($1)",
                };
                sqlx::query_scalar(sql)
                    .bind(&ids)
                    .fetch_all(&mut *cx.conn)
                    .await?
            }
        };
        for id in found {
            cx.memo.insert(format!("{kind:?}#{id}"), id);
        }
    }
    Ok(())
}

/// Counts one name lookup; past [`MAX_NAME_LOOKUPS`] the filter is too complex.
fn note_lookup(cx: &mut Cx<'_>, path: &str) -> Result<()> {
    cx.lookups += 1;
    if cx.lookups > MAX_NAME_LOOKUPS {
        return Err(too_complex(
            path,
            format!(
                "at most {MAX_NAME_LOOKUPS} different names per where; use ids (resolve, /filter-fields)"
            ),
        ));
    }
    Ok(())
}

/// A top-level `project eq X` (or `in [X]`) condition's project, if it resolves.
async fn root_project(cx: &mut Cx<'_>, node: &Node) -> Option<Uuid> {
    let conds: Vec<&Node> = match node {
        Node::And(ps) => ps.iter().collect(),
        c @ Node::Cond { .. } => vec![c],
        Node::Or(_) => return None,
    };
    for c in conds {
        let Node::Cond {
            path,
            field,
            op,
            value: Some(v),
        } = c
        else {
            continue;
        };
        if field.trim() != "project" {
            continue;
        }
        let one = match (op, v) {
            (WhereOp::Eq, x) if !x.is_array() => x,
            (WhereOp::In, Json::Array(xs)) if xs.len() == 1 => &xs[0],
            _ => continue,
        };
        if let Ok((RefVal::Id(id), _)) = ref_value(cx, RefKind::Project, path, one).await {
            return Some(id);
        }
    }
    None
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

fn resolve_node<'a, 'c: 'a>(cx: &'a mut Cx<'c>, node: &'a Node) -> BoxFut<'a, (Filter, Json)> {
    Box::pin(async move {
        match node {
            Node::And(ps) | Node::Or(ps) => {
                let mut fs = Vec::with_capacity(ps.len());
                let mut js = Vec::with_capacity(ps.len());
                for p in ps {
                    let (f, j) = resolve_node(&mut *cx, p).await?;
                    fs.push(f);
                    js.push(j);
                }
                Ok(if matches!(node, Node::And(_)) {
                    (Filter::And(fs), json!({ "and": js }))
                } else {
                    (Filter::Or(fs), json!({ "or": js }))
                })
            }
            Node::Cond {
                path,
                field,
                op,
                value,
            } => resolve_cond(cx, path, field, *op, value.as_ref()).await,
        }
    })
}

/// A reference value before expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefVal {
    Id(Uuid),
    Me,
}

fn single<'v>(path: &str, op: WhereOp, v: Option<&'v Json>) -> Result<&'v Json> {
    match v {
        Some(x) if !x.is_array() => Ok(x),
        _ => Err(bad_value(
            &format!("{path}.value"),
            format!("{path}: {} needs a single value", op.as_str()),
        )),
    }
}

fn list<'v>(path: &str, op: WhereOp, v: Option<&'v Json>) -> Result<&'v [Json]> {
    match v {
        Some(Json::Array(xs)) if !xs.is_empty() => Ok(xs),
        _ => Err(bad_value(
            &format!("{path}.value"),
            format!("{path}: {} needs a non-empty array", op.as_str()),
        )),
    }
}

fn pair<'v>(path: &str, v: Option<&'v Json>) -> Result<(&'v Json, &'v Json)> {
    match v {
        Some(Json::Array(xs)) if xs.len() == 2 && !(xs[0].is_null() && xs[1].is_null()) => {
            Ok((&xs[0], &xs[1]))
        }
        _ => Err(bad_value(
            &format!("{path}.value"),
            format!("{path}: between needs [min, max] (either may be null, not both)"),
        )),
    }
}

fn cond_json(field: &str, op: WhereOp, value: Option<Json>) -> Json {
    match value {
        Some(v) => json!({"field": field, "op": op.as_str(), "value": v}),
        None => json!({"field": field, "op": op.as_str()}),
    }
}

fn built(path: &str, r: Result<Filter>) -> Result<Filter> {
    r.map_err(|e| bad_value(path, format!("{path}: {e}")))
}

async fn resolve_cond(
    cx: &mut Cx<'_>,
    path: &str,
    field: &str,
    op: WhereOp,
    value: Option<&Json>,
) -> Result<(Filter, Json)> {
    let key = field.trim();
    let fpath = format!("{path}.field");
    let kind = match builtin_kind(key) {
        Some(k) => k,
        None => match find_cf(&cx.fields, key) {
            Some(i) => Fk::Cf(i),
            None => {
                return Err(ferr(
                    &fpath,
                    "unknown_field",
                    format!(
                        "{fpath}: unknown field {key:?} (built-in: {}; custom fields: cf:<id>)",
                        BUILTIN_FIELDS
                            .iter()
                            .map(|b| b.0)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        },
    };
    let (canonical, ops): (String, Vec<&'static str>) = match kind {
        Fk::Cf(i) => {
            let (row, _) = &cx.fields[i];
            (format!("cf:{}", row.id), cf_ops(row.field_type()?))
        }
        _ => {
            let b = BUILTIN_FIELDS.iter().find(|b| b.0 == key).unwrap();
            (b.0.to_string(), b.2.iter().map(|o| o.as_str()).collect())
        }
    };
    // Contract 05 §4 / R-VD7: agents get no spent_hours.
    if kind == Fk::Spent && cx.caller.user_id().is_none() {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("{fpath}: spent_hours is not available to agents"),
        )
        .with_details(json!({"path": fpath, "reason": "unavailable_to_agent"}))
        .into());
    }
    if !ops.contains(&op.as_str()) {
        return Err(ferr(
            &format!("{path}.op"),
            "unsupported_op",
            format!(
                "{path}: {canonical} takes {} (not {})",
                ops.join(", "),
                op.as_str()
            ),
        ));
    }
    if matches!(op, WhereOp::IsNull | WhereOp::NotNull) {
        if value.is_some() {
            return Err(bad_value(
                &format!("{path}.value"),
                format!("{path}: is_null / not_null take no value"),
            ));
        }
        let test = if op == WhereOp::IsNull {
            Test::IsNull
        } else {
            Test::NotNull
        };
        let f = match kind {
            Fk::Ref(r) => builtin_cond(r.id(), test),
            Fk::Date(id) => builtin_cond(id, test),
            Fk::Estimated => builtin_cond(ESTIMATED_FIELD, test),
            Fk::Cf(i) => cf_cond(&cx.fields[i], test),
            _ => unreachable!("ops table"),
        };
        return Ok((built(path, f)?, cond_json(&canonical, op, None)));
    }
    match kind {
        Fk::Ref(r) => resolve_ref(cx, path, &canonical, r, op, value).await,
        Fk::Category => {
            let vals: Vec<&Json> = match op {
                WhereOp::Eq => vec![single(path, op, value)?],
                _ => list(path, op, value)?.iter().collect(),
            };
            let mut cats: Vec<String> = Vec::new();
            for (i, v) in vals.iter().enumerate() {
                let vpath = format!("{path}.value");
                let s = v.as_str().map(str::trim).unwrap_or("");
                let add: &[&str] = match s {
                    "open" => &["todo", "doing"],
                    "todo" => &["todo"],
                    "doing" => &["doing"],
                    "done" => &["done"],
                    _ => {
                        return Err(bad_value(
                            &if op == WhereOp::Eq {
                                vpath
                            } else {
                                format!("{vpath}[{i}]")
                            },
                            format!(
                                "{path}: status_category is todo, doing, done or open, got {v}"
                            ),
                        ));
                    }
                };
                for c in add {
                    if !cats.iter().any(|x| x == c) {
                        cats.push(c.to_string());
                    }
                }
            }
            // "open" is expanded on input only: eq open → in [todo, doing].
            let (nop, test) = if cats.len() == 1 && op == WhereOp::Eq {
                (
                    WhereOp::Eq,
                    Test::Cmp(Cmp::Eq, Value::Text(cats[0].clone())),
                )
            } else {
                (
                    WhereOp::In,
                    Test::In(cats.iter().cloned().map(Value::Text).collect()),
                )
            };
            let norm = if nop == WhereOp::Eq {
                json!(cats[0])
            } else {
                json!(cats)
            };
            Ok((
                built(path, builtin_cond(CATEGORY_FIELD, test))?,
                cond_json(&canonical, nop, Some(norm)),
            ))
        }
        Fk::Triage => {
            let vals: Vec<&Json> = match op {
                WhereOp::Eq => vec![single(path, op, value)?],
                _ => list(path, op, value)?.iter().collect(),
            };
            let mut states: Vec<String> = Vec::new();
            for (i, v) in vals.iter().enumerate() {
                let s = v.as_str().map(str::trim).unwrap_or("");
                if !TRIAGE_VALUES.contains(&s) {
                    let vpath = format!("{path}.value");
                    return Err(bad_value(
                        &if op == WhereOp::Eq {
                            vpath
                        } else {
                            format!("{vpath}[{i}]")
                        },
                        format!(
                            "{path}: triage is pending, snoozed, accepted, declined, duplicate or none, got {v}"
                        ),
                    ));
                }
                if !states.iter().any(|x| x == s) {
                    states.push(s.to_string());
                }
            }
            let (test, norm) = if op == WhereOp::Eq {
                (
                    Test::Cmp(Cmp::Eq, Value::Text(states[0].clone())),
                    json!(states[0]),
                )
            } else {
                (
                    Test::In(states.iter().cloned().map(Value::Text).collect()),
                    json!(states),
                )
            };
            Ok((
                built(path, builtin_cond(TRIAGE_FIELD, test))?,
                cond_json(&canonical, op, Some(norm)),
            ))
        }
        Fk::TriageSelf => {
            let v = single(path, op, value)?;
            let b = bool_of(v).ok_or_else(|| {
                bad_value(
                    &format!("{path}.value"),
                    format!("{path}: expected true or false, got {v}"),
                )
            })?;
            Ok((
                built(
                    path,
                    builtin_cond(TRIAGE_SELF_FIELD, Test::Cmp(Cmp::Eq, Value::Bool(b))),
                )?,
                cond_json(&canonical, op, Some(json!(b))),
            ))
        }
        Fk::Private | Fk::Overdue => {
            let v = single(path, op, value)?;
            let b = bool_of(v).ok_or_else(|| {
                bad_value(
                    &format!("{path}.value"),
                    format!("{path}: expected true or false, got {v}"),
                )
            })?;
            let f = if kind == Fk::Private {
                builtin_cond(PRIVATE_FIELD, Test::Cmp(Cmp::Eq, Value::Bool(b)))
            } else {
                overdue(cx.clock.today, b)
            };
            Ok((built(path, f)?, cond_json(&canonical, op, Some(json!(b)))))
        }
        Fk::Date(id) | Fk::Day(id) => {
            let (test, norm) = date_test(cx.clock, path, op, value)?;
            Ok((
                built(path, builtin_cond(id, test))?,
                cond_json(&canonical, op, Some(norm)),
            ))
        }
        Fk::Estimated | Fk::Spent => {
            let num = |v: &Json, p: &str| -> Result<(Value, Json)> {
                let s = match v {
                    Json::String(s) => s.trim().to_string(),
                    Json::Number(n) => n.to_string(),
                    _ => String::new(),
                };
                match canonical_number(&s, false) {
                    Some(c) => Ok((Value::Num(c.parse::<f64>().unwrap_or(0.0)), json!(c))),
                    None => Err(bad_value(
                        p,
                        format!("{p}: expected a decimal string such as \"1.5\", got {v}"),
                    )),
                }
            };
            let vpath = format!("{path}.value");
            let (test, norm) = match op {
                WhereOp::Between => {
                    let (lo, hi) = pair(path, value)?;
                    let end = |x: &Json, i: usize| -> Result<(Option<Value>, Json)> {
                        if x.is_null() {
                            Ok((None, Json::Null))
                        } else {
                            let (v, j) = num(x, &format!("{vpath}[{i}]"))?;
                            Ok((Some(v), j))
                        }
                    };
                    let ((min, a), (max, b)) = (end(lo, 0)?, end(hi, 1)?);
                    (Test::Range { min, max }, json!([a, b]))
                }
                _ => {
                    let (v, j) = num(single(path, op, value)?, &vpath)?;
                    (Test::Cmp(cmp_of(op), v), j)
                }
            };
            let f = match (kind, cx.caller.user_id()) {
                (Fk::Spent, Some(me)) => spent_cond(me, test),
                _ => builtin_cond(ESTIMATED_FIELD, test),
            };
            Ok((built(path, f)?, cond_json(&canonical, op, Some(norm))))
        }
        Fk::Cf(i) => resolve_cf(cx, path, &canonical, i, op, value).await,
    }
}

fn bool_of(v: &Json) -> Option<bool> {
    match v {
        Json::Bool(b) => Some(*b),
        Json::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Json::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn cmp_of(op: WhereOp) -> Cmp {
    match op {
        WhereOp::Ne => Cmp::Ne,
        WhereOp::Lt => Cmp::Lt,
        WhereOp::Le => Cmp::Le,
        WhereOp::Gt => Cmp::Gt,
        WhereOp::Ge => Cmp::Ge,
        _ => Cmp::Eq,
    }
}

/// `overdue` (02 §17.12, §17.15): due before today and not done; `false` is
/// its complement (no due date, due today or later, or done).
pub fn overdue(today: NaiveDate, yes: bool) -> Result<Filter> {
    let done = || Value::Text("done".into());
    Ok(if yes {
        Filter::And(vec![
            builtin_cond(DUE_FIELD, Test::Cmp(Cmp::Lt, Value::Date(today)))?,
            builtin_cond(
                CATEGORY_FIELD,
                Test::In(vec![
                    Value::Text("todo".into()),
                    Value::Text("doing".into()),
                ]),
            )?,
        ])
    } else {
        Filter::Or(vec![
            builtin_cond(DUE_FIELD, Test::IsNull)?,
            builtin_cond(DUE_FIELD, Test::Cmp(Cmp::Ge, Value::Date(today)))?,
            builtin_cond(CATEGORY_FIELD, Test::Cmp(Cmp::Eq, done()))?,
        ])
    })
}

/// A date (`"YYYY-MM-DD"` or `{"rel": "today", "offset": n}`): its value
/// today and its normalized form.
fn date_value(clock: Clock, path: &str, v: &Json) -> Result<(NaiveDate, Json)> {
    let bad = || {
        bad_value(
            path,
            format!(
                "{path}: expected \"YYYY-MM-DD\" or {{\"rel\": \"today\", \"offset\": n}} (n in -{MAX_REL_OFFSET}..{MAX_REL_OFFSET}), got {v}"
            ),
        )
    };
    match v {
        Json::String(s) => NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .ok()
            .filter(|d| (MIN_YEAR..=MAX_YEAR).contains(&d.year()))
            .map(|d| (d, json!(d.to_string())))
            .ok_or_else(bad),
        Json::Object(m) => {
            if m.get("rel").and_then(Json::as_str) != Some("today")
                || m.keys().any(|k| k != "rel" && k != "offset")
            {
                return Err(bad());
            }
            let offset = match m.get("offset") {
                None | Some(Json::Null) => 0,
                Some(o) => o.as_i64().ok_or_else(bad)?,
            };
            if offset.abs() > MAX_REL_OFFSET {
                return Err(bad());
            }
            Ok((
                add_days(clock.today, offset),
                json!({"rel": "today", "offset": offset}),
            ))
        }
        _ => Err(bad()),
    }
}

/// A `within` value: its inclusive range today and its normalized form.
fn within_value(clock: Clock, path: &str, v: Option<&Json>) -> Result<(Test, Json)> {
    let p = format!("{path}.value");
    let bad = |x: String| {
        bad_value(
            &p,
            format!(
                "{p}: within takes {{\"range\": {}, \"n\"?: 1..{MAX_REL_N}}}{x}",
                RANGES.join("|")
            ),
        )
    };
    let Some(Json::Object(m)) = v else {
        return Err(bad(String::new()));
    };
    if m.keys().any(|k| k != "range" && k != "n") {
        return Err(bad(String::new()));
    }
    let range = m.get("range").and_then(Json::as_str).unwrap_or("");
    if !RANGES.contains(&range) {
        return Err(bad(format!(", got range {range:?}")));
    }
    let takes_n = range.ends_with("_n_days");
    let n = match m.get("n") {
        None | Some(Json::Null) => None,
        Some(x) => Some(
            x.as_i64()
                .filter(|n| (1..=MAX_REL_N).contains(n))
                .ok_or_else(|| bad(format!(", got n {x}")))?,
        ),
    };
    if takes_n != n.is_some() {
        return Err(bad(if takes_n {
            format!(": {range} needs n")
        } else {
            format!(": {range} takes no n")
        }));
    }
    let (lo, hi) = range_bounds(clock, range, n).ok_or_else(|| bad(String::new()))?;
    let norm = match n {
        Some(n) => json!({"range": range, "n": n}),
        None => json!({"range": range}),
    };
    Ok((
        Test::Range {
            min: Some(Value::Date(lo)),
            max: Some(Value::Date(hi)),
        },
        norm,
    ))
}

fn date_test(clock: Clock, path: &str, op: WhereOp, value: Option<&Json>) -> Result<(Test, Json)> {
    let vpath = format!("{path}.value");
    match op {
        WhereOp::Within => within_value(clock, path, value),
        WhereOp::Between => {
            let (lo, hi) = pair(path, value)?;
            let end = |x: &Json, i: usize| -> Result<(Option<Value>, Json)> {
                if x.is_null() {
                    Ok((None, Json::Null))
                } else {
                    let (d, j) = date_value(clock, &format!("{vpath}[{i}]"), x)?;
                    Ok((Some(Value::Date(d)), j))
                }
            };
            let ((min, a), (max, b)) = (end(lo, 0)?, end(hi, 1)?);
            Ok((Test::Range { min, max }, json!([a, b])))
        }
        _ => {
            let (d, j) = date_value(clock, &vpath, single(path, op, value)?)?;
            Ok((Test::Cmp(cmp_of(op), Value::Date(d)), j))
        }
    }
}

/// Is `id` a row of a reference table the caller may name? Projects: the
/// visible ones; versions: those of visible projects; others: existence.
async fn ref_exists(cx: &mut Cx<'_>, kind: RefKind, id: Uuid) -> Result<bool> {
    let sql = match kind {
        RefKind::Project => return Ok(cx.scope.project_ids().any(|p| p == id)),
        RefKind::Version => {
            let p: Option<Uuid> =
                sqlx::query_scalar("SELECT project_id FROM versions WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *cx.conn)
                    .await?;
            return Ok(p.is_some_and(|p| cx.scope.project_ids().any(|x| x == p)));
        }
        RefKind::Tracker => "SELECT EXISTS (SELECT 1 FROM trackers WHERE id = $1)",
        RefKind::Priority => "SELECT EXISTS (SELECT 1 FROM issue_priorities WHERE id = $1)",
        RefKind::Status => "SELECT EXISTS (SELECT 1 FROM issue_statuses WHERE id = $1)",
        RefKind::Assignee => {
            "SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1 AND kind IN ('user', 'group'))"
        }
        RefKind::Author => "SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1)",
    };
    Ok(sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&mut *cx.conn)
        .await?)
}

fn me_unavailable(path: &str) -> anyhow::Error {
    ferr(
        path,
        "me_unavailable",
        format!(
            "{path}: \"me\" needs a signed-in person (agents have no \"me\"; name the person instead)"
        ),
    )
}

/// One reference value: `{"me": true}`, `{"next": true}`, an id or a name.
/// `next` comes back as the version's id; its normalized form stays dynamic.
async fn ref_value(cx: &mut Cx<'_>, kind: RefKind, path: &str, v: &Json) -> Result<(RefVal, Json)> {
    let is_flag = |name: &str| {
        v.as_object()
            .is_some_and(|m| m.len() == 1 && m.get(name) == Some(&Json::Bool(true)))
    };
    let person = matches!(kind, RefKind::Assignee | RefKind::Author);
    if is_flag("me")
        || (person
            && v.as_str()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("me")))
    {
        if !person {
            return Err(bad_value(
                path,
                format!("{path}: \"me\" is only for assignee and author"),
            ));
        }
        if cx.caller.user_id().is_none() {
            return Err(me_unavailable(path));
        }
        return Ok((RefVal::Me, json!({"me": true})));
    }
    if is_flag("next") {
        if kind != RefKind::Version {
            return Err(bad_value(
                path,
                format!("{path}: \"next\" is only for fixed_version"),
            ));
        }
        let Some(project) = cx.project else {
            return Err(ferr(
                path,
                "project_scope_required",
                format!(
                    "{path}: next needs exactly one project (add project eq …, or use the version id)"
                ),
            ));
        };
        let key = format!("{kind:?}!next");
        let id = match cx.memo.get(&key) {
            Some(id) => *id,
            None => {
                let id = crate::versions::resolve_for_search(&mut *cx.conn, project, "next")
                    .await
                    .map_err(|e| remap(e, path, v))?;
                cx.memo.insert(key, id);
                id
            }
        };
        return Ok((RefVal::Id(id), json!({"next": true})));
    }
    let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(bad_value(
            path,
            format!("{path}: expected an id, a name or {{\"me\": true}}, got {v}"),
        ));
    };
    if let Ok(id) = Uuid::parse_str(s) {
        let key = format!("{kind:?}#{id}");
        if cx.memo.contains_key(&key) {
            return Ok((RefVal::Id(id), json!(id)));
        }
        return if ref_exists(cx, kind, id).await? {
            cx.memo.insert(key, id);
            Ok((RefVal::Id(id), json!(id)))
        } else {
            Err(unknown_value(path, v))
        };
    }
    let key = format!("{kind:?}:{s}");
    if let Some(id) = cx.memo.get(&key) {
        return Ok((RefVal::Id(*id), json!(id)));
    }
    note_lookup(cx, path)?;
    let id = match kind {
        RefKind::Priority => {
            let ids: Vec<Uuid> =
                sqlx::query_scalar("SELECT id FROM issue_priorities WHERE lower(name) = lower($1)")
                    .bind(s)
                    .fetch_all(&mut *cx.conn)
                    .await?;
            match ids.as_slice() {
                [one] => *one,
                _ => return Err(unknown_value(path, v)),
            }
        }
        RefKind::Version => {
            let Some(project) = cx.project else {
                return Err(ferr(
                    path,
                    "project_scope_required",
                    format!(
                        "{path}: a version name needs exactly one project (or use the version id)"
                    ),
                ));
            };
            crate::versions::resolve_for_search(&mut *cx.conn, project, s)
                .await
                .map_err(|e| remap(e, path, v))?
        }
        _ => {
            let k = match kind {
                RefKind::Project => "project",
                RefKind::Tracker => "tracker",
                RefKind::Status => "status",
                _ => "principal",
            };
            resolve_one(&mut *cx.conn, cx.caller, k, s)
                .await
                .map_err(|e| remap(e, path, v))?
        }
    };
    cx.memo.insert(key, id);
    Ok((RefVal::Id(id), json!(id)))
}

async fn resolve_ref(
    cx: &mut Cx<'_>,
    path: &str,
    canonical: &str,
    kind: RefKind,
    op: WhereOp,
    value: Option<&Json>,
) -> Result<(Filter, Json)> {
    let vpath = format!("{path}.value");
    let (vals, many): (Vec<(String, &Json)>, bool) = match op {
        WhereOp::Eq | WhereOp::Ne => (vec![(vpath.clone(), single(path, op, value)?)], false),
        _ => (
            list(path, op, value)?
                .iter()
                .enumerate()
                .map(|(i, x)| (format!("{vpath}[{i}]"), x))
                .collect(),
            true,
        ),
    };
    let mut ids: Vec<Uuid> = Vec::new();
    let mut mes = false;
    let mut norm = Vec::with_capacity(vals.len());
    for (p, x) in vals {
        let (r, j) = ref_value(cx, kind, &p, x).await?;
        norm.push(j);
        match r {
            RefVal::Id(id) => ids.push(id),
            RefVal::Me => mes = true,
        }
    }
    let me = cx.caller.user_id();
    // `me` (Redmine): the assignee side also matches my groups; the author
    // side also matches what an agent wrote on my behalf (02 §16.2).
    let mut all = ids.clone();
    if mes {
        match kind {
            RefKind::Assignee => all.extend(cx.scope.principal_ids.iter().copied()),
            _ => all.extend(me),
        }
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|u| seen.insert(*u));
    let refs = |xs: &[Uuid]| xs.iter().copied().map(Value::Ref).collect::<Vec<_>>();
    let positive = matches!(op, WhereOp::Eq | WhereOp::In | WhereOp::EverIn);
    let test = |xs: &[Uuid]| match op {
        WhereOp::Eq if xs.len() == 1 => Test::Cmp(Cmp::Eq, Value::Ref(xs[0])),
        WhereOp::Ne if xs.len() == 1 => Test::Cmp(Cmp::Ne, Value::Ref(xs[0])),
        WhereOp::Eq | WhereOp::In => Test::In(refs(xs)),
        WhereOp::Ne | WhereOp::NotIn => Test::NotIn(refs(xs)),
        _ => Test::EverIn(refs(xs)),
    };
    let main = built(path, builtin_cond(kind.id(), test(&all)))?;
    let filter = match (kind, mes, me) {
        (RefKind::Author, true, Some(me)) => {
            let behalf = built(path, builtin_cond(ON_BEHALF_FIELD, test(&[me])))?;
            if positive {
                Filter::Or(vec![main, behalf])
            } else {
                Filter::And(vec![main, behalf])
            }
        }
        _ => main,
    };
    let nv = if many { json!(norm) } else { norm.remove(0) };
    Ok((filter, cond_json(canonical, op, Some(nv))))
}

fn cf_cond(field: &(FieldRow, Pred), test: Test) -> Result<Filter> {
    let (row, gate) = field;
    Ok(Filter::cond_where(
        Field {
            id: row.id,
            ty: row.field_type()?,
        },
        test,
        gate.clone(),
    )?)
}

/// A custom field value: `{"<type>": v}` (02 §6.1) or a plain value as in v1.
async fn cf_value(cx: &mut Cx<'_>, i: usize, path: &str, v: &Json) -> Result<(Value, Json)> {
    let (row, _) = &cx.fields[i];
    let (fid, ftype, name) = (row.id, row.field_type()?, row.name.clone());
    let tname = ftype.as_str();
    let bad = |what: &str| {
        bad_value(
            path,
            format!("{path}: field {name:?} ({tname}): {what}, got {v}"),
        )
    };
    // Typed form: the type must be the field's.
    let (raw, typed) = match v {
        Json::Object(m) if m.len() == 1 && !m.contains_key("me") && !m.contains_key("rel") => {
            let (k, x) = m.iter().next().unwrap();
            if k != tname {
                return Err(bad(&format!("expected {{\"{tname}\": …}}")));
            }
            (x, true)
        }
        _ => (v, false),
    };
    match ftype {
        FieldType::Int | FieldType::Float => {
            let s = match raw {
                Json::String(s) => s.trim().to_string(),
                Json::Number(n) => n.to_string(),
                _ => String::new(),
            };
            let c = canonical_number(&s, ftype == FieldType::Int)
                .ok_or_else(|| bad("expected a number"))?;
            Ok((
                Value::Num(c.parse::<f64>().map_err(|_| bad("expected a number"))?),
                json!({tname: c}),
            ))
        }
        FieldType::Date => {
            let (d, j) = date_value(cx.clock, path, raw)?;
            Ok((
                Value::Date(d),
                if j.is_string() { json!({tname: j}) } else { j },
            ))
        }
        FieldType::Bool => {
            let b = bool_of(raw).ok_or_else(|| bad("expected true or false"))?;
            Ok((Value::Bool(b), json!({tname: b})))
        }
        FieldType::String | FieldType::Text | FieldType::Link => {
            let s = raw.as_str().ok_or_else(|| bad("expected a string"))?;
            Ok((Value::Text(s.to_string()), json!({tname: s})))
        }
        FieldType::List => {
            let s = raw
                .as_str()
                .map(str::trim)
                .ok_or_else(|| bad("expected an option id or label"))?;
            let key = format!("cf{fid}:{typed}:{s}");
            let id = match cx.memo.get(&key) {
                Some(id) => Some(*id),
                None => match Uuid::parse_str(s) {
                    Ok(id) => {
                        let ok: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM field_options WHERE id = $1 AND field_id = $2)",
                        )
                        .bind(id)
                        .bind(fid)
                        .fetch_one(&mut *cx.conn)
                        .await?;
                        ok.then_some(id)
                    }
                    Err(_) if !typed => {
                        note_lookup(cx, path)?;
                        custom_fields::option_id(&mut *cx.conn, fid, s).await?
                    }
                    Err(_) => None,
                },
            };
            let id = id.ok_or_else(|| unknown_value(path, v))?;
            cx.memo.insert(key, id);
            Ok((Value::Ref(id), json!({tname: id})))
        }
        FieldType::User => {
            let is_me = v
                .as_object()
                .is_some_and(|m| m.len() == 1 && m.get("me") == Some(&Json::Bool(true)))
                || (!typed
                    && raw
                        .as_str()
                        .is_some_and(|s| s.trim().eq_ignore_ascii_case("me")));
            if is_me {
                let me = cx.caller.user_id().ok_or_else(|| me_unavailable(path))?;
                return Ok((Value::Ref(me), json!({"me": true})));
            }
            let s = raw
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("expected a user id or login"))?;
            let key = format!("cfuser:{s}");
            let id = match (cx.memo.get(&key), Uuid::parse_str(s)) {
                (Some(id), _) => *id,
                (None, Ok(id)) => {
                    let ok: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1 AND kind IN ('user', 'group'))",
                    )
                    .bind(id)
                    .fetch_one(&mut *cx.conn)
                    .await?;
                    if !ok {
                        return Err(unknown_value(path, v));
                    }
                    id
                }
                (None, Err(_)) => {
                    note_lookup(cx, path)?;
                    resolve_one(&mut *cx.conn, cx.caller, "principal", s)
                        .await
                        .map_err(|e| remap(e, path, v))?
                }
            };
            cx.memo.insert(key, id);
            Ok((Value::Ref(id), json!({tname: id})))
        }
    }
}

async fn resolve_cf(
    cx: &mut Cx<'_>,
    path: &str,
    canonical: &str,
    i: usize,
    op: WhereOp,
    value: Option<&Json>,
) -> Result<(Filter, Json)> {
    let vpath = format!("{path}.value");
    let (test, norm) = match op {
        WhereOp::Within => within_value(cx.clock, path, value)?,
        WhereOp::Between => {
            let (lo, hi) = pair(path, value)?;
            let mut ends = Vec::with_capacity(2);
            for (k, x) in [lo, hi].into_iter().enumerate() {
                ends.push(if x.is_null() {
                    (None, Json::Null)
                } else {
                    let (v, j) = cf_value(cx, i, &format!("{vpath}[{k}]"), x).await?;
                    (Some(v), j)
                });
            }
            let (max, b) = ends.pop().unwrap();
            let (min, a) = ends.pop().unwrap();
            (Test::Range { min, max }, json!([a, b]))
        }
        WhereOp::In => {
            let xs = list(path, op, value)?;
            let mut vs = Vec::with_capacity(xs.len());
            let mut js = Vec::with_capacity(xs.len());
            for (k, x) in xs.iter().enumerate() {
                let (v, j) = cf_value(cx, i, &format!("{vpath}[{k}]"), x).await?;
                vs.push(v);
                js.push(j);
            }
            (Test::In(vs), json!(js))
        }
        _ => {
            let (v, j) = cf_value(cx, i, &vpath, single(path, op, value)?).await?;
            (Test::Cmp(cmp_of(op), v), j)
        }
    };
    Ok((
        built(path, cf_cond(&cx.fields[i], test))?,
        cond_json(canonical, op, Some(norm)),
    ))
}

// ---------------------------------------------------------------- sort

/// Built-in sort keys (design 05 §3.2) besides `cf:<id>`.
pub const SORTABLE: &[&str] = &[
    "updated_at",
    "created_at",
    "due_date",
    "start_date",
    "priority",
    "status",
    "key",
    "estimated_hours",
];

/// Custom field types that sort (list: by option position).
pub fn cf_sortable(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Int | FieldType::Float | FieldType::Date | FieldType::String | FieldType::List
    )
}

/// One ORDER BY column over the search query's aliases (`i` issues, `k`
/// current key, `p` project, `s` status, `pr` priority).
#[derive(Debug, Clone)]
pub struct SortCol {
    /// SQL over the aliases; for a custom field, the value subquery.
    sql: String,
    /// Custom fields: the value counts only where the field applies and is
    /// readable (else it sorts as missing, like a filter outside its gate).
    gate: Option<Pred>,
    /// The SQL type, to compare against cursor values.
    pub cast: &'static str,
    pub nullable: bool,
    pub desc: bool,
}

impl SortCol {
    fn plain(sql: &str, cast: &'static str, nullable: bool, desc: bool) -> SortCol {
        SortCol {
            sql: sql.into(),
            gate: None,
            cast,
            nullable,
            desc,
        }
    }

    /// An ascending column over the search aliases (views: the group and
    /// swimlane order), counted only where `gate` holds when one is given.
    pub(crate) fn lead(
        sql: impl Into<String>,
        gate: Option<Pred>,
        cast: &'static str,
        nullable: bool,
    ) -> SortCol {
        SortCol {
            sql: sql.into(),
            gate: gate.filter(|g| *g != Pred::True),
            cast,
            nullable,
            desc: false,
        }
    }

    /// Appends the column's expression.
    pub fn push(&self, qb: &mut QueryBuilder<Postgres>) {
        match &self.gate {
            None => {
                qb.push(&self.sql);
            }
            Some(g) => {
                qb.push("(CASE WHEN ");
                push_pred(qb, "i", g);
                qb.push(format!(" THEN {} END)", self.sql));
            }
        }
    }
}

fn sort_err(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path, "allowed": SORTABLE, "custom": "cf:<id>"}))
        .into()
}

/// Resolves `sort` into ORDER BY columns (without the final `id`) and its
/// normalized form. Unknown fields and custom fields the caller cannot read
/// give the same error.
pub fn resolve_sort(
    fields: &[(FieldRow, Pred)],
    input: &[SortInput],
) -> Result<(Vec<SortCol>, Json)> {
    if input.is_empty() || input.len() > MAX_SORT {
        return Err(sort_err("sort", format!("sort takes 1 to {MAX_SORT} keys")));
    }
    let mut cols = Vec::new();
    let mut norm = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (n, s) in input.iter().enumerate() {
        let path = format!("sort[{n}].field");
        let key = s.field.trim();
        let desc = s.dir == SortDir::Desc;
        let canonical = match key {
            "updated_at" => {
                cols.push(SortCol::plain("i.updated_at", "timestamptz", false, desc));
                key.to_string()
            }
            "created_at" => {
                cols.push(SortCol::plain("i.created_at", "timestamptz", false, desc));
                key.to_string()
            }
            "due_date" | "start_date" => {
                cols.push(SortCol::plain(&format!("i.{key}"), "date", true, desc));
                key.to_string()
            }
            "priority" => {
                cols.push(SortCol::plain("pr.position", "integer", true, desc));
                key.to_string()
            }
            "status" => {
                cols.push(SortCol::plain("s.position", "integer", false, desc));
                key.to_string()
            }
            "key" => {
                cols.push(SortCol::plain("p.key", "text", false, desc));
                cols.push(SortCol::plain("k.number", "integer", false, desc));
                key.to_string()
            }
            "estimated_hours" => {
                cols.push(SortCol::plain("i.estimated_hours", "numeric", true, desc));
                key.to_string()
            }
            _ => {
                let unknown = || sort_err(&path, format!("{path}: cannot sort by {key:?}"));
                let i = find_cf(fields, key).ok_or_else(unknown)?;
                let (row, gate) = &fields[i];
                let ty = row.field_type()?;
                if !cf_sortable(ty) {
                    return Err(unknown());
                }
                let (inner, cast) = match ty {
                    FieldType::List => (
                        format!(
                            "(SELECT fo.position FROM issue_cf cf JOIN field_options fo ON fo.id = cf.v_ref \
                             WHERE cf.issue_id = i.id AND cf.field_id = {})",
                            row.id
                        ),
                        "integer",
                    ),
                    FieldType::Date => (
                        format!(
                            "(SELECT cf.v_date FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {})",
                            row.id
                        ),
                        "date",
                    ),
                    FieldType::String => (
                        format!(
                            "(SELECT cf.v_text FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {})",
                            row.id
                        ),
                        "text",
                    ),
                    _ => (
                        format!(
                            "(SELECT cf.v_num FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {})",
                            row.id
                        ),
                        "numeric",
                    ),
                };
                cols.push(SortCol {
                    sql: inner,
                    gate: (*gate != Pred::True).then(|| gate.clone()),
                    cast,
                    nullable: true,
                    desc,
                });
                format!("cf:{}", row.id)
            }
        };
        if seen.contains(&canonical) {
            return Err(sort_err(
                &path,
                format!("{path}: {canonical} is listed twice"),
            ));
        }
        seen.push(canonical.clone());
        norm.push(json!({"field": canonical, "dir": if desc { "desc" } else { "asc" }}));
    }
    Ok((cols, Json::Array(norm)))
}

// ---------------------------------------------------------------- filter-fields

/// What can be filtered and sorted (`GET /filter-fields`, MCP `describe`
/// scope `filter_fields`; design 05 §3.2).
#[derive(Debug, Serialize, JsonSchema)]
pub struct FilterFields {
    /// The organization's date (R10), YYYY-MM-DD: what relative dates mean now.
    pub today: String,
    /// First day of `this_week`: 1 = Monday … 7 = Sunday.
    pub week_start: u32,
    pub fields: Vec<FilterFieldInfo>,
    /// `sort` keys: the built-in ones and sortable custom fields.
    pub sortable: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FilterFieldInfo {
    /// The `field` of a `where` condition.
    pub field: String,
    /// ref | principal | enum | bool | date | datetime | number, or a custom
    /// field type (int, float, date, string, text, list, user, bool, link).
    #[serde(rename = "type")]
    pub ftype: String,
    pub label: String,
    pub ops: Vec<&'static str>,
    /// Small option sets, listed here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FilterOption>>,
    /// Larger sets: where to look values up (`GET …/assignees`, `GET
    /// …/versions`, or `POST /api/v1/resolve` across projects).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_source: Option<String>,
    /// Dynamic values besides ids: `me` ({"me": true}), `next`
    /// ({"next": true}, one project only), `rel` ({"rel": "today",
    /// "offset": n}), `range` (within).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dynamic: Vec<&'static str>,
    /// Custom fields: true for a field with `visible = false` (only readers
    /// see it; it cannot be used in project views).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restricted: Option<bool>,
    /// Custom fields: moves when the type, unit or options change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_version: Option<i32>,
}

#[derive(Debug, Serialize, JsonSchema, sqlx::FromRow)]
pub struct FilterOption {
    /// The value to send (an id, or the enum value).
    pub id: String,
    pub label: String,
    /// Projects: the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Statuses: todo | doing | done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Priorities and list options: inactive ones still match old values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// Labels of the built-in fields (zh-TW, the SPA's language).
pub(crate) fn label(field: &str) -> &'static str {
    match field {
        "project" => "專案",
        "tracker" => "類型",
        "status" => "狀態",
        "status_category" => "狀態分類",
        "priority" => "優先權",
        "assignee" => "指派",
        "author" => "作者",
        "fixed_version" => "里程碑",
        "is_private" => "私有",
        "start_date" => "開始日期",
        "due_date" => "到期日",
        "created_at" => "建立時間",
        "updated_at" => "更新時間",
        "estimated_hours" => "預估工時",
        "spent_hours" => "已記工時",
        "overdue" => "逾期",
        "triage" => "Triage",
        "triage_self_decided" => "自建自分流",
        _ => "",
    }
}

/// Could `gate` hold for some issue of `project` (any tracker)?
fn gate_allows(gate: &Pred, project: Uuid) -> bool {
    match gate {
        Pred::True => true,
        Pred::False => false,
        Pred::And(ps) => ps.iter().all(|p| gate_allows(p, project)),
        Pred::Or(ps) => ps.iter().any(|p| gate_allows(p, project)),
        Pred::ProjectIs(p) => *p == project,
        Pred::TrackerIn(ts) => !ts.is_empty(),
        _ => true,
    }
}

async fn options(
    conn: &mut PgConnection,
    sql: &str,
    bind: Option<Vec<Uuid>>,
) -> Result<Vec<FilterOption>> {
    let q = sqlx::query_as(sqlx::AssertSqlSafe(sql.to_string()));
    let q = match bind {
        Some(b) => q.bind(b),
        None => q,
    };
    Ok(q.fetch_all(&mut *conn).await?)
}

/// `GET /filter-fields?project=`: the fields the caller may filter on (in
/// `project` when given: its trackers, the custom fields that apply and are
/// readable there), their ops and values, and the sort keys. `project`
/// unknown or invisible → `not_found`.
pub async fn filter_fields_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Option<&str>,
) -> Result<FilterFields> {
    let project = match project.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some(crate::describe::visible_project(&mut *conn, caller, p).await?),
        None => None,
    };
    let scope = crate::db::load_scope(&mut *conn, caller).await?;
    let clock = clock(&mut *conn).await?;
    let project_key: Option<String> = match project {
        Some(p) => Some(
            sqlx::query_scalar("SELECT key FROM projects WHERE id = $1")
                .bind(p)
                .fetch_one(&mut *conn)
                .await?,
        ),
        None => None,
    };
    let visible: Vec<Uuid> = scope.project_ids().collect();
    let mut fields = Vec::new();
    for (name, ty, ops) in BUILTIN_FIELDS {
        // Contract 05 §4: agents get no spent_hours.
        if *name == "spent_hours" && caller.user_id().is_none() {
            continue;
        }
        let mut info = FilterFieldInfo {
            field: name.to_string(),
            ftype: ty.to_string(),
            label: label(name).to_string(),
            ops: ops.iter().map(|o| o.as_str()).collect(),
            options: None,
            options_source: None,
            dynamic: Vec::new(),
            restricted: None,
            definition_version: None,
        };
        match *name {
            "project" => {
                info.options = Some(
                    options(
                        conn,
                        "SELECT id::text AS id, name AS label, key, NULL::text AS category, NULL::boolean AS active
                           FROM projects WHERE id = ANY($1) ORDER BY key",
                        Some(visible.clone()),
                    )
                    .await?,
                );
            }
            "tracker" => {
                info.options = Some(match project {
                    Some(p) => {
                        options(
                            conn,
                            "SELECT t.id::text AS id, t.name AS label, NULL::text AS key, NULL::text AS category,
                                    NULL::boolean AS active
                               FROM trackers t JOIN project_trackers pt ON pt.tracker_id = t.id
                              WHERE pt.project_id = ANY($1) ORDER BY t.position, t.name",
                            Some(vec![p]),
                        )
                        .await?
                    }
                    None => {
                        options(
                            conn,
                            "SELECT id::text AS id, name AS label, NULL::text AS key, NULL::text AS category,
                                    NULL::boolean AS active
                               FROM trackers ORDER BY position, name",
                            None,
                        )
                        .await?
                    }
                });
            }
            "status" => {
                info.options = Some(
                    options(
                        conn,
                        "SELECT id::text AS id, name AS label, NULL::text AS key, category, NULL::boolean AS active
                           FROM issue_statuses ORDER BY position, name",
                        None,
                    )
                    .await?,
                );
            }
            "triage" => {
                info.options = Some(
                    [
                        ("pending", "待分流"),
                        ("snoozed", "暫緩"),
                        ("accepted", "已接受"),
                        ("declined", "已拒絕"),
                        ("duplicate", "重複"),
                        ("none", "未進收件匣"),
                    ]
                    .into_iter()
                    .map(|(id, l)| FilterOption {
                        id: id.into(),
                        label: l.into(),
                        key: None,
                        category: None,
                        active: None,
                    })
                    .collect(),
                );
            }
            "status_category" => {
                info.options = Some(
                    [("todo", "待辦"), ("doing", "進行中"), ("done", "已完成")]
                        .into_iter()
                        .map(|(id, l)| FilterOption {
                            id: id.into(),
                            label: l.into(),
                            key: None,
                            category: None,
                            active: None,
                        })
                        .collect(),
                );
            }
            "priority" => {
                info.options = Some(
                    options(
                        conn,
                        "SELECT id::text AS id, name AS label, NULL::text AS key, NULL::text AS category, active
                           FROM issue_priorities ORDER BY position, name",
                        None,
                    )
                    .await?,
                );
            }
            "assignee" | "author" => {
                info.options_source = Some(match &project_key {
                    Some(k) => format!("GET /api/v1/projects/{k}/assignees"),
                    None => "POST /api/v1/resolve".into(),
                });
                info.dynamic = vec!["me"];
            }
            "fixed_version" => {
                info.options_source = project_key
                    .as_ref()
                    .map(|k| format!("GET /api/v1/projects/{k}/versions"));
                info.dynamic = vec!["next"];
            }
            "start_date" | "due_date" | "created_at" | "updated_at" => {
                info.dynamic = vec!["rel", "range"];
            }
            _ => {}
        }
        fields.push(info);
    }
    let mut sortable: Vec<String> = SORTABLE.iter().map(|s| s.to_string()).collect();
    for (row, gate) in readable_fields(&mut *conn, caller, &scope).await? {
        if project.is_some_and(|p| !gate_allows(&gate, p)) {
            continue;
        }
        let ty = row.field_type()?;
        let mut info = FilterFieldInfo {
            field: format!("cf:{}", row.id),
            ftype: row.ftype.clone(),
            label: row.name.clone(),
            ops: cf_ops(ty),
            options: None,
            options_source: None,
            dynamic: Vec::new(),
            restricted: Some(!row.visible),
            definition_version: Some(row.definition_version),
        };
        match ty {
            FieldType::List => {
                info.options = Some(
                    sqlx::query_as(
                        "SELECT id::text AS id, label, NULL::text AS key, NULL::text AS category, active
                           FROM field_options WHERE field_id = $1 ORDER BY position, label",
                    )
                    .bind(row.id)
                    .fetch_all(&mut *conn)
                    .await?,
                );
            }
            FieldType::User => {
                info.options_source = Some(match &project_key {
                    Some(k) => format!("GET /api/v1/projects/{k}/assignees"),
                    None => "POST /api/v1/resolve".into(),
                });
                info.dynamic = vec!["me"];
            }
            FieldType::Date => info.dynamic = vec!["rel", "range"],
            _ => {}
        }
        if cf_sortable(ty) {
            sortable.push(info.field.clone());
        }
        fields.push(info);
    }
    Ok(FilterFields {
        today: clock.today.to_string(),
        week_start: clock.week_start,
        fields,
        sortable,
    })
}

pub async fn filter_fields(
    app: &crate::service::App,
    caller: AuthContext,
    project: Option<&str>,
) -> Result<FilterFields> {
    let mut tx = crate::service::read_tx(app).await?;
    let r = filter_fields_in(&mut tx, caller, project).await?;
    tx.commit().await?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(y: i32, m: u32, d: u32, ws: u32) -> Clock {
        Clock {
            today: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            week_start: ws,
        }
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn week_and_month_ranges() {
        // 2026-11-11 is a Wednesday.
        let c = clock(2026, 11, 11, 1);
        assert_eq!(
            range_bounds(c, "this_week", None),
            Some((d("2026-11-09"), d("2026-11-15")))
        );
        let sun = clock(2026, 11, 11, 7);
        assert_eq!(
            range_bounds(sun, "this_week", None),
            Some((d("2026-11-08"), d("2026-11-14")))
        );
        assert_eq!(
            range_bounds(sun, "last_week", None),
            Some((d("2026-11-01"), d("2026-11-07")))
        );
        assert_eq!(
            range_bounds(c, "next_week", None),
            Some((d("2026-11-16"), d("2026-11-22")))
        );
        // A Sunday with weeks starting Sunday starts its own week.
        let s = clock(2026, 11, 15, 7);
        assert_eq!(
            range_bounds(s, "this_week", None),
            Some((d("2026-11-15"), d("2026-11-21")))
        );
        assert_eq!(
            range_bounds(clock(2026, 3, 31, 1), "last_month", None),
            Some((d("2026-02-01"), d("2026-02-28")))
        );
        assert_eq!(
            range_bounds(clock(2026, 12, 5, 1), "this_month", None),
            Some((d("2026-12-01"), d("2026-12-31")))
        );
        assert_eq!(
            range_bounds(c, "last_n_days", Some(7)),
            Some((d("2026-11-04"), d("2026-11-11")))
        );
        assert_eq!(
            range_bounds(c, "next_n_days", Some(1)),
            Some((d("2026-11-11"), d("2026-11-12")))
        );
    }

    fn reason(e: anyhow::Error) -> String {
        crate::error::classify(&e)
            .and_then(|a| a.details)
            .and_then(|d| d["reason"].as_str().map(String::from))
            .unwrap_or_default()
    }

    #[test]
    fn limits_are_too_complex() {
        let cond = json!({"field": "project", "op": "eq", "value": "X"});
        let mut deep = cond.clone();
        for _ in 0..MAX_DEPTH {
            deep = json!({ "and": [deep] });
        }
        assert!(parse(&deep).is_ok());
        assert_eq!(
            reason(parse(&json!({ "or": [deep] })).unwrap_err()),
            "too_complex"
        );
        let many = json!({ "and": vec![cond.clone(); MAX_CONDITIONS] });
        assert!(parse(&many).is_ok());
        let more = json!({ "and": vec![cond.clone(); MAX_CONDITIONS + 1] });
        assert_eq!(reason(parse(&more).unwrap_err()), "too_complex");
        let big_in = json!({"field": "status", "op": "in", "value": vec!["x"; MAX_IN_VALUES + 1]});
        assert_eq!(reason(parse(&big_in).unwrap_err()), "too_complex");
        let huge = json!({"field": "status", "op": "eq", "value": "x".repeat(MAX_BYTES)});
        assert_eq!(reason(parse(&huge).unwrap_err()), "too_complex");
        assert_eq!(
            reason(parse(&json!({"field": "x", "op": "like"})).unwrap_err()),
            "unsupported_op"
        );
        assert_eq!(
            reason(parse(&json!({"field": "x", "op": "eq", "vlaue": 1})).unwrap_err()),
            "invalid_shape"
        );
    }

    #[test]
    fn dates_and_within() {
        let c = clock(2026, 11, 11, 1);
        assert_eq!(
            date_value(c, "p", &json!({"rel": "today", "offset": -3})).unwrap(),
            (d("2026-11-08"), json!({"rel": "today", "offset": -3}))
        );
        assert_eq!(
            date_value(c, "p", &json!({"rel": "today"})).unwrap().0,
            d("2026-11-11")
        );
        assert!(date_value(c, "p", &json!({"rel": "today", "offset": 3651})).is_err());
        assert!(date_value(c, "p", &json!({"rel": "yesterday"})).is_err());
        assert!(date_value(c, "p", &json!("2026-13-01")).is_err());
        let (t, n) = within_value(c, "p", Some(&json!({"range": "next_n_days", "n": 7}))).unwrap();
        assert_eq!(
            t,
            Test::Range {
                min: Some(Value::Date(d("2026-11-11"))),
                max: Some(Value::Date(d("2026-11-18")))
            }
        );
        assert_eq!(n, json!({"range": "next_n_days", "n": 7}));
        assert!(within_value(c, "p", Some(&json!({"range": "next_n_days"}))).is_err());
        assert!(within_value(c, "p", Some(&json!({"range": "this_week", "n": 2}))).is_err());
        assert!(within_value(c, "p", Some(&json!({"range": "last_n_days", "n": 367}))).is_err());
    }

    #[test]
    fn schema_is_generated() {
        let text = serde_json::to_string(&schemars::schema_for!(WhereInput)).unwrap();
        assert!(
            text.contains("ever_in") && text.contains("not_in"),
            "{text}"
        );
    }
}
