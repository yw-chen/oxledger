// SPDX-License-Identifier: GPL-2.0-or-later
//! Custom field filters: user input (field names, option labels, logins) →
//! typed [`ox_core::filter::Filter`] → SQL.
//!
//! SQL rules (research doc 02 §5.3, decision D14):
//! 1. Every condition carries a type condition: a comparison on `cf.v_x`, or
//!    `cf.v_x IS NOT NULL` for is_null / not_null. (Adding `IS NOT NULL` to a
//!    comparison is redundant and makes PostgreSQL count the null fraction of
//!    v_x twice: measured 22x vs 4.9x underestimate, doc 02 §8.)
//! 2. Every condition is one `EXISTS` semijoin over `issue_cf`, correlated on
//!    the issue id, inside the condition's applicability gate E (Redmine):
//!    positive ops `(E AND EXISTS ..)`, `ne` `NOT (E AND EXISTS (.. = x))`,
//!    `is_null` `(E AND NOT EXISTS ..)`. E is a visibility-style `Pred`
//!    (tracker, project, readable projects) compiled by `db::push_pred`.
//! 3. The field id is inlined as an integer literal, not a bind parameter, so
//!    the planner can match the partial index `WHERE field_id = <id>` even when
//!    a prepared statement switches to a generic plan.
//!
//! Remaining planner limitation (rule 3 of §5.3): estimates for numeric ranges
//! are still several times too low (doc 02 §8); list/user equality is fixed by
//! the `issue_cf_field_ref` statistics object.

use crate::auth::AuthContext;
use crate::custom_fields::{self, FieldRow, value_column};
use crate::db::{self, push_pred};
use crate::error::{ApiError, ErrorCode, api};
use crate::search::{DateField, IxFilter};
use crate::service::resolve_one;
use anyhow::Result;
use chrono::NaiveDate;
use ox_core::Pred;
use ox_core::Scope;
use ox_core::filter::{Cmp, Field, FieldType, Filter, Test, Value, ValueKind};
use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::{PgConnection, Postgres, QueryBuilder};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

// ---------------------------------------------------------------- built-in fields

/// Issue columns the filter also takes (contract 02 §17.15), under
/// pseudo field ids below zero. Their SQL keeps the custom-field shape — one
/// `EXISTS` over `issues` itself — so `ne` and `is_null` mean the same.
/// A custom field with one of these names is shadowed.
pub const VERSION_FIELD: i32 = -1;
const BUILTINS: &[(&str, i32, FieldType, &str)] = &[
    (
        "fixed_version_id",
        VERSION_FIELD,
        FieldType::List,
        "fixed_version_id",
    ),
    ("start_date", -2, FieldType::Date, "start_date"),
    ("due_date", -3, FieldType::Date, "due_date"),
];

fn builtin_column(id: i32) -> Option<&'static str> {
    BUILTINS.iter().find(|b| b.1 == id).map(|b| b.3)
}

/// The index-side pre-filter for a resolved filter (§17.15 fast fields):
/// built-in conditions narrow tantivy's candidates, everything else is
/// [`IxFilter::All`]. Always a superset of what the SQL keeps (P2-D12): an
/// `or` with an untranslatable branch is not narrowed, and `ne` — which also
/// keeps issues outside its applicability gate — only when the gate is True.
pub fn index_prefilter(f: &Filter) -> IxFilter {
    match f {
        Filter::And(ps) => IxFilter::and(ps.iter().map(index_prefilter).collect()),
        Filter::Or(ps) => {
            let parts: Vec<IxFilter> = ps.iter().map(index_prefilter).collect();
            if parts.is_empty() || parts.contains(&IxFilter::All) {
                IxFilter::All
            } else {
                IxFilter::Or(parts)
            }
        }
        Filter::Cond {
            field,
            test,
            applies,
        } => cond_prefilter(field.id, test, *applies == Pred::True),
    }
}

