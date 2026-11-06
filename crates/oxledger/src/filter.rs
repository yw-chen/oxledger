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

use crate::custom_fields::{self, FieldRow, value_column};
use crate::db::{self, Caller, push_pred};
use crate::service::{App, resolve_one, user_err};
use anyhow::Result;
use chrono::NaiveDate;
use ox_core::Pred;
use ox_core::Scope;
use ox_core::filter::{Cmp, Field, FieldType, Filter, Test, Value, ValueKind};
use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::{Postgres, QueryBuilder};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

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
        return Err(user_err(format!(
            "{path}: expected an object like {{\"field\": .., \"op\": .., \"value\": ..}} or {{\"and\": [..]}}"
        )));
    };
    for key in ["and", "or"] {
        if let Some(list) = obj.get(key) {
            if obj.len() != 1 {
                return Err(user_err(format!(
                    "{path}: {key:?} must be the only key of its object"
                )));
            }
            let Some(items) = list.as_array() else {
                return Err(user_err(format!("{path}.{key}: expected an array")));
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
        .find(|k| !["field", "op", "value"].contains(&k.as_str()))
    {
        return Err(user_err(format!(
            "{path}: unknown key {k:?} (use field, op, value, or and/or)"
        )));
    }
    let Some(field) = obj.get("field").and_then(Json::as_str) else {
        return Err(user_err(format!(
            "{path}: \"field\" (a string) is required"
        )));
    };
    let op_text = obj.get("op").and_then(Json::as_str).unwrap_or("");
    let Some(op) = OPS.iter().find(|(s, _)| *s == op_text).map(|(_, o)| *o) else {
        return Err(user_err(format!(
            "{path}: \"op\" must be one of {}",
            OPS.map(|(s, _)| s).join(", ")
        )));
    };
    Ok(FilterInput::Cond(CondInput {
        field: field.to_string(),
        op,
        value: obj.get("value").filter(|v| !v.is_null()).cloned(),
    }))
}

// ---------------------------------------------------------------- resolve

struct Ctx<'a> {
    app: &'a App,
    caller: Caller,
    /// Fields the caller may read somewhere, by lowercase name, with their gate E.
    fields: HashMap<String, (FieldRow, Pred)>,
}

/// Where a field counts for the caller (Redmine
/// `IssueCustomField#visibility_by_project_condition`): its trackers, its
/// projects (or all), and — unless the field is visible or the caller is an
/// admin — the projects where the caller holds one of the field's roles.
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

/// Turns parsed input into a validated typed filter: looks up fields by name
/// (only those the caller may read; others look like unknown fields), list
/// options by label and users by login/name ("me" = the caller), and attaches
/// each condition's applicability gate.
pub async fn resolve(
    app: &App,
    caller: Caller,
    scope: &Scope,
    input: &FilterInput,
) -> Result<Filter> {
    let admin = db::is_admin(&app.pool, caller).await?;
    let readable = custom_fields::readable_projects(&app.pool, &scope.principal_ids).await?;
    let fields = custom_fields::list_fields(&app.pool)
        .await?
        .into_iter()
        .filter_map(|f| applies(&f, admin, &readable).map(|e| (f.name.to_lowercase(), (f, e))))
        .collect();
    let ctx = Ctx {
        app,
        caller,
        fields,
    };
    let filter = resolve_at(&ctx, input).await?;
    filter.validate().map_err(|e| user_err(e.to_string()))?;
    Ok(filter)
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

fn resolve_at<'a>(ctx: &'a Ctx<'a>, input: &'a FilterInput) -> BoxFut<'a, Filter> {
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
    let Some((row, gate)) = ctx.fields.get(&c.field.trim().to_lowercase()) else {
        let mut names: Vec<&str> = ctx.fields.values().map(|f| f.0.name.as_str()).collect();
        names.sort();
        return Err(user_err(format!(
            "no custom field {:?}; fields: {}",
            c.field,
            names.join(", ")
        )));
    };
    let field = Field {
        id: row.id,
        ty: row.field_type()?,
    };
    let name = row.name.as_str();
    let bad = |msg: String| user_err(format!("field {name:?}: {msg}"));
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
    Filter::cond_where(field, test, gate.clone()).map_err(|e| bad(e.to_string()))
}

/// Converts one JSON value to the field's value kind.
async fn value(ctx: &Ctx<'_>, field: Field, name: &str, x: &Json) -> Result<Value> {
    let bad = |what: &str| {
        user_err(format!(
            "field {name:?} ({}): {what}, got {x}",
            field.ty.as_str()
        ))
    };
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
                    if let Some(id) = custom_fields::option_id(&ctx.app.pool, field.id, s).await? {
                        return Ok(Value::Ref(id));
                    }
                    let labels: Vec<String> = sqlx::query_scalar(
                        "SELECT label FROM field_options WHERE field_id = $1 ORDER BY position, label",
                    )
                    .bind(field.id)
                    .fetch_all(&ctx.app.pool)
                    .await?;
                    Err(user_err(format!(
                        "field {name:?} has no option {s:?}; options: {}",
                        labels.join(", ")
                    )))
                }
                _ if s.eq_ignore_ascii_case("me") => match ctx.caller.0 {
                    Some(me) => Ok(Value::Ref(me)),
                    None => Err(user_err("\"me\" needs an authenticated caller")),
                },
                _ => Ok(Value::Ref(
                    resolve_one(ctx.app, ctx.caller, "principal", s).await?,
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
                push_filter(qb, a, f);
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
            push_exists(qb, a, *field, test);
            if gated || outer_not {
                qb.push(")");
            }
        }
    }
}

/// `EXISTS (..)` for the positive form of `test` (`ne` as `=`, `is_null` as presence).
fn push_exists(qb: &mut QueryBuilder<Postgres>, a: &str, field: Field, test: &Test) {
    let col = value_column(field.ty.kind());
    qb.push(format!(
        "EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = {a}.id AND cf.field_id = {}",
        field.id
    ));
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
    qb.push(")");
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
}