fn cond_prefilter(id: i32, test: &Test, ungated: bool) -> IxFilter {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    let not = |x: IxFilter| {
        if ungated {
            IxFilter::Not(Box::new(x))
        } else {
            IxFilter::All
        }
    };
    if id == VERSION_FIELD {
        let one = |v: &Value| match v {
            Value::Ref(u) => IxFilter::Version(Some(*u)),
            _ => IxFilter::All,
        };
        return match test {
            Test::Cmp(Cmp::Eq, v) => one(v),
            Test::Cmp(Cmp::Ne, v) => match one(v) {
                IxFilter::All => IxFilter::All,
                x => not(x),
            },
            Test::In(vs) => {
                let parts: Vec<IxFilter> = vs.iter().map(one).collect();
                if parts.is_empty() || parts.contains(&IxFilter::All) {
                    IxFilter::All
                } else {
                    IxFilter::Or(parts)
                }
            }
            Test::IsNull => IxFilter::Version(None),
            Test::NotNull => IxFilter::Not(Box::new(IxFilter::Version(None))),
            _ => IxFilter::All,
        };
    }
    let field = match id {
        -2 => DateField::Start,
        -3 => DateField::Due,
        _ => return IxFilter::All,
    };
    let d = |v: &Value| match v {
        Value::Date(d) => Some(*d),
        _ => None,
    };
    let range = |lo, hi| IxFilter::Date { field, lo, hi };
    match test {
        Test::Cmp(c, v) => {
            let Some(x) = d(v) else {
                return IxFilter::All;
            };
            match c {
                Cmp::Eq => range(Included(x), Included(x)),
                Cmp::Ne => not(range(Included(x), Included(x))),
                Cmp::Lt => range(Unbounded, Excluded(x)),
                Cmp::Le => range(Unbounded, Included(x)),
                Cmp::Gt => range(Excluded(x), Unbounded),
                Cmp::Ge => range(Included(x), Unbounded),
            }
        }
        Test::Range { min, max } => {
            let b = |v: &Option<Value>| match v.as_ref().map(d) {
                None => Some(Unbounded),
                Some(Some(x)) => Some(Included(x)),
                Some(None) => None,
            };
            match (b(min), b(max)) {
                (Some(lo), Some(hi)) => range(lo, hi),
                _ => IxFilter::All,
            }
        }
        Test::In(vs) => {
            let parts: Option<Vec<IxFilter>> = vs
                .iter()
                .map(|v| d(v).map(|x| range(Included(x), Included(x))))
                .collect();
            match parts {
                Some(p) if !p.is_empty() => IxFilter::Or(p),
                _ => IxFilter::All,
            }
        }
        Test::IsNull => IxFilter::DateNull(field),
        Test::NotNull => IxFilter::Not(Box::new(IxFilter::DateNull(field))),
    }
}

// ---------------------------------------------------------------- input

/// Custom field filter: a condition, or `{"and": [...]}` / `{"or": [...]}` of filters.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum FilterInput {
    And {
        /// All of these must hold.
        and: Vec<FilterInput>,
    },
    Or {
        /// At least one of these must hold.
        or: Vec<FilterInput>,
    },
    Cond(CondInput),
}

/// One condition on a custom field.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct CondInput {
    /// Custom field name (case-insensitive), e.g. "Severity".
    pub field: String,
    pub op: Op,
    /// eq/ne/lt/le/gt/ge: one value. in: an array. between: [min, max], inclusive, either may be null.
    /// is_null/not_null: omit. Numbers for int/float, "YYYY-MM-DD" for date, true/false for bool,
    /// the option label for list, "me"/login/name for user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
    /// Optional: the custom field's `definition_version` from `resolve(kind: "custom_field")`;
    /// if the definition changed since, the search fails with `field_definition_changed`
    /// (resolve again). Ignored for the built-in fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_version: Option<i32>,
}

/// `ne` also matches issues without a value; `lt`/`le`/`gt`/`ge`/`between` need an int, float or date field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Between,
    In,
    IsNull,
    NotNull,
}

const OPS: [(&str, Op); 10] = [
    ("eq", Op::Eq),
    ("ne", Op::Ne),
    ("lt", Op::Lt),
    ("le", Op::Le),
    ("gt", Op::Gt),
    ("ge", Op::Ge),
    ("between", Op::Between),
    ("in", Op::In),
    ("is_null", Op::IsNull),
    ("not_null", Op::NotNull),
];

/// Parses the JSON of a `filter` parameter, with errors a model can act on.
/// (Hand-written instead of `#[serde(untagged)]`, whose only error is
/// "did not match any variant".)
pub fn parse_input(v: &Json) -> Result<FilterInput> {
    parse_at(v, "filter")
}

fn parse_at(v: &Json, path: &str) -> Result<FilterInput> {
    let Some(obj) = v.as_object() else {
        return Err(filter_err(format!(
            "{path}: expected an object like {{\"field\": .., \"op\": .., \"value\": ..}} or {{\"and\": [..]}}"
        )));
    };
    for key in ["and", "or"] {
        if let Some(list) = obj.get(key) {
            if obj.len() != 1 {
                return Err(filter_err(format!(
                    "{path}: {key:?} must be the only key of its object"
                )));
            }
            let Some(items) = list.as_array() else {
                return Err(filter_err(format!("{path}.{key}: expected an array")));
            };
            let parts = items
                .iter()
                .enumerate()
                .map(|(i, x)| parse_at(x, &format!("{path}.{key}[{i}]")))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if key == "and" {
                FilterInput::And { and: parts }
            } else {
                FilterInput::Or { or: parts }
            });
        }
    }
    if let Some(k) = obj
        .keys()
        .find(|k| !["field", "op", "value", "definition_version"].contains(&k.as_str()))
    {
        return Err(filter_err(format!(
            "{path}: unknown key {k:?} (use field, op, value, definition_version, or and/or)"
        )));
    }
    let definition_version = match obj.get("definition_version") {
        None | Some(Json::Null) => None,
        Some(v) => match v.as_i64().and_then(|n| i32::try_from(n).ok()) {
            Some(n) if n >= 1 => Some(n),
            _ => {
                return Err(filter_err(format!(
                    "{path}.definition_version: expected a positive integer"
                )));
            }
        },
    };
    let Some(field) = obj.get("field").and_then(Json::as_str) else {
        return Err(filter_err(format!(
            "{path}: \"field\" (a string) is required"
        )));
    };
    let op_text = obj.get("op").and_then(Json::as_str).unwrap_or("");
    let Some(op) = OPS.iter().find(|(s, _)| *s == op_text).map(|(_, o)| *o) else {
        return Err(filter_err(format!(
            "{path}: \"op\" must be one of {}",
            OPS.map(|(s, _)| s).join(", ")
        )));
    };
    Ok(FilterInput::Cond(CondInput {
        field: field.to_string(),
        op,
        value: obj.get("value").filter(|v| !v.is_null()).cloned(),
        definition_version,
    }))
}

/// The `filter` argument cannot be used (`invalid_filter`, design 03 §3.7).
fn filter_err(msg: impl Into<String>) -> anyhow::Error {
    api(ErrorCode::InvalidFilter, msg)
}

// ---------------------------------------------------------------- resolve

struct Ctx<'a> {
    /// The read transaction's connection, shared by the recursive resolution.
    conn: tokio::sync::Mutex<&'a mut PgConnection>,
    caller: AuthContext,
    /// Fields the caller may read somewhere, by lowercase name, with their gate E.
    fields: HashMap<String, (FieldRow, Pred)>,
}

/// Where a field counts for the caller (Redmine
/// `IssueCustomField#visibility_by_project_condition`): its trackers, its
/// projects (or all), and — unless the field is visible or the caller is an
/// admin — the projects where the caller's effective roles (builtin ones
/// included) contain one of the field's roles.
/// `None` when the caller can read the field nowhere.
fn applies(row: &FieldRow, admin: bool, readable: &HashMap<i32, Vec<Uuid>>) -> Option<Pred> {
    let projects_pred = |ids: &[Uuid]| Pred::Or(ids.iter().map(|p| Pred::ProjectIs(*p)).collect());
    let readable = if row.visible || admin {
        Pred::True
    } else {
        match readable.get(&row.id) {
            Some(ids) if !ids.is_empty() => projects_pred(ids),
            _ => return None,
        }
    };
    let project = if row.is_for_all {
        Pred::True
    } else {
        projects_pred(&row.projects)
    };
    Some(
        Pred::And(vec![
            Pred::TrackerIn(row.trackers.clone()),
            project,
            readable,
        ])
        .simplify(),
    )
}

/// The custom fields the caller may read somewhere, each with its gate E.
/// Others are invisible: filters, `resolve` and errors never name them.
pub async fn readable_fields(
    conn: &mut PgConnection,
    caller: AuthContext,
    scope: &Scope,
) -> Result<Vec<(FieldRow, Pred)>> {
    let me = db::effective(&mut *conn, caller).await?;
    let admin = me.is_admin;
    // Agents read only fields with visible = true (contract 02 §3.5), so no
    // role-based readability at all.
    let readable = if me.agent.is_some() {
        Default::default()
    } else {
        custom_fields::readable_projects(&mut *conn, caller, &scope.principal_ids).await?
    };
    Ok(custom_fields::list_fields(&mut *conn)
        .await?
        .into_iter()
        .filter_map(|f| applies(&f, admin, &readable).map(|e| (f, e)))
        .collect())
}

/// Filter operators a field type takes (`resolve(kind: "custom_field")`).
pub fn operators(ty: FieldType) -> Vec<&'static str> {
    match ty {
        FieldType::Text => vec!["is_null", "not_null"],
        FieldType::Int | FieldType::Float | FieldType::Date => vec![
            "eq", "ne", "lt", "le", "gt", "ge", "between", "in", "is_null", "not_null",
        ],
        FieldType::Bool => vec!["eq", "ne", "is_null", "not_null"],
        _ => vec!["eq", "ne", "in", "is_null", "not_null"],
    }
}

/// Turns parsed input into a validated typed filter: looks up fields by name
/// (only those the caller may read; others look like unknown fields), list
/// options by label and users by login/name ("me" = the caller), and attaches
/// each condition's applicability gate.
pub async fn resolve(
    conn: &mut PgConnection,
    caller: AuthContext,
    scope: &Scope,
    input: &FilterInput,
) -> Result<Filter> {
    let fields = readable_fields(&mut *conn, caller, scope)
        .await?
        .into_iter()
        .map(|(f, e)| (f.name.to_lowercase(), (f, e)))
        .collect();
    let ctx = Ctx {
        conn: tokio::sync::Mutex::new(conn),
        caller,
        fields,
    };
    let filter = resolve_at(&ctx, input).await?;
    filter.validate().map_err(|e| filter_err(e.to_string()))?;
    Ok(filter)
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

fn resolve_at<'a, 'c: 'a>(ctx: &'a Ctx<'c>, input: &'a FilterInput) -> BoxFut<'a, Filter> {
    Box::pin(async move {
        match input {
            FilterInput::And { and } => {
                let mut out = Vec::with_capacity(and.len());
                for f in and {
                    out.push(resolve_at(ctx, f).await?);
                }
                Ok(Filter::And(out))
            }
            FilterInput::Or { or } => {
                let mut out = Vec::with_capacity(or.len());
                for f in or {
                    out.push(resolve_at(ctx, f).await?);
                }
                Ok(Filter::Or(out))
            }
            FilterInput::Cond(c) => resolve_cond(ctx, c).await,
        }
    })
}

async fn resolve_cond(ctx: &Ctx<'_>, c: &CondInput) -> Result<Filter> {
    let wanted = c.field.trim().to_lowercase();
    let builtin = BUILTINS.iter().find(|b| b.0 == wanted);
    let (field, name, gate) = match (builtin, ctx.fields.get(&wanted)) {
        (Some(b), _) => (Field { id: b.1, ty: b.2 }, b.0, Pred::True),
        (None, Some((row, _)))
            if c.definition_version
                .is_some_and(|v| v != row.definition_version) =>
        {
            return Err(ApiError::new(
                ErrorCode::FieldDefinitionChanged,
                format!(
                    "custom field {:?} changed since definition_version {}; resolve it again",
                    row.name,
                    c.definition_version.unwrap_or_default()
                ),
            )
            .with_details(serde_json::json!({
                "field": row.name,
                "current_version": row.definition_version,
                "suggested_tool": "resolve",
            }))
            .into());
        }
        (None, Some((row, gate))) => (
            Field {
                id: row.id,
                ty: row.field_type()?,
            },
            row.name.as_str(),
            gate.clone(),
        ),
        (None, None) => {
            let mut names: Vec<&str> = ctx.fields.values().map(|f| f.0.name.as_str()).collect();
            names.sort();
            let builtins: Vec<&str> = BUILTINS.iter().map(|b| b.0).collect();
            return Err(filter_err(format!(
                "no custom field {:?}; fields: {}; built-in: {}",
                c.field,
                names.join(", "),
                builtins.join(", ")
            )));
        }
    };
    let bad = |msg: String| filter_err(format!("field {name:?}: {msg}"));
    let one = |v: &Option<Json>| -> Result<Json> {
        match v {
            Some(Json::Array(_)) | None => Err(bad(format!("{:?} needs a single value", c.op))),
            Some(x) => Ok(x.clone()),
        }
    };
    let test = match c.op {
        Op::IsNull | Op::NotNull => {
            if c.value.is_some() {
                return Err(bad("is_null / not_null take no value".into()));
            }
            if c.op == Op::IsNull {
                Test::IsNull
            } else {
                Test::NotNull
            }
        }
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let cmp = match c.op {
                Op::Eq => Cmp::Eq,
                Op::Ne => Cmp::Ne,
                Op::Lt => Cmp::Lt,
                Op::Le => Cmp::Le,
                Op::Gt => Cmp::Gt,
                _ => Cmp::Ge,
            };
            Test::Cmp(cmp, value(ctx, field, name, &one(&c.value)?).await?)
        }
        Op::In => {
            let Some(Json::Array(xs)) = &c.value else {
                return Err(bad("in needs an array of values".into()));
            };
            let mut vs = Vec::with_capacity(xs.len());
            for x in xs {
                vs.push(value(ctx, field, name, x).await?);
            }
            Test::In(vs)
        }
        Op::Between => {
            let Some(Json::Array(xs)) = &c.value else {
                return Err(bad("between needs [min, max] (either may be null)".into()));
            };
            let [lo, hi] = xs.as_slice() else {
                return Err(bad("between needs [min, max] (either may be null)".into()));
            };
            let end = async |x: &Json| -> Result<Option<Value>> {
                if x.is_null() {
                    Ok(None)
                } else {
                    Ok(Some(value(ctx, field, name, x).await?))
                }
            };
            Test::Range {
                min: end(lo).await?,
                max: end(hi).await?,
            }
        }
    };
    if field.id == VERSION_FIELD && matches!(c.op, Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Between)
    {
        return Err(bad(
            "versions have no order; use eq, ne, in or is_null".into()
        ));
    }
    Filter::cond_where(field, test, gate).map_err(|e| bad(e.to_string()))
}

/// Converts one JSON value to the field's value kind.
async fn value(ctx: &Ctx<'_>, field: Field, name: &str, x: &Json) -> Result<Value> {
    let bad = |what: &str| {
        filter_err(format!(
            "field {name:?} ({}): {what}, got {x}",
            field.ty.as_str()
        ))
    };
    if field.id == VERSION_FIELD {
        // Ids only: names are per project (§17.15 `version:` resolves them).
        return x
            .as_str()
            .and_then(|s| Uuid::parse_str(s.trim()).ok())
            .map(Value::Ref)
            .ok_or_else(|| bad("expected a version id (uuid)"));
    }
    match field.ty.kind() {
        ValueKind::Num => {
            let n = match x {
                Json::Number(n) => n.as_f64(),
                Json::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            n.filter(|n| n.is_finite())
                .map(Value::Num)
                .ok_or_else(|| bad("expected a number"))
        }
        ValueKind::Date => x
            .as_str()
            .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
            .map(Value::Date)
            .ok_or_else(|| bad("expected a date \"YYYY-MM-DD\"")),
        ValueKind::Text => x
            .as_str()
            .map(|s| Value::Text(s.to_string()))
            .ok_or_else(|| bad("expected a string")),
        ValueKind::Bool => match x {
            Json::Bool(b) => Ok(Value::Bool(*b)),
            Json::String(s) if s.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
            Json::String(s) if s.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
            _ => Err(bad("expected true or false")),
        },
        ValueKind::Ref => {
            let Some(s) = x.as_str().map(str::trim) else {
                return Err(bad("expected a string"));
            };
            match field.ty {
                FieldType::List => {
                    let mut conn = ctx.conn.lock().await;
                    if let Some(id) = custom_fields::option_id(&mut **conn, field.id, s).await? {
                        return Ok(Value::Ref(id));
                    }
                    let labels: Vec<String> = sqlx::query_scalar(
                        "SELECT label FROM field_options WHERE field_id = $1 ORDER BY position, label",
                    )
                    .bind(field.id)
                    .fetch_all(&mut **conn)
                    .await?;
                    Err(filter_err(format!(
                        "field {name:?} has no option {s:?}; options: {}",
                        labels.join(", ")
                    )))
                }
                _ if s.eq_ignore_ascii_case("me") => match ctx.caller.user_id() {
                    Some(me) => Ok(Value::Ref(me)),
                    None => Err(filter_err("\"me\" needs an authenticated caller")),
                },
                _ => Ok(Value::Ref(
                    resolve_one(&mut **ctx.conn.lock().await, ctx.caller, "principal", s).await?,
                )),
            }
        }
    }
}

// ---------------------------------------------------------------- SQL

fn push_value(qb: &mut QueryBuilder<Postgres>, v: &Value) {
    match v {
        // Text → numeric keeps the comparison on numeric (a float8 parameter
        // would make PostgreSQL cast the column instead and skip the index).
        Value::Num(n) => {
            qb.push_bind(n.to_string()).push("::numeric");
        }
        Value::Date(d) => {
            qb.push_bind(*d).push("::date");
        }
        Value::Text(t) => {
            qb.push_bind(t.clone()).push("::text");
        }
        Value::Ref(u) => {
            qb.push_bind(*u).push("::uuid");
        }
        Value::Bool(b) => {
            qb.push_bind(*b).push("::boolean");
        }
    }
}

fn push_array(qb: &mut QueryBuilder<Postgres>, kind: ValueKind, vs: &[Value]) {
    macro_rules! arr {
        ($variant:ident, $map:expr, $cast:literal) => {{
            let xs: Vec<_> = vs
                .iter()
                .filter_map(|v| match v {
                    Value::$variant(x) => Some($map(x)),
                    _ => None,
                })
                .collect();
            qb.push_bind(xs).push($cast);
        }};
    }
    match kind {
        ValueKind::Num => arr!(Num, |n: &f64| n.to_string(), "::numeric[]"),
        ValueKind::Date => arr!(Date, |d: &NaiveDate| *d, "::date[]"),
        ValueKind::Text => arr!(Text, |t: &String| t.clone(), "::text[]"),
        ValueKind::Ref => arr!(Ref, |u: &uuid::Uuid| *u, "::uuid[]"),
        ValueKind::Bool => arr!(Bool, |b: &bool| *b, "::boolean[]"),
    }
}

/// Appends `filter` (already validated) as a SQL boolean expression over issue alias `a`.
pub fn push_filter(qb: &mut QueryBuilder<Postgres>, a: &str, filter: &Filter) {
    push_filter_shaped(qb, a, filter, Shape::S0);
}

/// SQL shape of the `EXISTS` conditions (design 03 §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Plain `EXISTS`: the planner may pull it up into a join and start
    /// from the rarest condition.
    S0,
    /// `EXISTS (.. OFFSET 0)`: stays a correlated SubPlan, probed per issue
    /// while walking the sort order until a page is full. PostgreSQL 18
    /// behaviour (`simplify_EXISTS_query` stops at OFFSET); re-check on
    /// major upgrades.
    S2,
}

/// Probe cap T + 1 (03 §5.4: T = 10,000, an absolute count).
pub const PROBE_CAP: i64 = 10_001;

/// The conditions strategy selection applies to (03 §5.4 item 2): an AND
/// of at least two positive, single-valued custom field conditions (no
/// `or`, `ne`, `is_null`, `not_null`, built-ins or long text). `None`:
/// keep the default shape.
pub fn probe_conditions(f: &Filter) -> Option<Vec<(Field, Test)>> {
    let conds: Vec<&Filter> = match f {
        Filter::And(ps) => ps.iter().collect(),
        c @ Filter::Cond { .. } => vec![c],
        Filter::Or(_) => return None,
    };
    let mut out = Vec::with_capacity(conds.len());
    for c in conds {
        let Filter::Cond { field, test, .. } = c else {
            return None;
        };
        let positive = matches!(
            test,
            Test::Cmp(Cmp::Eq | Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge, _)
                | Test::In(_)
                | Test::Range { .. }
        );
        if !positive || field.id <= 0 || field.ty == FieldType::Text {
            return None;
        }
        out.push((*field, test.clone()));
    }
    (out.len() >= 2).then_some(out)
}

/// `SELECT count(*)` of one condition's index entries, capped at
/// [`PROBE_CAP`] (index only; the count picks a shape and is never shown).
pub fn push_probe(qb: &mut QueryBuilder<Postgres>, field: Field, test: &Test) {
    qb.push(format!(
        "SELECT count(*) FROM (SELECT 1 FROM issue_cf cf WHERE cf.field_id = {}",
        field.id
    ));
    push_value_test(qb, value_column(field.ty.kind()), field, test);
    qb.push(format!(" LIMIT {PROBE_CAP}) p"));
}

/// [`push_filter`] in a chosen [`Shape`].
pub fn push_filter_shaped(qb: &mut QueryBuilder<Postgres>, a: &str, filter: &Filter, shape: Shape) {
    match filter {
        Filter::And(fs) | Filter::Or(fs) => {
            let and = matches!(filter, Filter::And(_));
            if fs.is_empty() {
                qb.push(if and { "TRUE" } else { "FALSE" });
                return;
            }
            qb.push("(");
            for (i, f) in fs.iter().enumerate() {
                if i > 0 {
                    qb.push(if and { " AND " } else { " OR " });
                }
                push_filter_shaped(qb, a, f, shape);
            }
            qb.push(")");
        }
        Filter::Cond {
            field,
            test,
            applies,
        } => {
            // ne is the complement of eq; is_null needs E but no value.
            let (outer_not, inner_not) = match test {
                Test::Cmp(Cmp::Ne, _) => (true, false),
                Test::IsNull => (false, true),
                _ => (false, false),
            };
            let gated = *applies != Pred::True;
            if outer_not {
                qb.push("NOT ");
            }
            if gated || outer_not {
                qb.push("(");
            }
            if gated {
                push_pred(qb, a, applies);
                qb.push(" AND ");
            }
            if inner_not {
                qb.push("NOT ");
            }
            push_exists(qb, a, *field, test, shape);
            if gated || outer_not {
                qb.push(")");
            }
        }
    }
}

/// `EXISTS (..)` for the positive form of `test` (`ne` as `=`, `is_null` as presence).
fn push_exists(qb: &mut QueryBuilder<Postgres>, a: &str, field: Field, test: &Test, shape: Shape) {
    let col = match builtin_column(field.id) {
        Some(c) => {
            qb.push(format!(
                "EXISTS (SELECT 1 FROM issues cf WHERE cf.id = {a}.id"
            ));
            c
        }
        None => {
            qb.push(format!(
                "EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = {a}.id AND cf.field_id = {}",
                field.id
            ));
            value_column(field.ty.kind())
        }
    };
    push_value_test(qb, col, field, test);
    if shape == Shape::S2 {
        qb.push(" OFFSET 0");
    }
    qb.push(")");
}

/// ` AND cf.<col> ..` for the positive form of `test`.
fn push_value_test(qb: &mut QueryBuilder<Postgres>, col: &str, field: Field, test: &Test) {
    match test {
        Test::Cmp(op, v) => {
            let sym = match op {
                Cmp::Eq | Cmp::Ne => "=",
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Gt => ">",
                Cmp::Ge => ">=",
            };
            qb.push(format!(" AND cf.{col} {sym} "));
            push_value(qb, v);
        }
        Test::Range { min, max } => {
            if let Some(v) = min {
                qb.push(format!(" AND cf.{col} >= "));
                push_value(qb, v);
            }
            if let Some(v) = max {
                qb.push(format!(" AND cf.{col} <= "));
                push_value(qb, v);
            }
        }
        Test::In(vs) => {
            qb.push(format!(" AND cf.{col} = ANY("));
            push_array(qb, field.ty.kind(), vs);
            qb.push(")");
        }
        Test::IsNull | Test::NotNull => {
            qb.push(format!(" AND cf.{col} IS NOT NULL"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sql(f: &Filter) -> String {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("");
        push_filter(&mut qb, "i", f);
        qb.sql().as_str().to_string()
    }

    const EST: Field = Field {
        id: 7,
        ty: FieldType::Float,
    };
    const SEV: Field = Field {
        id: 9,
        ty: FieldType::List,
    };

    #[test]
    fn every_condition_is_exists_with_type_condition_and_literal_id() {
        let f = Filter::Or(vec![
            Filter::cond(
                EST,
                Test::Range {
                    min: Some(Value::Num(1.5)),
                    max: None,
                },
            )
            .unwrap(),
            Filter::cond(SEV, Test::IsNull).unwrap(),
            Filter::cond(EST, Test::Cmp(Cmp::Ne, Value::Num(3.0))).unwrap(),
        ]);
        assert_eq!(
            sql(&f),
            "(EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = 7 AND cf.v_num >= $1::numeric) \
             OR NOT EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = 9 AND cf.v_ref IS NOT NULL) \
             OR NOT (EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = 7 AND cf.v_num = $2::numeric)))"
        );
        assert_eq!(sql(&Filter::And(vec![])), "TRUE");
        assert_eq!(sql(&Filter::Or(vec![])), "FALSE");
    }

    #[test]
    fn gate_wraps_each_form() {
        let tracker = Uuid::from_u128(5);
        let gate = Pred::TrackerIn(vec![tracker]);
        let at = |test| Filter::cond_where(EST, test, gate.clone()).unwrap();
        let e = "i.tracker_id = ANY($1)";
        let ex = "EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = 7";
        assert_eq!(
            sql(&at(Test::Cmp(Cmp::Gt, Value::Num(1.0)))),
            format!("({e} AND {ex} AND cf.v_num > $2::numeric))")
        );
        assert_eq!(
            sql(&at(Test::Cmp(Cmp::Ne, Value::Num(1.0)))),
            format!("NOT ({e} AND {ex} AND cf.v_num = $2::numeric))")
        );
        assert_eq!(
            sql(&at(Test::IsNull)),
            format!("({e} AND NOT {ex} AND cf.v_num IS NOT NULL))")
        );
    }

    #[test]
    fn in_binds_one_array() {
        let f = Filter::cond(EST, Test::In(vec![Value::Num(1.0), Value::Num(2.5)])).unwrap();
        assert!(sql(&f).ends_with("AND cf.v_num = ANY($1::numeric[]))"));
    }

    #[test]
    fn parse_input_shapes_and_errors() {
        let v = json!({"and": [
            {"field": "Estimate", "op": "between", "value": [1, null]},
            {"or": [{"field": "Severity", "op": "is_null"}, {"field": "Severity", "op": "in", "value": ["High"]}]}
        ]});
        let parsed = parse_input(&v).unwrap();
        let FilterInput::And { and } = &parsed else {
            panic!("{parsed:?}")
        };
        assert_eq!(and.len(), 2);
        assert!(matches!(&and[1], FilterInput::Or { or } if or.len() == 2));
        // Round-trips through the published shape.
        assert_eq!(
            parse_input(&serde_json::to_value(&parsed).unwrap()).unwrap(),
            parsed
        );

        let err = |v: Json| parse_input(&v).unwrap_err().to_string();
        assert!(err(json!({"field": "x", "op": "like"})).contains("must be one of"));
        assert!(
            err(json!({"field": "x", "op": "eq", "vlaue": 1})).contains("unknown key \"vlaue\"")
        );
        assert!(err(json!({"and": [], "or": []})).contains("only key"));
        assert!(err(json!({"and": [{"op": "eq"}]})).contains("filter.and[0]"));
        assert!(err(json!([1])).contains("expected an object"));
    }

    #[test]
    fn schema_is_generated() {
        let schema = schemars::schema_for!(FilterInput);
        let text = serde_json::to_string(&schema).unwrap();
        assert!(
            text.contains("is_null") && text.contains("\"and\""),
            "{text}"
        );
    }

    /// P2-D12: the index pre-filter never drops an issue the SQL keeps.
    /// Random built-in filters (gated and not) over random issues, with the
    /// SQL semantics of `push_filter` written out by hand.
    #[test]
    fn index_prefilter_is_a_superset() {
        use rand::{Rng, SeedableRng};
        use std::ops::Bound;
        #[derive(Clone, Copy)]
        struct Doc {
            version: Option<Uuid>,
            start: Option<NaiveDate>,
            due: Option<NaiveDate>,
            gated: bool,
        }
        let vs = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
        let day =
            |n: i64| NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(n as u64);
        let gate = Pred::ProjectIs(Uuid::from_u128(9));
        let mut rng = rand::rngs::StdRng::seed_from_u64(20261109);
        fn value_of(d: &Doc, id: i32) -> Option<Value> {
            match id {
                VERSION_FIELD => d.version.map(Value::Ref),
                -2 => d.start.map(Value::Date),
                _ => d.due.map(Value::Date),
            }
        }
        fn holds(t: &Test, v: &Option<Value>, e: bool) -> bool {
            let cmp = |a: &Value, b: &Value| match (a, b) {
                (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
                (Value::Ref(x), Value::Ref(y)) => Some(x.cmp(y)),
                _ => None,
            };
            use std::cmp::Ordering::*;
            match t {
                Test::Cmp(Cmp::Ne, x) => {
                    !(e && v.as_ref().is_some_and(|v| cmp(v, x) == Some(Equal)))
                }
                Test::IsNull => e && v.is_none(),
                Test::NotNull => e && v.is_some(),
                Test::Cmp(c, x) => {
                    e && v.as_ref().is_some_and(|v| match (c, cmp(v, x)) {
                        (Cmp::Eq, Some(o)) => o == Equal,
                        (Cmp::Lt, Some(o)) => o == Less,
                        (Cmp::Le, Some(o)) => o != Greater,
                        (Cmp::Gt, Some(o)) => o == Greater,
                        (Cmp::Ge, Some(o)) => o != Less,
                        _ => false,
                    })
                }
                Test::In(xs) => {
                    e && v
                        .as_ref()
                        .is_some_and(|v| xs.iter().any(|x| cmp(v, x) == Some(Equal)))
                }
                Test::Range { min, max } => {
                    e && v.as_ref().is_some_and(|v| {
                        min.as_ref().is_none_or(|m| cmp(v, m) != Some(Less))
                            && max.as_ref().is_none_or(|m| cmp(v, m) != Some(Greater))
                    })
                }
            }
        }
        fn sql_keeps(f: &Filter, d: &Doc) -> bool {
            match f {
                Filter::And(ps) => ps.iter().all(|p| sql_keeps(p, d)),
                Filter::Or(ps) => ps.iter().any(|p| sql_keeps(p, d)),
                Filter::Cond {
                    field,
                    test,
                    applies,
                } => {
                    let e = *applies == Pred::True || d.gated;
                    holds(test, &value_of(d, field.id), e)
                }
            }
        }
        fn ix_keeps(x: &IxFilter, d: &Doc) -> bool {
            let within = |v: Option<NaiveDate>, lo: &Bound<NaiveDate>, hi: &Bound<NaiveDate>| {
                v.is_some_and(|v| {
                    (match lo {
                        Bound::Included(l) => v >= *l,
                        Bound::Excluded(l) => v > *l,
                        Bound::Unbounded => true,
                    }) && (match hi {
                        Bound::Included(h) => v <= *h,
                        Bound::Excluded(h) => v < *h,
                        Bound::Unbounded => true,
                    })
                })
            };
            match x {
                IxFilter::All => true,
                IxFilter::And(ps) => ps.iter().all(|p| ix_keeps(p, d)),
                IxFilter::Or(ps) => ps.iter().any(|p| ix_keeps(p, d)),
                IxFilter::Not(p) => !ix_keeps(p, d),
                IxFilter::Version(v) => d.version == *v,
                IxFilter::DateNull(DateField::Start) => d.start.is_none(),
                IxFilter::DateNull(DateField::Due) => d.due.is_none(),
                IxFilter::Date {
                    field: DateField::Start,
                    lo,
                    hi,
                } => within(d.start, lo, hi),
                IxFilter::Date {
                    field: DateField::Due,
                    lo,
                    hi,
                } => within(d.due, lo, hi),
            }
        }
        let mut narrowed = 0;
        for _ in 0..3000 {
            fn random_filter(
                rng: &mut rand::rngs::StdRng,
                depth: u32,
                vs: &[Uuid; 3],
                day: &dyn Fn(i64) -> NaiveDate,
                gate: &Pred,
            ) -> Filter {
                if depth > 0 && rng.random_bool(0.4) {
                    let n = rng.random_range(1..=3);
                    let ps = (0..n)
                        .map(|_| random_filter(rng, depth - 1, vs, day, gate))
                        .collect();
                    return if rng.random_bool(0.5) {
                        Filter::And(ps)
                    } else {
                        Filter::Or(ps)
                    };
                }
                let id = [VERSION_FIELD, -2, -3][rng.random_range(0..3)];
                let val = |rng: &mut rand::rngs::StdRng| {
                    if id == VERSION_FIELD {
                        Value::Ref(vs[rng.random_range(0..3)])
                    } else {
                        Value::Date(day(rng.random_range(0..8)))
                    }
                };
                let cmps = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];
                let test = match rng.random_range(0..5) {
                    0 => Test::Cmp(cmps[rng.random_range(0..6)], val(rng)),
                    1 => Test::In((0..rng.random_range(1..3)).map(|_| val(rng)).collect()),
                    2 if id != VERSION_FIELD => Test::Range {
                        min: rng.random_bool(0.7).then(|| val(rng)),
                        max: Some(val(rng)),
                    },
                    3 => Test::IsNull,
                    _ => Test::NotNull,
                };
                let applies = if rng.random_bool(0.3) {
                    gate.clone()
                } else {
                    Pred::True
                };
                Filter::Cond {
                    field: Field {
                        id,
                        ty: if id == VERSION_FIELD {
                            FieldType::List
                        } else {
                            FieldType::Date
                        },
                    },
                    test,
                    applies,
                }
            }
            let f = random_filter(&mut rng, 3, &vs, &day, &gate);
            let ix = index_prefilter(&f);
            if ix != IxFilter::All {
                narrowed += 1;
            }
            for _ in 0..20 {
                let pick = |rng: &mut rand::rngs::StdRng| -> Option<NaiveDate> {
                    rng.random_bool(0.7).then(|| day(rng.random_range(0..8)))
                };
                let d = Doc {
                    version: rng.random_bool(0.7).then(|| vs[rng.random_range(0..3)]),
                    start: pick(&mut rng),
                    due: pick(&mut rng),
                    gated: rng.random_bool(0.5),
                };
                if sql_keeps(&f, &d) {
                    assert!(ix_keeps(&ix, &d), "{f:?}\n→ {ix:?}");
                }
            }
        }
        assert!(
            narrowed > 1000,
            "the pre-filter narrows most filters ({narrowed})"
        );
    }
}
