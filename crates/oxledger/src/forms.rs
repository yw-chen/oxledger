// SPDX-License-Identifier: GPL-2.0-or-later
//! Issue forms (contract 05 §2.6, §3.6): L1 condition rules
//! (`required_if`, `allowed_values_if`, `readonly_if`), enforced by the
//! command engine on every entry point, and L2 layouts, a display hint only.
//!
//! A form belongs to a tracker, at site level or for one project. Each part
//! (rules, layout) goes draft → impact → publish; a rollback publishes an
//! old body again under a new version. The effective rules of an issue are
//! the site rules and the project rules together (a project only adds); the
//! effective layout is the project's when it published one, else the site's.
//!
//! The condition language (§3.6.2) compiles to the Filter AST of M1.1
//! ([`ox_core::filter::Filter`]) and is evaluated with its in-memory
//! [`Filter::eval`], so forms and filters share one set of comparison rules
//! (`ne` matches a missing value, `in` is any-equal, dates and numbers
//! order). `api/form-conformance.json` holds vectors the SPA's `show_if`
//! evaluator must agree with (T-M1-56).
//!
//! The effective rules version is `r<site>.<project>.s<L0>.w<workflow>`
//! (appendix C R13): the published rule versions of both layers, the L0
//! counter (`org_settings.form_l0_version`, migration 0025) and the
//! workflow version. A layout publish changes none of them (T-M1-49).

use crate::auth::{self, AuthContext, SessionPolicy};
use crate::commands::{self, Key};
use crate::db;
use crate::describe::{self, OptionView};
use crate::error::{ApiError, ErrorCode, api};
use crate::filter::{
    ASSIGNEE_FIELD, CATEGORY_FIELD, DUE_FIELD, ESTIMATED_FIELD, PRIORITY_FIELD, PRIVATE_FIELD,
    START_FIELD, STATUS_FIELD, TRACKER_FIELD, VERSION_FIELD,
};
use crate::perm::{self, MANAGE_PROJECT};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::IssueFacts;
use ox_core::filter::{Cmp, Field, FieldType, Filter, Test, Value};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// §3.6.2: a group holds at most 5 conditions.
pub const MAX_GROUP: usize = 5;
/// §3.6.2: `all`/`any` may nest once (two levels).
pub const MAX_DEPTH: usize = 2;
/// Values of one `in` condition.
pub const MAX_IN: usize = 20;
/// Rules of one form layer.
pub const MAX_RULES: usize = 100;
/// Values of one `allowed_values_if`.
pub const MAX_ALLOWED: usize = 100;
/// §3.6.3: the impact report scans at most this many issues.
pub const SCAN_LIMIT: i64 = 20_000;
/// Sample keys in an impact report.
const SAMPLE_KEYS: usize = 10;
/// Sections of one layout mode, and fields of one mode.
const MAX_SECTIONS: usize = 30;
const MAX_LAYOUT_FIELDS: usize = 200;

/// `form_rule_versions.state` / `form_layout_versions.state` (0024).
pub const VERSION_STATES: [&str; 4] = ["draft", "published", "superseded", "discarded"];
/// `form_rule_versions.publish_mode` (0024).
pub const PUBLISH_MODES: [&str; 2] = ["block", "accept_and_mark"];
/// Rule kinds (§3.6.2).
pub const RULE_KINDS: [&str; 3] = ["required_if", "allowed_values_if", "readonly_if"];
/// Condition ops (§3.6.2).
pub const COND_OPS: [&str; 9] = [
    "eq", "ne", "in", "is_set", "not_set", "lt", "le", "gt", "ge",
];
/// Core fields a condition may name (§3.6.2).
pub const CORE_COND_FIELDS: [&str; 10] = [
    "tracker",
    "status",
    "status_category",
    "priority",
    "assignee",
    "fixed_version",
    "start_date",
    "due_date",
    "estimated_hours",
    "is_private",
];
/// `form_invalid` kinds (§3.6.3, plus `malformed` for the JSON shape and
/// `invalid_rule` for a rule whose parts do not fit together).
pub const INVALID_KINDS: [&str; 8] = [
    "unknown_field",
    "restricted_condition_field",
    "condition_too_complex",
    "cycle",
    "hides_required",
    "field_not_applicable",
    "malformed",
    "invalid_rule",
];

// ---------------------------------------------------------------- errors

fn form_invalid(kind: &str, msg: impl Into<String>, extra: Json) -> anyhow::Error {
    let mut d = json!({"kind": kind});
    if let (Json::Object(m), Json::Object(x)) = (&mut d, extra) {
        m.extend(x);
    }
    ApiError::new(ErrorCode::FormInvalid, msg)
        .with_details(d)
        .into()
}

fn forbidden(msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": "permission"}))
        .into()
}

fn not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no such {what}"))
}

// ---------------------------------------------------------------- catalog

/// A custom field as forms see it.
#[derive(Debug, Clone)]
pub(crate) struct CfInfo {
    pub ftype: FieldType,
    pub visible: bool,
    /// Deactivated (0025) or being dropped: no new rules about it.
    pub inactive: bool,
    /// Applies to the form's tracker (and project, for a project form).
    pub applies: bool,
    pub is_required: bool,
}

/// Every custom field, with whether it applies to (tracker, project).
pub(crate) async fn catalog(
    conn: &mut PgConnection,
    tracker: Uuid,
    project: Option<Uuid>,
) -> Result<BTreeMap<i32, CfInfo>> {
    let rows: Vec<(i32, String, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT d.id, d.ftype, d.visible,
                d.deactivated_at IS NOT NULL OR d.index_state = 'dropping',
                EXISTS (SELECT 1 FROM field_trackers ft WHERE ft.field_id = d.id AND ft.tracker_id = $1)
                  AND ($2::uuid IS NULL OR d.is_for_all
                       OR EXISTS (SELECT 1 FROM field_projects fp WHERE fp.field_id = d.id AND fp.project_id = $2)),
                d.is_required
         FROM field_defs d",
    )
    .bind(tracker)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, t, visible, inactive, applies, is_required)| {
            Some((
                id,
                CfInfo {
                    ftype: FieldType::parse(&t)?,
                    visible,
                    inactive,
                    applies,
                    is_required,
                },
            ))
        })
        .collect())
}

// ---------------------------------------------------------------- conditions

/// A parsed `when`: the compiled filter, the fields it names and its
/// normalized JSON (for comparisons between conditions).
#[derive(Debug, Clone)]
pub(crate) struct When {
    pub filter: Filter,
    pub fields: BTreeSet<String>,
    pub json: Json,
    /// `all` items (normalized JSON) when the top group is `all`.
    pub all_items: Option<Vec<Json>>,
}

/// The Filter AST field of a condition field name.
fn cond_field(name: &str, cat: &BTreeMap<i32, CfInfo>) -> Option<(Field, Option<&'static str>)> {
    let core = |id, ty| Some((Field { id, ty }, None));
    match name {
        "tracker" => core(TRACKER_FIELD, FieldType::List),
        "status" => core(STATUS_FIELD, FieldType::List),
        "status_category" => core(CATEGORY_FIELD, FieldType::String),
        "priority" => core(PRIORITY_FIELD, FieldType::List),
        "assignee" => core(ASSIGNEE_FIELD, FieldType::List),
        "fixed_version" => core(VERSION_FIELD, FieldType::List),
        "start_date" => core(START_FIELD, FieldType::Date),
        "due_date" => core(DUE_FIELD, FieldType::Date),
        "estimated_hours" => core(ESTIMATED_FIELD, FieldType::Float),
        "is_private" => core(PRIVATE_FIELD, FieldType::Bool),
        _ => {
            let Some(Key::Cf(id)) = Key::parse(name) else {
                return None;
            };
            let c = cat.get(&id)?;
            Some((Field { id, ty: c.ftype }, Some(c.ftype.as_str())))
        }
    }
}

/// A condition value (§3.6.2) → its Filter value and normalized JSON.
fn cond_value(field: &str, ty: FieldType, typed: Option<&str>, v: &Json) -> Option<(Value, Json)> {
    if let Some(t) = typed {
        // Custom fields: `{"<type>": v}` as in command documents.
        let (got, canon) = commands::parse_cf("", v).ok()?;
        if got != Some(t) || canon.is_null() {
            return None;
        }
        let inner = canon.get(t)?.clone();
        let val = match ty {
            FieldType::Int | FieldType::Float => Value::Num(inner.as_str()?.parse().ok()?),
            FieldType::Date => Value::Date(inner.as_str()?.parse().ok()?),
            FieldType::List | FieldType::User => Value::Ref(Uuid::parse_str(inner.as_str()?).ok()?),
            FieldType::Bool => Value::Bool(inner.as_bool()?),
            FieldType::String | FieldType::Text | FieldType::Link => {
                Value::Text(inner.as_str()?.to_string())
            }
        };
        return Some((val, canon));
    }
    match field {
        "status_category" => {
            let s = v.as_str()?;
            ["todo", "doing", "done"]
                .contains(&s)
                .then(|| (Value::Text(s.into()), json!(s)))
        }
        "start_date" | "due_date" => {
            let d: NaiveDate = v.as_str()?.parse().ok()?;
            Some((Value::Date(d), json!(d.to_string())))
        }
        "estimated_hours" => {
            let c = commands::estimated_hours(v.as_str()?).ok()?;
            Some((Value::Num(c.parse().ok()?), json!(c)))
        }
        "is_private" => {
            let b = v.as_bool()?;
            Some((Value::Bool(b), json!(b)))
        }
        _ => {
            let u = Uuid::parse_str(v.as_str()?).ok()?;
            Some((Value::Ref(u), json!(u)))
        }
    }
}

/// Parses a `when` (§3.6.2). `restricted_ok` lets a condition name a
/// restricted custom field (only when loading what was published earlier).
pub(crate) fn parse_when(
    v: &Json,
    path: &str,
    cat: &BTreeMap<i32, CfInfo>,
    lenient: bool,
) -> Result<When> {
    let mut fields = BTreeSet::new();
    let (filter, json) = parse_group(v, path, 1, cat, lenient, &mut fields)?;
    let all_items = match &json {
        Json::Object(m) => m.get("all").and_then(Json::as_array).cloned(),
        _ => None,
    };
    Ok(When {
        filter,
        fields,
        json,
        all_items,
    })
}

fn parse_group(
    v: &Json,
    path: &str,
    depth: usize,
    cat: &BTreeMap<i32, CfInfo>,
    lenient: bool,
    fields: &mut BTreeSet<String>,
) -> Result<(Filter, Json)> {
    let Json::Object(m) = v else {
        return Err(form_invalid(
            "malformed",
            "a condition group is {\"all\": [...]} or {\"any\": [...]}",
            json!({"path": path}),
        ));
    };
    let (key, items) = match (m.len(), m.get("all"), m.get("any")) {
        (1, Some(Json::Array(a)), None) => ("all", a),
        (1, None, Some(Json::Array(a))) => ("any", a),
        _ => {
            return Err(form_invalid(
                "malformed",
                "a condition group is {\"all\": [...]} or {\"any\": [...]}",
                json!({"path": path}),
            ));
        }
    };
    if depth > MAX_DEPTH {
        return Err(form_invalid(
            "condition_too_complex",
            "conditions nest at most two levels",
            json!({"path": path}),
        ));
    }
    if items.is_empty() || items.len() > MAX_GROUP {
        return Err(form_invalid(
            "condition_too_complex",
            "a condition group has 1–5 items",
            json!({"path": path}),
        ));
    }
    let mut fs = Vec::new();
    let mut js = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let p = format!("{path}.{key}[{i}]");
        let is_group = it
            .as_object()
            .is_some_and(|o| o.contains_key("all") || o.contains_key("any"));
        let (f, j) = if is_group {
            parse_group(it, &p, depth + 1, cat, lenient, fields)?
        } else {
            parse_cond(it, &p, cat, lenient, fields)?
        };
        fs.push(f);
        js.push(j);
    }
    let filter = if key == "all" {
        Filter::And(fs)
    } else {
        Filter::Or(fs)
    };
    Ok((filter, json!({ key: js })))
}

fn parse_cond(
    v: &Json,
    path: &str,
    cat: &BTreeMap<i32, CfInfo>,
    lenient: bool,
    fields: &mut BTreeSet<String>,
) -> Result<(Filter, Json)> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct C {
        field: String,
        op: String,
        #[serde(default)]
        value: Option<Json>,
    }
    let c: C = serde_json::from_value(v.clone()).map_err(|e| {
        form_invalid(
            "malformed",
            format!("a condition is {{field, op, value}}: {e}"),
            json!({"path": path}),
        )
    })?;
    let Some((field, typed)) = cond_field(&c.field, cat) else {
        return Err(form_invalid(
            "unknown_field",
            format!("unknown condition field {:?}", c.field),
            json!({"path": format!("{path}.field"), "field": c.field}),
        ));
    };
    if let Some(Key::Cf(id)) = Key::parse(&c.field) {
        let info = &cat[&id];
        if !lenient {
            if !info.visible {
                return Err(form_invalid(
                    "restricted_condition_field",
                    "conditions may only name custom fields everyone can read",
                    json!({"path": format!("{path}.field"), "field": c.field}),
                ));
            }
            if info.inactive {
                return Err(form_invalid(
                    "unknown_field",
                    format!("unknown condition field {:?}", c.field),
                    json!({"path": format!("{path}.field"), "field": c.field}),
                ));
            }
            if !info.applies {
                return Err(form_invalid(
                    "field_not_applicable",
                    format!("{} does not apply to this tracker", c.field),
                    json!({"path": format!("{path}.field"), "field": c.field}),
                ));
            }
        }
    }
    let bad_op = || {
        form_invalid(
            "invalid_rule",
            format!("op {:?} does not fit {}", c.op, c.field),
            json!({"path": format!("{path}.op"), "allowed": COND_OPS}),
        )
    };
    let bad_value = || {
        form_invalid(
            "invalid_rule",
            format!("not a value of {}", c.field),
            json!({"path": format!("{path}.value")}),
        )
    };
    let one = |x: Option<&Json>| -> Result<(Value, Json)> {
        let x = x.ok_or_else(bad_value)?;
        cond_value(&c.field, field.ty, typed, x).ok_or_else(bad_value)
    };
    let text_only = field.ty == FieldType::Text;
    let (test, value) = match c.op.as_str() {
        "is_set" | "not_set" => {
            if c.value.is_some() {
                return Err(bad_value());
            }
            let t = if c.op == "is_set" {
                Test::NotNull
            } else {
                Test::IsNull
            };
            (t, None)
        }
        _ if text_only => return Err(bad_op()),
        "eq" | "ne" => {
            let (v, j) = one(c.value.as_ref())?;
            let op = if c.op == "eq" { Cmp::Eq } else { Cmp::Ne };
            (Test::Cmp(op, v), Some(j))
        }
        "lt" | "le" | "gt" | "ge" => {
            if !matches!(
                field.ty,
                FieldType::Int | FieldType::Float | FieldType::Date
            ) {
                return Err(bad_op());
            }
            let (v, j) = one(c.value.as_ref())?;
            let op = match c.op.as_str() {
                "lt" => Cmp::Lt,
                "le" => Cmp::Le,
                "gt" => Cmp::Gt,
                _ => Cmp::Ge,
            };
            (Test::Cmp(op, v), Some(j))
        }
        "in" => {
            let Some(Json::Array(xs)) = &c.value else {
                return Err(bad_value());
            };
            if xs.is_empty() || xs.len() > MAX_IN {
                return Err(form_invalid(
                    "condition_too_complex",
                    "in takes 1–20 values",
                    json!({"path": format!("{path}.value")}),
                ));
            }
            let mut vs = Vec::new();
            let mut js = Vec::new();
            for x in xs {
                let (v, j) = one(Some(x))?;
                vs.push(v);
                js.push(j);
            }
            (Test::In(vs), Some(Json::Array(js)))
        }
        _ => return Err(bad_op()),
    };
    let filter = Filter::cond(field, test).map_err(|e| {
        form_invalid(
            "invalid_rule",
            e.to_string(),
            json!({"path": format!("{path}.value")}),
        )
    })?;
    fields.insert(c.field.clone());
    let mut j = json!({"field": c.field, "op": c.op});
    if let Some(v) = value {
        j["value"] = v;
    }
    Ok((filter, j))
}

// ---------------------------------------------------------------- evaluation state

/// The values a condition sees: the Filter AST field id → value.
#[derive(Debug, Default, Clone)]
pub(crate) struct State(BTreeMap<i32, Value>);

const NO_FACTS: IssueFacts = IssueFacts {
    project_id: Uuid::nil(),
    tracker_id: Uuid::nil(),
    is_private: false,
    author_id: Uuid::nil(),
    assigned_to_id: None,
};

impl State {
    /// An issue's state: tracker, status (and its category), privacy and
    /// the field values in command form (`assigned_to_id`, `cf:7`, …).
    pub fn of<'a>(
        tracker: Uuid,
        status: Uuid,
        category: Option<&str>,
        is_private: bool,
        values: impl IntoIterator<Item = (String, &'a Json)>,
    ) -> State {
        let mut s = State::default();
        s.0.insert(TRACKER_FIELD, Value::Ref(tracker));
        s.0.insert(STATUS_FIELD, Value::Ref(status));
        if let Some(c) = category {
            s.0.insert(CATEGORY_FIELD, Value::Text(c.into()));
        }
        s.0.insert(PRIVATE_FIELD, Value::Bool(is_private));
        for (k, v) in values {
            let name = match k.as_str() {
                "assigned_to_id" => "assignee",
                "priority_id" => "priority",
                "fixed_version_id" => "fixed_version",
                other => other,
            };
            s.set(name, v);
        }
        s
    }

    /// Sets one value by condition field name (custom fields typed).
    pub fn set(&mut self, name: &str, v: &Json) {
        if v.is_null() || v.as_str() == Some("") {
            return;
        }
        let value = match name {
            "tracker" | "status" | "priority" | "assignee" | "fixed_version" => v
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .map(Value::Ref),
            "status_category" => v.as_str().map(|s| Value::Text(s.into())),
            "start_date" | "due_date" => v.as_str().and_then(|s| s.parse().ok()).map(Value::Date),
            "estimated_hours" => v.as_str().and_then(|s| s.parse().ok()).map(Value::Num),
            "is_private" => v.as_bool().map(Value::Bool),
            _ => {
                let Some(Key::Cf(_)) = Key::parse(name) else {
                    return;
                };
                let Json::Object(m) = v else { return };
                let Some((t, inner)) = m.iter().next() else {
                    return;
                };
                match t.as_str() {
                    "int" | "float" => inner.as_str().and_then(|s| s.parse().ok()).map(Value::Num),
                    "date" => inner.as_str().and_then(|s| s.parse().ok()).map(Value::Date),
                    "list" | "user" => inner
                        .as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .map(Value::Ref),
                    "bool" => inner.as_bool().map(Value::Bool),
                    // An empty string is no value (as in command documents).
                    _ => inner
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(|s| Value::Text(s.into())),
                }
            }
        };
        let id = match name {
            "tracker" => TRACKER_FIELD,
            "status" => STATUS_FIELD,
            "status_category" => CATEGORY_FIELD,
            "priority" => PRIORITY_FIELD,
            "assignee" => ASSIGNEE_FIELD,
            "fixed_version" => VERSION_FIELD,
            "start_date" => START_FIELD,
            "due_date" => DUE_FIELD,
            "estimated_hours" => ESTIMATED_FIELD,
            "is_private" => PRIVATE_FIELD,
            _ => match Key::parse(name) {
                Some(Key::Cf(id)) => id,
                _ => return,
            },
        };
        if let Some(v) = value {
            self.0.insert(id, v);
        }
    }

    pub fn holds(&self, w: &When) -> bool {
        w.filter.eval(&NO_FACTS, &|id| self.0.get(&id))
    }
}

/// Evaluates a `when` against values keyed by condition field name (the
/// conformance vectors' shape). `cf_types` gives custom field types.
pub fn eval_vector(when: &Json, values: &Json, cf_types: &BTreeMap<i32, String>) -> Result<bool> {
    let cat: BTreeMap<i32, CfInfo> = cf_types
        .iter()
        .filter_map(|(id, t)| {
            Some((
                *id,
                CfInfo {
                    ftype: FieldType::parse(t)?,
                    visible: true,
                    inactive: false,
                    applies: true,
                    is_required: false,
                },
            ))
        })
        .collect();
    let w = parse_when(when, "when", &cat, false)?;
    let mut s = State::default();
    if let Json::Object(m) = values {
        for (k, v) in m {
            s.set(k, v);
        }
    }
    Ok(s.holds(&w))
}

// ---------------------------------------------------------------- rules

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum RuleKind {
    RequiredIf,
    AllowedValuesIf,
    ReadonlyIf,
}

/// One L1 rule.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub id: String,
    /// The target field (command key: `due_date`, `cf:12`, …).
    pub field: String,
    pub kind: RuleKind,
    pub when: When,
    /// `allowed_values_if`: canonical values (as the engine stores them).
    pub values: Vec<Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesBodyIn {
    rules: Vec<Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleIn {
    id: String,
    field: String,
    kind: String,
    when: Json,
    #[serde(default)]
    values: Option<Vec<Json>>,
}

fn rule_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Parses one rule. `lenient` (loading a published body): conditions may
/// name fields that became restricted or inactive since.
fn parse_rule(v: &Json, i: usize, cat: &BTreeMap<i32, CfInfo>, lenient: bool) -> Result<Rule> {
    let path = format!("rules[{i}]");
    let r: RuleIn = serde_json::from_value(v.clone()).map_err(|e| {
        form_invalid(
            "malformed",
            format!("a rule is {{id, field, kind, when, values?}}: {e}"),
            json!({"path": path}),
        )
    })?;
    let rid = json!(r.id);
    if !rule_id_ok(&r.id) {
        return Err(form_invalid(
            "invalid_rule",
            "a rule id is 1–40 characters of a–z, 0–9 and _",
            json!({"path": format!("{path}.id")}),
        ));
    }
    let kind = match r.kind.as_str() {
        "required_if" => RuleKind::RequiredIf,
        "allowed_values_if" => RuleKind::AllowedValuesIf,
        "readonly_if" => RuleKind::ReadonlyIf,
        _ => {
            return Err(form_invalid(
                "invalid_rule",
                format!("unknown rule kind {:?}", r.kind),
                json!({"path": format!("{path}.kind"), "rule_id": rid, "allowed": RULE_KINDS}),
            ));
        }
    };
    let Some(key) = Key::parse(&r.field) else {
        return Err(form_invalid(
            "unknown_field",
            format!("unknown field {:?}", r.field),
            json!({"path": format!("{path}.field"), "rule_id": rid}),
        ));
    };
    if let Key::Cf(id) = key {
        match cat.get(&id) {
            None => {
                return Err(form_invalid(
                    "unknown_field",
                    format!("unknown field {:?}", r.field),
                    json!({"path": format!("{path}.field"), "rule_id": rid}),
                ));
            }
            Some(c) if !lenient && c.inactive => {
                return Err(form_invalid(
                    "unknown_field",
                    format!("unknown field {:?}", r.field),
                    json!({"path": format!("{path}.field"), "rule_id": rid}),
                ));
            }
            Some(c) if !lenient && !c.applies => {
                return Err(form_invalid(
                    "field_not_applicable",
                    format!("{} does not apply to this tracker", r.field),
                    json!({"path": format!("{path}.field"), "rule_id": rid}),
                ));
            }
            _ => {}
        }
    }
    let fits = match (kind, key) {
        (RuleKind::ReadonlyIf, _) => true,
        (RuleKind::RequiredIf, Key::Subject) => false,
        (RuleKind::RequiredIf, _) => true,
        (RuleKind::AllowedValuesIf, Key::Subject | Key::Description) => false,
        (RuleKind::AllowedValuesIf, Key::Cf(id)) => cat[&id].ftype != FieldType::Text,
        (RuleKind::AllowedValuesIf, _) => true,
    };
    if !fits {
        return Err(form_invalid(
            "invalid_rule",
            format!("{} cannot have a {} rule", r.field, r.kind),
            json!({"path": format!("{path}.field"), "rule_id": rid}),
        ));
    }
    let when = parse_when(&r.when, &format!("{path}.when"), cat, lenient).map_err(|e| {
        // Name the rule in every condition error.
        match crate::error::classify(&e) {
            Some(mut a) => {
                if let Some(Json::Object(d)) = a.details.as_mut() {
                    d.insert("rule_id".into(), rid.clone());
                }
                a.into()
            }
            None => e,
        }
    })?;
    let values = match (kind, r.values) {
        (RuleKind::AllowedValuesIf, Some(vs)) => {
            if vs.is_empty() || vs.len() > MAX_ALLOWED {
                return Err(form_invalid(
                    "invalid_rule",
                    "allowed_values_if takes 1–100 values",
                    json!({"path": format!("{path}.values"), "rule_id": rid}),
                ));
            }
            let mut out = Vec::new();
            for (j, x) in vs.iter().enumerate() {
                let vp = format!("{path}.values[{j}]");
                let canon = match key {
                    Key::Cf(id) => match commands::parse_cf(&vp, x) {
                        Ok((Some(t), c)) if t == cat[&id].ftype.as_str() && !c.is_null() => c,
                        _ => Json::Null,
                    },
                    _ => commands::parse_builtin(&vp, key, x).unwrap_or(Json::Null),
                };
                if canon.is_null() || canon.as_str() == Some("") {
                    return Err(form_invalid(
                        "invalid_rule",
                        format!("not a value of {}", r.field),
                        json!({"path": vp, "rule_id": rid}),
                    ));
                }
                out.push(canon);
            }
            out
        }
        (RuleKind::AllowedValuesIf, None) => {
            return Err(form_invalid(
                "invalid_rule",
                "allowed_values_if needs values",
                json!({"path": format!("{path}.values"), "rule_id": rid}),
            ));
        }
        (_, Some(_)) => {
            return Err(form_invalid(
                "invalid_rule",
                "only allowed_values_if takes values",
                json!({"path": format!("{path}.values"), "rule_id": rid}),
            ));
        }
        (_, None) => Vec::new(),
    };
    Ok(Rule {
        id: r.id,
        field: key.name(),
        kind,
        when,
        values,
    })
}

/// Validates a rules body (a draft). Every error is `form_invalid`.
pub(crate) fn parse_rules(body: &Json, cat: &BTreeMap<i32, CfInfo>) -> Result<Vec<Rule>> {
    let b: RulesBodyIn = serde_json::from_value(body.clone()).map_err(|e| {
        form_invalid(
            "malformed",
            format!("the rules body is {{\"rules\": [...]}}: {e}"),
            json!({"path": ""}),
        )
    })?;
    if b.rules.len() > MAX_RULES {
        return Err(form_invalid(
            "condition_too_complex",
            "at most 100 rules",
            json!({"path": "rules"}),
        ));
    }
    let mut out: Vec<Rule> = Vec::new();
    for (i, r) in b.rules.iter().enumerate() {
        let rule = parse_rule(r, i, cat, false)?;
        if out.iter().any(|o| o.id == rule.id) {
            return Err(form_invalid(
                "invalid_rule",
                format!("rule id {:?} is used twice", rule.id),
                json!({"path": format!("rules[{i}].id"), "rule_id": rule.id}),
            ));
        }
        out.push(rule);
    }
    Ok(out)
}

/// A published body: rules that no longer parse (a field was dropped) are
/// left out rather than failing every write.
fn load_rules(body: &Json, cat: &BTreeMap<i32, CfInfo>) -> Vec<Rule> {
    let Some(Json::Array(rs)) = body.get("rules") else {
        return Vec::new();
    };
    rs.iter()
        .enumerate()
        .filter_map(|(i, r)| parse_rule(r, i, cat, true).ok())
        .collect()
}

// ---------------------------------------------------------------- layout

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutIn {
    modes: ModesIn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModesIn {
    create: ModeIn,
    /// `"same_as_create"` (default) or its own sections.
    #[serde(default)]
    update: Option<Json>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeIn {
    sections: Vec<SectionIn>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SectionIn {
    id: String,
    title: String,
    #[serde(default)]
    columns: Option<u8>,
    #[serde(default)]
    show_if: Option<Json>,
    fields: Vec<FieldIn>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldIn {
    key: String,
    #[serde(default)]
    span: Option<u8>,
    #[serde(default)]
    help: Option<String>,
    #[serde(default)]
    show_if: Option<Json>,
}

/// A section of the effective layout (§3.6.1).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LayoutSection {
    pub id: String,
    pub title: String,
    pub columns: u8,
    /// A display hint, not a rule (the same condition language as rules).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_if: Option<Json>,
    pub fields: Vec<LayoutField>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LayoutField {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_if: Option<Json>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Layout {
    pub sections: Vec<LayoutSection>,
}

/// Fields a layout may place: the built-in command keys and `status_id`.
const LAYOUT_BUILTINS: [&str; 9] = [
    "subject",
    "description",
    "assigned_to_id",
    "priority_id",
    "fixed_version_id",
    "start_date",
    "due_date",
    "estimated_hours",
    "status_id",
];

/// The condition field a layout key's value appears under.
fn cond_name(key: &str) -> &str {
    match key {
        "assigned_to_id" => "assignee",
        "priority_id" => "priority",
        "fixed_version_id" => "fixed_version",
        "status_id" => "status",
        k => k,
    }
}

/// A field that is required in some situation: unconditionally (`None`) or
/// under these `required_if` conditions.
pub(crate) type Required = BTreeMap<String, Option<Vec<When>>>;

/// Whether `req` being true implies every show clause is true (a
/// structural check: the same condition, or `all` items that contain the
/// clause's items).
fn implies(req: &When, shows: &[&When]) -> bool {
    let req_items: Vec<Json> = req
        .all_items
        .clone()
        .unwrap_or_else(|| vec![req.json.clone()]);
    shows.iter().all(|s| {
        if s.json == req.json {
            return true;
        }
        let items: Vec<Json> = s.all_items.clone().unwrap_or_else(|| vec![s.json.clone()]);
        items.iter().all(|i| req_items.contains(i))
    })
}

fn parse_mode(
    m: &ModeIn,
    path: &str,
    cat: &BTreeMap<i32, CfInfo>,
    required: &Required,
) -> Result<()> {
    if m.sections.len() > MAX_SECTIONS {
        return Err(form_invalid(
            "malformed",
            "at most 30 sections",
            json!({"path": path}),
        ));
    }
    let mut ids = BTreeSet::new();
    // key → (section show_if, field show_if)
    let mut placed: BTreeMap<String, Vec<When>> = BTreeMap::new();
    let mut count = 0;
    for (si, s) in m.sections.iter().enumerate() {
        let sp = format!("{path}.sections[{si}]");
        let id_ok = !s.id.is_empty()
            && s.id.len() <= 40
            && s.id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !id_ok || !ids.insert(s.id.clone()) {
            return Err(form_invalid(
                "malformed",
                "section ids are unique, 1–40 characters of letters, digits, _ and -",
                json!({"path": format!("{sp}.id"), "section_id": s.id}),
            ));
        }
        let n = s.title.chars().count();
        if n == 0 || n > 100 {
            return Err(form_invalid(
                "malformed",
                "a section title is 1–100 characters",
                json!({"path": format!("{sp}.title"), "section_id": s.id}),
            ));
        }
        if s.columns.is_some_and(|c| !(1..=3).contains(&c)) {
            return Err(form_invalid(
                "malformed",
                "columns is 1–3",
                json!({"path": format!("{sp}.columns"), "section_id": s.id}),
            ));
        }
        let section_show = match &s.show_if {
            Some(w) => Some(
                parse_when(w, &format!("{sp}.show_if"), cat, false)
                    .map_err(|e| with_detail(e, "section_id", json!(s.id)))?,
            ),
            None => None,
        };
        for (fi, f) in s.fields.iter().enumerate() {
            let fp = format!("{sp}.fields[{fi}]");
            count += 1;
            if count > MAX_LAYOUT_FIELDS {
                return Err(form_invalid(
                    "malformed",
                    "at most 200 fields",
                    json!({"path": fp}),
                ));
            }
            let known = LAYOUT_BUILTINS.contains(&f.key.as_str())
                || matches!(Key::parse(&f.key), Some(Key::Cf(id)) if cat.get(&id).is_some_and(|c| !c.inactive));
            if !known {
                return Err(form_invalid(
                    "unknown_field",
                    format!("unknown field {:?}", f.key),
                    json!({"path": format!("{fp}.key"), "section_id": s.id}),
                ));
            }
            if let Some(Key::Cf(id)) = Key::parse(&f.key)
                && !cat[&id].applies
            {
                return Err(form_invalid(
                    "field_not_applicable",
                    format!("{} does not apply to this tracker", f.key),
                    json!({"path": format!("{fp}.key"), "section_id": s.id}),
                ));
            }
            if f.span.is_some_and(|c| !(1..=3).contains(&c))
                || f.help.as_ref().is_some_and(|h| h.chars().count() > 500)
            {
                return Err(form_invalid(
                    "malformed",
                    "span is 1–3 and help at most 500 characters",
                    json!({"path": fp, "section_id": s.id}),
                ));
            }
            let mut shows: Vec<When> = section_show.iter().cloned().collect();
            if let Some(w) = &f.show_if {
                shows.push(
                    parse_when(w, &format!("{fp}.show_if"), cat, false)
                        .map_err(|e| with_detail(e, "section_id", json!(s.id)))?,
                );
            }
            if placed.insert(f.key.clone(), shows).is_some() {
                return Err(form_invalid(
                    "malformed",
                    format!("{} is placed twice", f.key),
                    json!({"path": format!("{fp}.key"), "section_id": s.id}),
                ));
            }
        }
    }
    // A field shown only under a condition must not be required where it
    // is hidden.
    for (key, shows) in &placed {
        if shows.is_empty() {
            continue;
        }
        let refs: Vec<&When> = shows.iter().collect();
        let hidden_required = match required.get(key) {
            None => false,
            Some(None) => true,
            Some(Some(reqs)) => reqs.iter().any(|r| !implies(r, &refs)),
        };
        if hidden_required {
            return Err(form_invalid(
                "hides_required",
                format!("{key} is required in some cases but show_if can hide it"),
                json!({"path": path, "field": key}),
            ));
        }
    }
    // Display conditions must not depend on each other in a circle.
    let deps: BTreeMap<&str, BTreeSet<String>> = placed
        .iter()
        .map(|(k, shows)| {
            let d: BTreeSet<String> = shows
                .iter()
                .flat_map(|w| w.fields.iter().cloned())
                .collect();
            (k.as_str(), d)
        })
        .collect();
    let node = |cond: &str| -> Option<&str> {
        placed
            .keys()
            .find(|k| cond_name(k) == cond)
            .map(String::as_str)
    };
    // 0 = new, 1 = on the stack, 2 = done.
    let mut mark: BTreeMap<&str, u8> = BTreeMap::new();
    fn visit<'a>(
        k: &'a str,
        deps: &BTreeMap<&'a str, BTreeSet<String>>,
        node: &dyn Fn(&str) -> Option<&'a str>,
        mark: &mut BTreeMap<&'a str, u8>,
    ) -> Option<&'a str> {
        match mark.get(k) {
            Some(1) => return Some(k),
            Some(2) => return None,
            _ => {}
        }
        mark.insert(k, 1);
        for d in deps.get(k).into_iter().flatten() {
            if let Some(n) = node(d)
                && let Some(c) = visit(n, deps, node, mark)
            {
                return Some(c);
            }
        }
        mark.insert(k, 2);
        None
    }
    for k in deps.keys() {
        if let Some(c) = visit(k, &deps, &node, &mut mark) {
            return Err(form_invalid(
                "cycle",
                "display conditions depend on each other",
                json!({"path": path, "field": c}),
            ));
        }
    }
    Ok(())
}

fn with_detail(e: anyhow::Error, key: &str, v: Json) -> anyhow::Error {
    match crate::error::classify(&e) {
        Some(mut a) => {
            if let Some(Json::Object(d)) = a.details.as_mut() {
                d.insert(key.into(), v);
            }
            a.into()
        }
        None => e,
    }
}

/// Validates a layout body against the rules in force.
pub(crate) fn parse_layout(
    body: &Json,
    cat: &BTreeMap<i32, CfInfo>,
    required: &Required,
) -> Result<()> {
    let l: LayoutIn = serde_json::from_value(body.clone()).map_err(|e| {
        form_invalid(
            "malformed",
            format!("the layout body is {{\"modes\": {{\"create\": …, \"update\": …}}}}: {e}"),
            json!({"path": ""}),
        )
    })?;
    parse_mode(&l.modes.create, "modes.create", cat, required)?;
    match &l.modes.update {
        None => {}
        Some(Json::String(s)) if s == "same_as_create" => {}
        Some(v) => {
            let m: ModeIn = serde_json::from_value(v.clone()).map_err(|e| {
                form_invalid(
                    "malformed",
                    format!("update is \"same_as_create\" or {{\"sections\": [...]}}: {e}"),
                    json!({"path": "modes.update"}),
                )
            })?;
            parse_mode(&m, "modes.update", cat, required)?;
        }
    }
    Ok(())
}

/// The sections of `op` in a published layout body (no checks: it was
/// valid when published).
fn layout_sections(body: &Json, op: &str) -> Option<Vec<SectionIn>> {
    let l: LayoutIn = serde_json::from_value(body.clone()).ok()?;
    let mode = match (op, &l.modes.update) {
        ("update", Some(v)) if v.is_object() => serde_json::from_value::<ModeIn>(v.clone()).ok()?,
        _ => l.modes.create,
    };
    Some(mode.sections)
}

/// What is required in some situation for a layout check: definition
/// required fields (`subject`, required custom fields), fields a workflow
/// makes required in a status (`workflow`: (status, field key), for any
/// role) and the targets of `required_if`.
pub(crate) fn required_map(
    cat: &BTreeMap<i32, CfInfo>,
    rules: &[Rule],
    workflow: &[(Uuid, String)],
) -> Required {
    let mut r: Required = BTreeMap::new();
    r.insert("subject".into(), None);
    for (id, c) in cat {
        if c.is_required && c.applies && !c.inactive {
            r.insert(format!("cf:{id}"), None);
        }
    }
    for (status, key) in workflow {
        let when = json!({"all": [{"field": "status", "op": "eq", "value": status}]});
        let Ok(w) = parse_when(&when, "workflow", cat, true) else {
            continue;
        };
        match r.entry(key.clone()).or_insert_with(|| Some(Vec::new())) {
            None => {}
            Some(v) => v.push(w),
        }
    }
    for rule in rules.iter().filter(|x| x.kind == RuleKind::RequiredIf) {
        match r
            .entry(rule.field.clone())
            .or_insert_with(|| Some(Vec::new()))
        {
            None => {}
            Some(v) => v.push(rule.when.clone()),
        }
    }
    r
}

// ---------------------------------------------------------------- versions and the effective form

#[derive(Debug, Clone, sqlx::FromRow)]
struct FormRow {
    id: Uuid,
    tracker_id: Uuid,
    project_id: Option<Uuid>,
    published_rules_version: i32,
    published_layout_version: i32,
    lock_version: i32,
}

const FORM_COLS: &str =
    "id, tracker_id, project_id, published_rules_version, published_layout_version, lock_version";

/// The rules and layout in force for (project, tracker).
#[derive(Debug, Clone)]
pub(crate) struct Effective {
    /// Site rules, then project rules.
    pub rules: Vec<Rule>,
    pub rules_version: String,
    pub layout_version: String,
    /// The published layout body in force (project over site), if any.
    layout: Option<Json>,
    /// Status id → category.
    pub categories: BTreeMap<Uuid, String>,
}

impl Effective {
    pub fn category(&self, status: Uuid) -> Option<&str> {
        self.categories.get(&status).map(String::as_str)
    }
}

/// The counters behind the version strings of (project, tracker):
/// (site rules, project rules, site layout, project layout, L0, workflow).
async fn counters(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
) -> Result<(i32, i32, i32, i32, i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT COALESCE((SELECT published_rules_version FROM form_defs WHERE tracker_id = $1 AND project_id IS NULL), 0),
                COALESCE((SELECT published_rules_version FROM form_defs WHERE tracker_id = $1 AND project_id = $2), 0),
                COALESCE((SELECT published_layout_version FROM form_defs WHERE tracker_id = $1 AND project_id IS NULL), 0),
                COALESCE((SELECT published_layout_version FROM form_defs WHERE tracker_id = $1 AND project_id = $2), 0),
                form_l0_version, workflow_version
         FROM org_settings",
    )
    .bind(tracker)
    .bind(project)
    .fetch_one(&mut *conn)
    .await?)
}

fn version_strings(c: (i32, i32, i32, i32, i64, i64)) -> (String, String) {
    (
        format!("r{}.{}.s{}.w{}", c.0, c.1, c.4, c.5),
        format!("l{}.{}", c.2, c.3),
    )
}

/// The effective rules version of (project, tracker).
pub(crate) async fn rules_version(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
) -> Result<String> {
    Ok(version_strings(counters(conn, project, tracker).await?).0)
}

/// The rules version of a set of (project, tracker) pairs: one version, or
/// several joined by `,` in pair order.
pub(crate) async fn pairs_version(
    conn: &mut PgConnection,
    pairs: &BTreeSet<(Uuid, Uuid)>,
) -> Result<Option<String>> {
    if pairs.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for (p, t) in pairs {
        out.push(rules_version(&mut *conn, *p, *t).await?);
    }
    out.dedup();
    Ok(Some(out.join(",")))
}

/// The (project, tracker) pairs a stored command document touches.
pub(crate) async fn doc_pairs(
    conn: &mut PgConnection,
    body: &Json,
) -> Result<BTreeSet<(Uuid, Uuid)>> {
    let mut pairs = BTreeSet::new();
    let mut issues: Vec<Uuid> = Vec::new();
    for op in body["ops"].as_array().into_iter().flatten() {
        if matches!(
            op["kind"].as_str(),
            Some("log_time" | "update_time_entry" | "delete_time_entry")
        ) {
            continue;
        }
        if op["kind"] == "create_issue" {
            if let (Some(p), Some(t)) = (
                op["project_id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok()),
                op["tracker_id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok()),
            ) {
                pairs.insert((p, t));
            }
            continue;
        }
        for k in ["target", "source", "duplicate_of"] {
            if op[k]["type"] == "issue"
                && let Some(id) = op[k]["id"].as_str().and_then(|s| Uuid::parse_str(s).ok())
            {
                issues.push(id);
            }
        }
    }
    let rows: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT project_id, tracker_id FROM issues WHERE id = ANY($1)")
            .bind(&issues)
            .fetch_all(&mut *conn)
            .await?;
    pairs.extend(rows);
    Ok(pairs)
}

/// Whether a proposal stored under `stored` meets other rules now
/// (§3.6.3: the queue flags it and it cannot be ticked). A version stored
/// before 0024 (`None`) counts as changed once any L1 rules are in force.
pub(crate) async fn rules_changed(
    conn: &mut PgConnection,
    body: &Json,
    stored: Option<&str>,
) -> Result<bool> {
    let pairs = doc_pairs(&mut *conn, body).await?;
    let Some(now) = pairs_version(&mut *conn, &pairs).await? else {
        return Ok(false);
    };
    Ok(match stored {
        Some(s) => s != now,
        None => now.split(',').any(|v| !v.starts_with("r0.0.")),
    })
}

/// Locks the form rows of the pairs FOR SHARE (a publish takes FOR UPDATE,
/// so a write sees the rules before or after it, never half).
pub(crate) async fn lock_forms(
    conn: &mut PgConnection,
    pairs: &BTreeSet<(Uuid, Uuid)>,
) -> Result<()> {
    let trackers: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
    let projects: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    sqlx::query(
        "SELECT 1 FROM form_defs WHERE tracker_id = ANY($1) AND (project_id IS NULL OR project_id = ANY($2))
         ORDER BY id FOR SHARE",
    )
    .bind(&trackers)
    .bind(&projects)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn published_body(
    conn: &mut PgConnection,
    table: &str,
    form: Uuid,
    version: i32,
) -> Result<Option<Json>> {
    if version == 0 {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT body FROM {table} WHERE form_id = $1 AND version = $2"
    )))
    .bind(form)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn form_rows(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
) -> Result<(Option<FormRow>, Option<FormRow>)> {
    let rows: Vec<FormRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FORM_COLS} FROM form_defs WHERE tracker_id = $1 AND (project_id IS NULL OR project_id = $2)"
    )))
    .bind(tracker)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    let site = rows.iter().find(|r| r.project_id.is_none()).cloned();
    let proj = rows.into_iter().find(|r| r.project_id.is_some());
    Ok((site, proj))
}

async fn categories(conn: &mut PgConnection) -> Result<BTreeMap<Uuid, String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, category FROM issue_statuses")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Loads the effective form of (project, tracker).
pub(crate) async fn effective(
    conn: &mut PgConnection,
    project: Uuid,
    tracker: Uuid,
) -> Result<Effective> {
    let (site, proj) = form_rows(&mut *conn, project, tracker).await?;
    let cat = catalog(&mut *conn, tracker, Some(project)).await?;
    let mut rules = Vec::new();
    for f in [&site, &proj].into_iter().flatten() {
        if let Some(b) = published_body(
            &mut *conn,
            "form_rule_versions",
            f.id,
            f.published_rules_version,
        )
        .await?
        {
            rules.extend(load_rules(&b, &cat));
        }
    }
    let layout = match (&proj, &site) {
        (Some(p), _) if p.published_layout_version > 0 => {
            published_body(
                &mut *conn,
                "form_layout_versions",
                p.id,
                p.published_layout_version,
            )
            .await?
        }
        (_, Some(s)) => {
            published_body(
                &mut *conn,
                "form_layout_versions",
                s.id,
                s.published_layout_version,
            )
            .await?
        }
        _ => None,
    };
    let (rules_version, layout_version) =
        version_strings(counters(&mut *conn, project, tracker).await?);
    Ok(Effective {
        rules,
        rules_version,
        layout_version,
        layout,
        categories: categories(&mut *conn).await?,
    })
}

// ---------------------------------------------------------------- the engine's checks

/// A value is empty (command form).
fn empty(v: Option<&Json>) -> bool {
    v.is_none_or(|v| v.is_null() || v.as_str() == Some(""))
}

/// `readonly_if` rules on `field` that hold in the state at the start of
/// the transaction.
pub(crate) fn readonly_rules<'a>(
    form: &'a Effective,
    field: &'a str,
    baseline: &'a State,
) -> impl Iterator<Item = &'a Rule> + 'a {
    form.rules.iter().filter(move |r| {
        r.kind == RuleKind::ReadonlyIf && r.field == field && baseline.holds(&r.when)
    })
}

/// `required_if` rules that hold in the final state and whose target is empty.
pub(crate) fn missing<'a>(
    form: &'a Effective,
    fin: &State,
    values: &BTreeMap<String, Json>,
) -> Vec<&'a Rule> {
    form.rules
        .iter()
        .filter(|r| {
            r.kind == RuleKind::RequiredIf && empty(values.get(&r.field)) && fin.holds(&r.when)
        })
        .collect()
}

/// `allowed_values_if` rules broken by a newly assigned value.
pub(crate) fn disallowed<'a>(
    form: &'a Effective,
    fin: &State,
    values: &BTreeMap<String, Json>,
    orig: Option<&BTreeMap<String, Json>>,
) -> Vec<&'a Rule> {
    form.rules
        .iter()
        .filter(|r| {
            if r.kind != RuleKind::AllowedValuesIf {
                return false;
            }
            let Some(v) = values.get(&r.field).filter(|v| !empty(Some(v))) else {
                return false;
            };
            let kept = orig.is_some_and(|o| o.get(&r.field) == Some(v));
            !kept && !r.values.contains(v) && fin.holds(&r.when)
        })
        .collect()
}

/// Rules whose condition holds in `state` and whose target is broken by it
/// (for impact reports and marks): `required_if` with an empty target,
/// `allowed_values_if` with a value outside the list.
pub(crate) fn violations(
    rules: &[Rule],
    state: &State,
    values: &BTreeMap<String, Json>,
) -> Vec<String> {
    rules
        .iter()
        .filter(|r| match r.kind {
            RuleKind::RequiredIf => empty(values.get(&r.field)) && state.holds(&r.when),
            RuleKind::AllowedValuesIf => values
                .get(&r.field)
                .filter(|v| !empty(Some(v)))
                .is_some_and(|v| !r.values.contains(v) && state.holds(&r.when)),
            RuleKind::ReadonlyIf => false,
        })
        .map(|r| r.id.clone())
        .collect()
}

// ---------------------------------------------------------------- describe_form (§3.6.1)

/// MCP `describe_form`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeFormParams {
    /// Project key (e.g. "AUTH") or id.
    pub project: String,
    /// Tracker id or name (e.g. "Bug").
    pub tracker: String,
    /// "create" or "update".
    pub op: String,
    /// op "update": the issue (e.g. "AUTH-12"); its current status' rules are added.
    #[serde(default)]
    pub issue_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TrackerRef {
    pub id: Uuid,
    pub name: String,
}

/// One rule as `describe_form` lists it on its target field.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuleRef {
    pub rule_id: String,
    pub when: Json,
    /// `allowed_values_if`: the values allowed while `when` holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Json>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormField {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub field_type: String,
    /// Required always (definition, or workflow at the status in question).
    pub required: bool,
    pub editable: bool,
    /// Why not editable: `not_editable`, `workflow_readonly`,
    /// `project_not_writable`, `policy` (delegation off), `form_rule`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Agents: `"review"` when writing it sends the document for review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<OptionView>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_if: Vec<RuleRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_values_if: Vec<RuleRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readonly_if: Vec<RuleRef>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormCreation {
    pub allowed: bool,
    pub initial_status_id: Option<Uuid>,
    pub enters_triage: bool,
    pub is_private: describe::PrivateChoice,
}

/// `GET /projects/{id}/trackers/{t}/form` and MCP `describe_form`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormDescription {
    pub project: String,
    pub project_id: Uuid,
    pub tracker: TrackerRef,
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_key: Option<String>,
    /// op update: the issue's lock_version (for the precondition).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_version: Option<i32>,
    /// Send it back as expected_rules_version to detect rule changes.
    pub rules_version: String,
    pub layout_version: String,
    pub schema_version: i64,
    pub fields: Vec<FormField>,
    /// A display hint only; every field you may set is in some section.
    pub layout: Layout,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation: Option<FormCreation>,
}

/// REST query of the form endpoint.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FormQuery {
    /// "create" (default) or "update".
    #[serde(default)]
    pub op: Option<String>,
    /// op update: the issue key.
    #[serde(default)]
    pub issue: Option<String>,
}

/// The agent's delegation policies in `project` (None for people).
async fn agent_policies(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<Option<crate::policy::Policies>> {
    let Some(context) = caller.context_id() else {
        return Ok(None);
    };
    if caller.kind() != crate::auth::PrincipalKind::Agent {
        return Ok(None);
    }
    let d: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM delegations WHERE context_id = $1 AND project_id = $2
           AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(context)
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match d {
        Some(d) => Some(crate::policy::load(&mut *conn, d).await?),
        None => Some(crate::policy::Policies::default()),
    })
}

pub async fn describe_form(
    app: &App,
    caller: AuthContext,
    project: &str,
    tracker: &str,
    op: &str,
    issue_key: Option<&str>,
) -> Result<FormDescription> {
    let mut tx = read_tx(app).await?;
    let r = describe_form_in(&mut tx, caller, project, tracker, op, issue_key).await?;
    tx.commit().await?;
    Ok(r)
}

fn arg_err(path: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

async fn describe_form_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
    tracker: &str,
    op: &str,
    issue_key: Option<&str>,
) -> Result<FormDescription> {
    match (op, issue_key) {
        ("create", None) | ("update", Some(_)) => {}
        ("create", Some(_)) => return Err(arg_err("issue_key", "op create takes no issue")),
        ("update", None) => return Err(arg_err("issue_key", "op update needs the issue key")),
        _ => return Err(arg_err("op", "op is \"create\" or \"update\"")),
    }
    let schema = describe::tracker_schema_in(&mut *conn, caller, project, tracker).await?;
    let (p, t) = (schema.project_id, schema.tracker_id);
    let actions = match issue_key {
        Some(k) => {
            let a = describe::issue_actions_in(&mut *conn, caller, k).await?;
            let (ip, it): (Uuid, Uuid) =
                sqlx::query_as("SELECT project_id, tracker_id FROM issues WHERE id = $1")
                    .bind(a.issue_id)
                    .fetch_one(&mut *conn)
                    .await?;
            if ip != p || it != t {
                return Err(arg_err(
                    "issue_key",
                    "the issue is not of this project and tracker",
                ));
            }
            Some(a)
        }
        None => None,
    };
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, p)
        .await?
        .ok_or_else(|| not_found("project"))?;
    let form = effective(&mut *conn, p, t).await?;
    let (pkey, tname): (String, String) = sqlx::query_as(
        "SELECT (SELECT key FROM projects WHERE id = $1), (SELECT name FROM trackers WHERE id = $2)",
    )
    .bind(p)
    .bind(t)
    .fetch_one(&mut *conn)
    .await?;
    let policies = agent_policies(&mut *conn, caller, p).await?;
    let command = if op == "create" {
        perm::OP_CREATE_ISSUE
    } else {
        perm::OP_UPDATE_FIELDS
    };
    // Workflow rules at the initial status (create).
    let initial_rules = match (op, schema.creation.initial_status_id) {
        ("create", Some(s)) => {
            let roles = pp.workflow_roles();
            let by = describe::rules_by_status(&mut *conn, t, &roles).await?;
            let cfs = describe::applicable_fields(&mut *conn, p, t).await?;
            let hidden: Vec<ox_core::workflow::HiddenField> = cfs
                .iter()
                .filter(|c| !c.visible)
                .map(|c| ox_core::workflow::HiddenField {
                    field_key: c.key(),
                    roles: c.roles.clone(),
                })
                .collect();
            ox_core::workflow::field_rules(
                by.get(&s).map(Vec::as_slice).unwrap_or(&[]),
                &roles,
                &hidden,
            )
        }
        _ => BTreeMap::new(),
    };
    let readable: BTreeSet<String> = schema.fields.iter().map(|f| f.key.clone()).collect();
    let cfs_here: BTreeSet<i32> = describe::applicable_fields(&mut *conn, p, t)
        .await?
        .iter()
        .map(|c| c.id)
        .collect();
    let caller_is_agent = actor.is_agent();
    // Current values for readonly_if on update.
    let baseline = match &actions {
        Some(a) => Some(issue_state(&mut *conn, a.issue_id, &form).await?),
        None => None,
    };
    let mut fields = Vec::new();
    for f in &schema.fields {
        if f.key == "status_id" && op == "create" {
            continue;
        }
        let (mut editable, mut required, mut reason) = match &actions {
            None => {
                let rule = initial_rules.get(&f.key).copied();
                let can = f.ops.iter().any(|o| o == "create");
                let ro = rule == Some(ox_core::workflow::Rule::Readonly);
                (
                    can && !ro,
                    f.required || rule == Some(ox_core::workflow::Rule::Required),
                    if !can {
                        Some("not_editable".to_string())
                    } else if ro {
                        Some("workflow_readonly".to_string())
                    } else {
                        None
                    },
                )
            }
            Some(a) => {
                if f.key == "status_id" {
                    let can = a.transitions.iter().any(|t| !t.blocked);
                    (can, false, (!can).then(|| "not_editable".to_string()))
                } else {
                    match a.fields.iter().find(|x| x.key == f.key) {
                        Some(x) => (x.editable, x.required, x.reason.clone()),
                        None => (false, f.required, Some("not_editable".to_string())),
                    }
                }
            }
        };
        let mut policy = None;
        if let Some(pol) = &policies
            && f.key != "status_id"
        {
            let items = [(0usize, command, vec![f.key.clone()])];
            match pol.decide_items(&items) {
                crate::policy::Decision::Off { .. } => {
                    editable = false;
                    reason = Some("policy".into());
                }
                crate::policy::Decision::Review => policy = Some("review".to_string()),
                crate::policy::Decision::Direct => {}
            }
        }
        // Rules on this field the caller may see: the target and every
        // condition field readable.
        let visible_rule = |r: &&Rule| {
            r.field == f.key
                && r.when.fields.iter().all(|c| {
                    let k = match c.as_str() {
                        "assignee" => "assigned_to_id",
                        "priority" => "priority_id",
                        "fixed_version" => "fixed_version_id",
                        "status" | "status_category" | "tracker" | "is_private" => {
                            return true;
                        }
                        other => other,
                    };
                    readable.contains(k)
                })
        };
        let refs = |kind: RuleKind| -> Vec<RuleRef> {
            form.rules
                .iter()
                .filter(|r| r.kind == kind)
                .filter(visible_rule)
                .map(|r| RuleRef {
                    rule_id: r.id.clone(),
                    when: r.when.json.clone(),
                    values: (kind == RuleKind::AllowedValuesIf).then(|| r.values.clone()),
                })
                .collect()
        };
        let readonly_if = refs(RuleKind::ReadonlyIf);
        // As the engine: a person is held to the rules they can see; an
        // agent also to those naming a restricted field (blocked, unnamed).
        let restricted_cf = |r: &&Rule| {
            r.when.fields.iter().any(|c| {
                matches!(Key::parse(c), Some(Key::Cf(id))
                    if !readable.contains(c.as_str()) && cfs_here.contains(&id))
            })
        };
        if let Some(b) = &baseline
            && editable
            && form.rules.iter().any(|r| {
                r.kind == RuleKind::ReadonlyIf
                    && r.field == f.key
                    && b.holds(&r.when)
                    && (visible_rule(&r) || (caller_is_agent && restricted_cf(&r)))
            })
        {
            editable = false;
            reason = Some("form_rule".into());
        }
        if f.key == "subject" {
            required = true;
        }
        fields.push(FormField {
            key: f.key.clone(),
            name: f.name.clone(),
            field_type: f.field_type.clone(),
            required,
            editable,
            reason,
            policy,
            constraints: f.constraints.clone(),
            options_source: f.options_source.clone(),
            options: f.options.clone(),
            required_if: refs(RuleKind::RequiredIf),
            allowed_values_if: refs(RuleKind::AllowedValuesIf),
            readonly_if,
        });
    }
    let listed: Vec<String> = fields.iter().map(|f| f.key.clone()).collect();
    let layout = effective_layout(form.layout.as_ref(), op, &listed);
    Ok(FormDescription {
        project: pkey,
        project_id: p,
        tracker: TrackerRef { id: t, name: tname },
        op: op.into(),
        issue_key: actions.as_ref().map(|a| a.key.clone()),
        lock_version: actions.as_ref().map(|a| a.lock_version),
        rules_version: form.rules_version,
        layout_version: form.layout_version,
        schema_version: schema.schema_version,
        fields,
        layout,
        creation: (op == "create").then(|| FormCreation {
            allowed: schema.creation.allowed,
            initial_status_id: schema.creation.initial_status_id,
            enters_triage: schema.creation.enters_triage,
            is_private: schema.creation.is_private.clone(),
        }),
    })
}

/// The layout as shown: the published sections with fields the caller
/// cannot see left out (and display conditions on them dropped), then an
/// "其他" section with every listed field no section placed. Without a
/// published layout: one section with every field.
fn effective_layout(body: Option<&Json>, op: &str, listed: &[String]) -> Layout {
    let Some(sections) = body.and_then(|b| layout_sections(b, op)) else {
        return Layout {
            sections: vec![LayoutSection {
                id: "default".into(),
                title: "基本".into(),
                columns: 2,
                show_if: None,
                fields: listed
                    .iter()
                    .map(|k| LayoutField {
                        key: k.clone(),
                        span: None,
                        help: None,
                        show_if: None,
                    })
                    .collect(),
            }],
        };
    };
    let cond_ok = |w: &Option<Json>| -> Option<Json> {
        let w = w.as_ref()?;
        let text = w.to_string();
        // A condition on a field the caller cannot see is dropped (shown always).
        let hidden = text.contains("\"cf:")
            && w.to_string()
                .split("\"field\":\"")
                .skip(1)
                .filter_map(|s| s.split('"').next())
                .filter(|f| f.starts_with("cf:"))
                .any(|f| !listed.iter().any(|l| l == f));
        (!hidden).then(|| w.clone())
    };
    let mut placed: BTreeSet<String> = BTreeSet::new();
    // A section whose every field the caller cannot see is left out (its
    // title would say what it holds).
    let mut out: Vec<LayoutSection> = sections
        .into_iter()
        .filter_map(|s| {
            let had = !s.fields.is_empty();
            let fields: Vec<LayoutField> = s
                .fields
                .into_iter()
                .filter(|f| listed.contains(&f.key))
                .map(|f| {
                    placed.insert(f.key.clone());
                    LayoutField {
                        key: f.key,
                        span: f.span,
                        help: f.help,
                        show_if: cond_ok(&f.show_if),
                    }
                })
                .collect();
            if had && fields.is_empty() {
                return None;
            }
            Some(LayoutSection {
                id: s.id,
                title: s.title,
                columns: s.columns.unwrap_or(1),
                show_if: cond_ok(&s.show_if),
                fields,
            })
        })
        .collect();
    let rest: Vec<LayoutField> = listed
        .iter()
        .filter(|k| !placed.contains(*k))
        .map(|k| LayoutField {
            key: k.clone(),
            span: None,
            help: None,
            show_if: None,
        })
        .collect();
    if !rest.is_empty() {
        out.push(LayoutSection {
            id: "_other".into(),
            title: "其他".into(),
            columns: 1,
            show_if: None,
            fields: rest,
        });
    }
    Layout { sections: out }
}

/// An issue's current state for conditions.
async fn issue_state(conn: &mut PgConnection, issue: Uuid, form: &Effective) -> Result<State> {
    let (tracker, status, private, values) = issue_values(&mut *conn, issue).await?;
    Ok(State::of(
        tracker,
        status,
        form.category(status),
        private,
        values.iter().map(|(k, v)| (k.clone(), v)),
    ))
}

/// (tracker, status, is_private, values in command form) of an issue.
async fn issue_values(
    conn: &mut PgConnection,
    issue: Uuid,
) -> Result<(Uuid, Uuid, bool, BTreeMap<String, Json>)> {
    let Some((tracker, status, private, values)) = commands::issue_state(&mut *conn, issue).await?
    else {
        return Err(not_found("issue"));
    };
    Ok((tracker, status, private, values))
}

/// R19: `then_required` of `GET /issues/{key}/actions` also names the
/// targets of `required_if` rules that hold once the issue is in `to`.
pub(crate) async fn then_required(
    conn: &mut PgConnection,
    issue: Uuid,
    project: Uuid,
    tracker: Uuid,
    to: Uuid,
) -> Result<Vec<String>> {
    let form = effective(&mut *conn, project, tracker).await?;
    if form.rules.is_empty() {
        return Ok(Vec::new());
    }
    let (tracker, _, private, values) = issue_values(&mut *conn, issue).await?;
    let s = State::of(
        tracker,
        to,
        form.category(to),
        private,
        values.iter().map(|(k, v)| (k.clone(), v)),
    );
    Ok(form
        .rules
        .iter()
        .filter(|r| r.kind == RuleKind::RequiredIf && s.holds(&r.when))
        .map(|r| r.field.clone())
        .collect())
}

// ---------------------------------------------------------------- administration (§3.6.3)

/// Rules or layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Rules,
    Layout,
}

impl Part {
    pub fn parse(s: &str) -> Result<Part> {
        match s {
            "rules" => Ok(Part::Rules),
            "layout" => Ok(Part::Layout),
            _ => Err(not_found("form part")),
        }
    }
    fn table(self) -> &'static str {
        match self {
            Part::Rules => "form_rule_versions",
            Part::Layout => "form_layout_versions",
        }
    }
    fn published_col(self) -> &'static str {
        match self {
            Part::Rules => "published_rules_version",
            Part::Layout => "published_layout_version",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormSummary {
    pub id: Uuid,
    pub tracker_id: Uuid,
    pub tracker_name: String,
    /// `null`: site level.
    pub project_id: Option<Uuid>,
    pub project_key: Option<String>,
    pub published_rules_version: i32,
    pub published_layout_version: i32,
    pub rules_draft: Option<i32>,
    pub layout_draft: Option<i32>,
    pub lock_version: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct FormVersionView {
    pub version: i32,
    /// draft, published, superseded, discarded.
    pub state: String,
    pub body: Json,
    pub publish_mode: Option<String>,
    pub impact: Option<Json>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub published_by: Option<Uuid>,
    pub published_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PartView {
    pub published: Option<FormVersionView>,
    pub draft: Option<FormVersionView>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormDetail {
    #[serde(flatten)]
    pub summary: FormSummary,
    pub rules: PartView,
    pub layout: PartView,
    /// Effective rules version of this form's layer (site: rN.0…).
    pub rules_version: String,
    pub layout_version: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewForm {
    pub tracker_id: Uuid,
    /// Project key or id; omit for the site-level form.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftInput {
    /// The current draft's version, or (no draft) the published version (0: none).
    pub base_version: i32,
    pub body: Json,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishInput {
    pub draft_version: i32,
    /// Rules: `block` or `accept_and_mark`. Layout: omit.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackInput {
    /// "rules" or "layout".
    pub part: String,
    pub to_version: i32,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExistingIssues {
    pub violating: i64,
    pub sample_keys: Vec<String>,
    pub scanned: i64,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PendingChangesets {
    pub would_fail: i64,
    pub ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UndoRecords {
    pub not_applicable_after: i64,
}

/// `POST /forms/{id}/drafts/{part}/impact` (§3.6.3). Counts only what the
/// caller can see; sample keys are visible issues.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FormImpact {
    pub draft_version: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_version_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_version_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_issues: Option<ExistingIssues>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_changesets: Option<PendingChangesets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo_records: Option<UndoRecords>,
    pub layout_warnings: Vec<Json>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PublishResult {
    pub form_id: Uuid,
    pub part: String,
    pub version: i32,
    pub rules_version: String,
    pub layout_version: String,
    /// Issues marked as not meeting the new rules (accept_and_mark).
    pub marked: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FormsQuery {
    /// A project key or id: that project's forms; omitted: the site forms.
    #[serde(default)]
    pub project: Option<String>,
}

/// Who may manage a form layer: administrators; for a project form also
/// holders of manage_project in that (active) project. Returns the user.
async fn authorize(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Option<Uuid>,
) -> Result<Uuid> {
    let who = commands::executor(&caller)?;
    if db::is_admin(&mut *conn, caller).await? {
        return Ok(who.user_id);
    }
    let Some(p) = project else {
        return Err(forbidden("only administrators manage site forms"));
    };
    let actor = perm::actor(&mut *conn, caller).await?;
    let scope = db::load_scope(&mut *conn, caller).await?;
    if !scope.project_ids().any(|x| x == p) {
        return Err(not_found("form"));
    }
    let ok = perm::project_perms(&mut *conn, &actor, p)
        .await?
        .is_some_and(|pp| pp.status == "active" && pp.allowed_any(MANAGE_PROJECT));
    if !ok {
        return Err(forbidden(
            "managing this project's forms needs manage_project",
        ));
    }
    Ok(who.user_id)
}

async fn load_form(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<FormRow> {
    let sql = format!(
        "SELECT {FORM_COLS} FROM form_defs WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| not_found("form"))
}

/// The form for an id the caller may manage (else `not_found` /
/// `forbidden_op`).
async fn managed_form(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    lock: bool,
) -> Result<(FormRow, Uuid)> {
    let f = load_form(&mut *conn, id, false).await?;
    let user = authorize(&mut *conn, caller, f.project_id).await?;
    let f = if lock {
        load_form(&mut *conn, id, true).await?
    } else {
        f
    };
    Ok((f, user))
}

const VERSION_COLS: &str = "version, state, body, publish_mode, impact, created_by, created_at, published_by, published_at";

async fn version_row(
    conn: &mut PgConnection,
    part: Part,
    form: Uuid,
    which: &str,
) -> Result<Option<FormVersionView>> {
    let cond = match which {
        "draft" => "state = 'draft'",
        _ => "state = 'published'",
    };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLS} FROM {} WHERE form_id = $1 AND {cond}",
        part.table()
    )))
    .bind(form)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn summary(conn: &mut PgConnection, f: &FormRow) -> Result<FormSummary> {
    let (tname, pkey): (String, Option<String>) = sqlx::query_as(
        "SELECT (SELECT name FROM trackers WHERE id = $1), (SELECT key FROM projects WHERE id = $2)",
    )
    .bind(f.tracker_id)
    .bind(f.project_id)
    .fetch_one(&mut *conn)
    .await?;
    let rd: Option<i32> = sqlx::query_scalar(
        "SELECT version FROM form_rule_versions WHERE form_id = $1 AND state = 'draft'",
    )
    .bind(f.id)
    .fetch_optional(&mut *conn)
    .await?;
    let ld: Option<i32> = sqlx::query_scalar(
        "SELECT version FROM form_layout_versions WHERE form_id = $1 AND state = 'draft'",
    )
    .bind(f.id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(FormSummary {
        id: f.id,
        tracker_id: f.tracker_id,
        tracker_name: tname,
        project_id: f.project_id,
        project_key: pkey,
        published_rules_version: f.published_rules_version,
        published_layout_version: f.published_layout_version,
        rules_draft: rd,
        layout_draft: ld,
        lock_version: f.lock_version,
    })
}

async fn detail(conn: &mut PgConnection, f: &FormRow) -> Result<FormDetail> {
    let s = summary(&mut *conn, f).await?;
    let rules = PartView {
        published: version_row(&mut *conn, Part::Rules, f.id, "published").await?,
        draft: version_row(&mut *conn, Part::Rules, f.id, "draft").await?,
    };
    let layout = PartView {
        published: version_row(&mut *conn, Part::Layout, f.id, "published").await?,
        draft: version_row(&mut *conn, Part::Layout, f.id, "draft").await?,
    };
    let (rv, lv) = layer_versions(&mut *conn, f).await?;
    Ok(FormDetail {
        summary: s,
        rules,
        layout,
        rules_version: rv,
        layout_version: lv,
    })
}

/// Version strings for a layer: a project form's are the effective ones of
/// its project; a site form's leave the project part 0.
async fn layer_versions(conn: &mut PgConnection, f: &FormRow) -> Result<(String, String)> {
    let mut c = counters(
        &mut *conn,
        f.project_id.unwrap_or(Uuid::nil()),
        f.tracker_id,
    )
    .await?;
    if f.project_id.is_none() {
        c.1 = 0;
        c.3 = 0;
    }
    Ok(version_strings(c))
}

async fn resolve_project(conn: &mut PgConnection, caller: AuthContext, p: &str) -> Result<Uuid> {
    describe::visible_project(&mut *conn, caller, p).await
}

/// `GET /admin/forms?project=`: the forms of the site (or of a project).
pub async fn list(app: &App, caller: AuthContext, q: &FormsQuery) -> Result<Vec<FormSummary>> {
    let mut tx = read_tx(app).await?;
    let project = match &q.project {
        Some(p) => Some(resolve_project(&mut tx, caller, p).await?),
        None => None,
    };
    authorize(&mut tx, caller, project).await?;
    let rows: Vec<FormRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FORM_COLS} FROM form_defs f WHERE project_id IS NOT DISTINCT FROM $1
         ORDER BY (SELECT position FROM trackers t WHERE t.id = f.tracker_id), tracker_id"
    )))
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(summary(&mut tx, r).await?);
    }
    tx.commit().await?;
    Ok(out)
}

/// `POST /admin/forms`: the form of (tracker, project?), created when
/// missing (naturally idempotent).
pub async fn ensure(app: &App, caller: AuthContext, input: &NewForm) -> Result<FormDetail> {
    let mut tx = app.pool.begin().await?;
    let project = match &input.project {
        Some(p) => Some(resolve_project(&mut tx, caller, p).await?),
        None => None,
    };
    authorize(&mut tx, caller, project).await?;
    let tracker_ok: bool = match project {
        Some(p) => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM project_trackers WHERE project_id = $1 AND tracker_id = $2)",
        )
        .bind(p)
        .bind(input.tracker_id)
        .fetch_one(&mut *tx)
        .await?,
        None => sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM trackers WHERE id = $1)")
            .bind(input.tracker_id)
            .fetch_one(&mut *tx)
            .await?,
    };
    if !tracker_ok {
        return Err(
            ApiError::new(ErrorCode::InvalidValue, "no such tracker here")
                .with_details(json!({"path": "tracker_id"}))
                .into(),
        );
    }
    sqlx::query(
        "INSERT INTO form_defs (tracker_id, project_id) VALUES ($1, $2)
         ON CONFLICT (tracker_id, project_id) DO NOTHING",
    )
    .bind(input.tracker_id)
    .bind(project)
    .execute(&mut *tx)
    .await?;
    let f: FormRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FORM_COLS} FROM form_defs WHERE tracker_id = $1 AND project_id IS NOT DISTINCT FROM $2"
    )))
    .bind(input.tracker_id)
    .bind(project)
    .fetch_one(&mut *tx)
    .await?;
    let d = detail(&mut tx, &f).await?;
    tx.commit().await?;
    Ok(d)
}

/// `GET /forms/{id}`.
pub async fn get(app: &App, caller: AuthContext, id: Uuid) -> Result<FormDetail> {
    let mut tx = read_tx(app).await?;
    let (f, _) = managed_form(&mut tx, caller, id, false).await?;
    let d = detail(&mut tx, &f).await?;
    tx.commit().await?;
    Ok(d)
}

/// `GET /forms/{id}/versions?part=`: newest first.
pub async fn versions(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    part: Part,
) -> Result<Vec<FormVersionView>> {
    let mut tx = read_tx(app).await?;
    managed_form(&mut tx, caller, id, false).await?;
    let rows: Vec<FormVersionView> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VERSION_COLS} FROM {} WHERE form_id = $1 ORDER BY version DESC",
        part.table()
    )))
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// The site rules of the tracker (for a project form's checks) and the
/// catalog of the form's layer.
async fn layer_catalog(conn: &mut PgConnection, f: &FormRow) -> Result<BTreeMap<i32, CfInfo>> {
    catalog(&mut *conn, f.tracker_id, f.project_id).await
}

/// Published rules of the layers a layout check must respect: the site's,
/// and for a project form also the project's.
async fn rules_in_force(
    conn: &mut PgConnection,
    f: &FormRow,
    cat: &BTreeMap<i32, CfInfo>,
    replace_own: Option<&[Rule]>,
) -> Result<Vec<Rule>> {
    let (site, proj) = form_rows(
        &mut *conn,
        f.project_id.unwrap_or(Uuid::nil()),
        f.tracker_id,
    )
    .await?;
    let mut out = Vec::new();
    for layer in [site, proj].into_iter().flatten() {
        if layer.id == f.id
            && let Some(r) = replace_own
        {
            out.extend(r.iter().cloned());
            continue;
        }
        if let Some(b) = published_body(
            &mut *conn,
            "form_rule_versions",
            layer.id,
            layer.published_rules_version,
        )
        .await?
        {
            out.extend(load_rules(&b, cat));
        }
    }
    Ok(out)
}

/// Published layouts that must still hold when rules change: this form's
/// and, for a site form, the project forms of its tracker.
async fn layouts_to_recheck(conn: &mut PgConnection, f: &FormRow) -> Result<Vec<(FormRow, Json)>> {
    let rows: Vec<FormRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {FORM_COLS} FROM form_defs WHERE tracker_id = $1
           AND (id = $2 OR ($3::uuid IS NULL AND project_id IS NOT NULL))
           AND published_layout_version > 0"
    )))
    .bind(f.tracker_id)
    .bind(f.id)
    .bind(f.project_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(b) = published_body(
            &mut *conn,
            "form_layout_versions",
            r.id,
            r.published_layout_version,
        )
        .await?
        {
            out.push((r, b));
        }
    }
    Ok(out)
}

/// The catalog an author writes rules against: administrators see every
/// field; anyone else (a project manager) only the fields they can read, so
/// a field they cannot read is `unknown_field`, in a target or a condition
/// alike (no existence oracle; `restricted_condition_field` is only said
/// to those who can read the field).
async fn author_catalog(
    conn: &mut PgConnection,
    caller: AuthContext,
    f: &FormRow,
    cat: &BTreeMap<i32, CfInfo>,
) -> Result<BTreeMap<i32, CfInfo>> {
    if db::is_admin(&mut *conn, caller).await? {
        return Ok(cat.clone());
    }
    let roles = match (
        caller.kind() == crate::auth::PrincipalKind::Agent,
        f.project_id,
    ) {
        (false, Some(p)) => db::role_ids_for(&mut *conn, caller, p).await?,
        _ => Vec::new(),
    };
    let granted: BTreeSet<i32> =
        sqlx::query_scalar("SELECT DISTINCT field_id FROM field_roles WHERE role_id = ANY($1)")
            .bind(&roles)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    Ok(cat
        .iter()
        .filter(|(id, c)| c.visible || granted.contains(id))
        .map(|(id, c)| (*id, c.clone()))
        .collect())
}

/// (status, field key) pairs a workflow of `tracker` makes required (for
/// any role).
async fn workflow_required(conn: &mut PgConnection, tracker: Uuid) -> Result<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT old_status_id, field_key FROM workflow_field_rules
          WHERE tracker_id = $1 AND rule = 'required' ORDER BY 1, 2",
    )
    .bind(tracker)
    .fetch_all(&mut *conn)
    .await?)
}

/// Checks a body for `part` of form `f` written or published by `caller`
/// (every error `form_invalid`).
async fn check_body(
    conn: &mut PgConnection,
    caller: AuthContext,
    f: &FormRow,
    part: Part,
    body: &Json,
) -> Result<Vec<Rule>> {
    let cat = layer_catalog(&mut *conn, f).await?;
    match part {
        Part::Rules => {
            let mine = author_catalog(&mut *conn, caller, f, &cat).await?;
            let rules = parse_rules(body, &mine)?;
            // A rule must not make a field required where a published
            // layout hides it.
            for (layout_form, lb) in layouts_to_recheck(&mut *conn, f).await? {
                let lcat = layer_catalog(&mut *conn, &layout_form).await?;
                let all = rules_in_force(&mut *conn, &layout_form, &lcat, Some(&rules)).await?;
                let all = if layout_form.id == f.id || f.project_id.is_some() {
                    all
                } else {
                    // A project layout under a changed site form: the
                    // site's new rules plus the project's.
                    let mut a = rules.clone();
                    a.extend(
                        all.into_iter()
                            .filter(|r| !rules.iter().any(|x| x.id == r.id)),
                    );
                    a
                };
                let wf = workflow_required(&mut *conn, layout_form.tracker_id).await?;
                parse_layout(&lb, &lcat, &required_map(&lcat, &all, &wf))?;
            }
            Ok(rules)
        }
        Part::Layout => {
            let all = rules_in_force(&mut *conn, f, &cat, None).await?;
            let wf = workflow_required(&mut *conn, f.tracker_id).await?;
            parse_layout(body, &cat, &required_map(&cat, &all, &wf))?;
            Ok(Vec::new())
        }
    }
}

/// `PUT /forms/{id}/drafts/{part}`: replaces the draft (the old one becomes
/// `discarded`, the new one gets the next version number).
pub async fn put_draft(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    part: Part,
    input: &DraftInput,
) -> Result<FormVersionView> {
    let mut tx = app.pool.begin().await?;
    let (f, user) = managed_form(&mut tx, caller, id, true).await?;
    let draft = version_row(&mut tx, part, id, "draft").await?;
    let base = match (&draft, part) {
        (Some(d), _) => d.version,
        (None, Part::Rules) => f.published_rules_version,
        (None, Part::Layout) => f.published_layout_version,
    };
    if input.base_version != base {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the form changed since you opened it; reload it",
        )
        .with_details(json!({"current_version": base}))
        .into());
    }
    check_body(&mut tx, caller, &f, part, &input.body).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET state = 'discarded' WHERE form_id = $1 AND state = 'draft'",
        part.table()
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let v: FormVersionView = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {t} (form_id, version, state, body, created_by)
         VALUES ($1, (SELECT COALESCE(max(version), 0) + 1 FROM {t} WHERE form_id = $1), 'draft', $2, $3)
         RETURNING {VERSION_COLS}",
        t = part.table()
    )))
    .bind(id)
    .bind(&input.body)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE form_defs SET lock_version = lock_version + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(v)
}

/// `POST /forms/{id}/drafts/{part}/discard`.
pub async fn discard(app: &App, caller: AuthContext, id: Uuid, part: Part) -> Result<FormDetail> {
    let mut tx = app.pool.begin().await?;
    let (_, _) = managed_form(&mut tx, caller, id, true).await?;
    let n = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET state = 'discarded' WHERE form_id = $1 AND state = 'draft'",
        part.table()
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(not_found("draft"));
    }
    sqlx::query("UPDATE form_defs SET lock_version = lock_version + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let f = load_form(&mut tx, id, false).await?;
    let d = detail(&mut tx, &f).await?;
    tx.commit().await?;
    Ok(d)
}

/// What a scan found.
struct Scanned {
    /// Issues (that the reader can see) breaking the rules, with rule ids.
    broken: Vec<(Uuid, Vec<String>)>,
    /// Issues of the scope the reader can see that were checked.
    scanned: i64,
    /// Every issue of the scope was checked (`limit` not reached).
    complete: bool,
    /// Some issue the reader cannot see breaks the rules.
    hidden_broken: bool,
}

/// The scan of an impact: every issue of the form's scope (up to `limit`)
/// that breaks `rules`, with the broken rule ids. With `scope`, only issues
/// it can see are counted and listed; the others only set `hidden_broken`.
async fn scan(
    conn: &mut PgConnection,
    f: &FormRow,
    rules: &[Rule],
    limit: Option<i64>,
    scope: Option<&ox_core::Scope>,
) -> Result<Scanned> {
    let mut out = Scanned {
        broken: Vec::new(),
        scanned: 0,
        complete: true,
        hidden_broken: false,
    };
    if rules.is_empty() {
        return Ok(out);
    }
    let cats = categories(&mut *conn).await?;
    let mut seen = 0i64;
    let mut after = Uuid::nil();
    loop {
        type Row = (Uuid, Uuid, bool, Uuid, Option<Uuid>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, project_id, is_private, author_id, assigned_to_id FROM issues
              WHERE tracker_id = $1 AND ($2::uuid IS NULL OR project_id = $2)
               AND archived_at IS NULL AND id > $3 ORDER BY id LIMIT 500",
        )
        .bind(f.tracker_id)
        .bind(f.project_id)
        .bind(after)
        .fetch_all(&mut *conn)
        .await?;
        if rows.is_empty() {
            return Ok(out);
        }
        for (id, project, private, author, assignee) in rows {
            if limit.is_some_and(|l| seen >= l) {
                out.complete = false;
                return Ok(out);
            }
            after = id;
            seen += 1;
            let visible = scope.is_none_or(|sc| {
                sc.can_see(&IssueFacts {
                    project_id: project,
                    tracker_id: f.tracker_id,
                    is_private: private,
                    author_id: author,
                    assigned_to_id: assignee,
                })
            });
            if visible {
                out.scanned += 1;
            }
            let (tracker, status, private, values) = issue_values(&mut *conn, id).await?;
            let s = State::of(
                tracker,
                status,
                cats.get(&status).map(String::as_str),
                private,
                values.iter().map(|(k, v)| (k.clone(), v)),
            );
            let broken = violations(rules, &s, &values);
            if !broken.is_empty() {
                if visible {
                    out.broken.push((id, broken));
                } else {
                    out.hidden_broken = true;
                }
            }
        }
    }
}

/// Pending proposals of the scope whose end state would break `rules`
/// (approximation: the document's field values and transition applied to
/// the current values; not a full simulation).
async fn pending_failing(
    conn: &mut PgConnection,
    caller: AuthContext,
    f: &FormRow,
    rules: &[Rule],
) -> Result<Vec<Uuid>> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    // Only proposals the caller can read: their own, every target visible.
    let Ok(owner) = commands::executor(&caller).map(|e| e.user_id) else {
        return Ok(Vec::new());
    };
    let cats = categories(&mut *conn).await?;
    let rows: Vec<(Uuid, Json)> = sqlx::query_as(
        "SELECT c.id, pc.body FROM changesets c
         JOIN changeset_versions v ON v.changeset_id = c.id AND v.version = c.current_version
         JOIN proposal_contents pc ON pc.id = v.content_id
         WHERE c.state IN ('proposed','approved') AND pc.body IS NOT NULL
           AND c.on_behalf_of_id = $1
         ORDER BY c.id",
    )
    .bind(owner)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (cs, body) in rows {
        if !crate::changesets::targets_readable(&mut *conn, caller, &body).await? {
            continue;
        }
        let mut failing = false;
        // issue id → (tracker, status, private, values)
        type Sim = (Uuid, Uuid, bool, BTreeMap<String, Json>);
        let mut states: BTreeMap<Uuid, Sim> = BTreeMap::new();
        for op in body["ops"].as_array().into_iter().flatten() {
            let target = op["target"]["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok());
            match op["kind"].as_str() {
                Some("create_issue") => {
                    let (Some(p), Some(t)) = (
                        op["project_id"]
                            .as_str()
                            .and_then(|s| Uuid::parse_str(s).ok()),
                        op["tracker_id"]
                            .as_str()
                            .and_then(|s| Uuid::parse_str(s).ok()),
                    ) else {
                        continue;
                    };
                    if t != f.tracker_id || f.project_id.is_some_and(|x| x != p) {
                        continue;
                    }
                    let status: Option<Uuid> =
                        sqlx::query_scalar("SELECT default_status_id FROM trackers WHERE id = $1")
                            .bind(t)
                            .fetch_one(&mut *conn)
                            .await?;
                    let mut values = BTreeMap::new();
                    if let Some(Json::Object(m)) = op.get("fields") {
                        for (k, v) in m {
                            values.insert(k.clone(), canonical(k, v));
                        }
                    }
                    let st = status.unwrap_or_default();
                    let s = State::of(
                        t,
                        st,
                        cats.get(&st).map(String::as_str),
                        op["is_private"].as_bool().unwrap_or(false),
                        values.iter().map(|(k, v)| (k.clone(), v)),
                    );
                    if !violations(rules, &s, &values).is_empty() {
                        failing = true;
                    }
                }
                Some("update_fields" | "transition") => {
                    let Some(id) = target else { continue };
                    let e = match states.entry(id) {
                        std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                        std::collections::btree_map::Entry::Vacant(v) => {
                            let row: Option<(Uuid, Uuid)> = sqlx::query_as(
                                "SELECT project_id, tracker_id FROM issues WHERE id = $1",
                            )
                            .bind(id)
                            .fetch_optional(&mut *conn)
                            .await?;
                            let Some((p, t)) = row else { continue };
                            if t != f.tracker_id || f.project_id.is_some_and(|x| x != p) {
                                continue;
                            }
                            v.insert(issue_values(&mut *conn, id).await?)
                        }
                    };
                    if op["kind"] == "transition" {
                        if let Some(to) = op["to_status_id"]
                            .as_str()
                            .and_then(|s| Uuid::parse_str(s).ok())
                        {
                            e.1 = to;
                        }
                    } else if let Some(Json::Object(m)) = op.get("set") {
                        for (k, v) in m {
                            e.3.insert(k.clone(), canonical(k, v));
                        }
                    }
                }
                _ => {}
            }
        }
        for (tr, st, pr, vals) in states.values() {
            let s = State::of(
                *tr,
                *st,
                cats.get(st).map(String::as_str),
                *pr,
                vals.iter().map(|(k, v)| (k.clone(), v)),
            );
            if !violations(rules, &s, vals).is_empty() {
                failing = true;
            }
        }
        if failing {
            out.push(cs);
        }
    }
    Ok(out)
}

/// A document value in canonical command form (as the engine stores it).
fn canonical(key: &str, v: &Json) -> Json {
    match Key::parse(key) {
        Some(Key::Cf(_)) => commands::parse_cf("", v).map(|x| x.1).unwrap_or(Json::Null),
        Some(k) => commands::parse_builtin("", k, v).unwrap_or(Json::Null),
        None => Json::Null,
    }
}

/// Agent field changes still undoable whose restoration would break `rules`.
async fn undo_failing(
    conn: &mut PgConnection,
    f: &FormRow,
    rules: &[Rule],
    scope: &ox_core::Scope,
) -> Result<i64> {
    if rules.is_empty() {
        return Ok(0);
    }
    let cats = categories(&mut *conn).await?;
    type Row = (
        Uuid,
        Uuid,
        String,
        Option<Json>,
        Uuid,
        bool,
        Uuid,
        Option<Uuid>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.execution_id, c.issue_id, c.field_key, c.before_value,
                i.project_id, i.is_private, i.author_id, i.assigned_to_id
         FROM execution_field_changes c JOIN issues i ON i.id = c.issue_id
         JOIN execution_refs r ON r.id = c.execution_id
         WHERE i.tracker_id = $1 AND ($2::uuid IS NULL OR i.project_id = $2)
           AND uuid_extract_timestamp(r.id) > now() - make_interval(days => $3)
           AND (r.kind = 'changeset'
                OR EXISTS (SELECT 1 FROM command_executions e
                           WHERE e.id = r.id AND e.origin = 'agent_direct'))
           AND c.field_key NOT IN ('status_id', '_triage')
         ORDER BY c.execution_id",
    )
    .bind(f.tracker_id)
    .bind(f.project_id)
    .bind(crate::undo::UNDO_DAYS as i32)
    .fetch_all(&mut *conn)
    .await?;
    let mut failing: BTreeSet<Uuid> = BTreeSet::new();
    for (exec, issue, field, before, project, private, author, assignee) in rows {
        // Only records of issues the caller can see.
        let facts = IssueFacts {
            project_id: project,
            tracker_id: f.tracker_id,
            is_private: private,
            author_id: author,
            assigned_to_id: assignee,
        };
        if failing.contains(&exec) || !scope.can_see(&facts) {
            continue;
        }
        let (tr, st, pr, mut vals) = issue_values(&mut *conn, issue).await?;
        let s0 = State::of(
            tr,
            st,
            cats.get(&st).map(String::as_str),
            pr,
            vals.iter().map(|(k, v)| (k.clone(), v)),
        );
        let now = violations(rules, &s0, &vals);
        vals.insert(field, before.unwrap_or(Json::Null));
        let s = State::of(
            tr,
            st,
            cats.get(&st).map(String::as_str),
            pr,
            vals.iter().map(|(k, v)| (k.clone(), v)),
        );
        if violations(rules, &s, &vals)
            .iter()
            .any(|r| !now.contains(r))
        {
            failing.insert(exec);
        }
    }
    Ok(failing.len() as i64)
}

async fn rules_impact(
    conn: &mut PgConnection,
    caller: AuthContext,
    f: &FormRow,
    rules: &[Rule],
    version: i32,
) -> Result<(FormImpact, bool)> {
    // Counts only what the caller can see (§3.6.3, D9). An issue they
    // cannot see that breaks the rules makes the scan incomplete (no
    // number, no key): `block` cannot prove there is no violation.
    let scope = db::load_scope(&mut *conn, caller).await?;
    let sc = scan(&mut *conn, f, rules, Some(SCAN_LIMIT), Some(&scope)).await?;
    let ids: Vec<Uuid> = sc.broken.iter().map(|b| b.0).collect();
    let sample: Vec<String> = sqlx::query_scalar(
        "SELECT k.key FROM issue_keys k WHERE k.issue_id = ANY($1) AND k.is_current
         ORDER BY k.issue_id LIMIT $2",
    )
    .bind(&ids)
    .bind(SAMPLE_KEYS as i64)
    .fetch_all(&mut *conn)
    .await?;
    let pending = pending_failing(&mut *conn, caller, f, rules).await?;
    let undo = undo_failing(&mut *conn, f, rules, &scope).await?;
    let mut c = counters(
        &mut *conn,
        f.project_id.unwrap_or(Uuid::nil()),
        f.tracker_id,
    )
    .await?;
    if f.project_id.is_some() {
        c.1 = version;
    } else {
        c.0 = version;
        c.1 = 0;
    }
    Ok((
        FormImpact {
            draft_version: version,
            rules_version_after: Some(version_strings(c).0),
            layout_version_after: None,
            existing_issues: Some(ExistingIssues {
                violating: ids.len() as i64,
                sample_keys: sample,
                scanned: sc.scanned,
                complete: sc.complete && !sc.hidden_broken,
            }),
            pending_changesets: Some(PendingChangesets {
                would_fail: pending.len() as i64,
                ids: pending,
            }),
            undo_records: Some(UndoRecords {
                not_applicable_after: undo,
            }),
            layout_warnings: Vec::new(),
        },
        sc.hidden_broken,
    ))
}

/// The impact kept with a published version: counts only, no issue keys
/// or proposal ids (whoever reads the version later may not see them).
fn stored_impact(imp: &FormImpact, marked: i64) -> Result<Json> {
    let mut j = serde_json::to_value(imp)?;
    if let Some(e) = j.get_mut("existing_issues") {
        e["sample_keys"] = json!([]);
    }
    if let Some(p) = j.get_mut("pending_changesets") {
        p["ids"] = json!([]);
    }
    j["marked"] = json!(marked);
    Ok(j)
}

fn layout_warnings(body: &Json, cat: &BTreeMap<i32, CfInfo>) -> Vec<Json> {
    let mut placed = BTreeSet::new();
    if let Some(ss) = layout_sections(body, "create") {
        for s in ss {
            for f in s.fields {
                placed.insert(f.key);
            }
        }
    }
    let mut w = Vec::new();
    for k in LAYOUT_BUILTINS {
        if k != "status_id" && !placed.contains(k) {
            w.push(json!({"kind": "unplaced", "field": k}));
        }
    }
    for (id, c) in cat {
        let k = format!("cf:{id}");
        if c.applies && !c.inactive && !placed.contains(&k) {
            w.push(json!({"kind": "unplaced", "field": k}));
        }
    }
    w
}

/// `POST /forms/{id}/drafts/{part}/impact`.
pub async fn impact(app: &App, caller: AuthContext, id: Uuid, part: Part) -> Result<FormImpact> {
    let mut tx = read_tx(app).await?;
    let (f, _) = managed_form(&mut tx, caller, id, false).await?;
    let draft = version_row(&mut tx, part, id, "draft")
        .await?
        .ok_or_else(|| not_found("draft"))?;
    let rules = check_body(&mut tx, caller, &f, part, &draft.body).await?;
    let out = match part {
        Part::Rules => {
            rules_impact(&mut tx, caller, &f, &rules, draft.version)
                .await?
                .0
        }
        Part::Layout => {
            let cat = layer_catalog(&mut tx, &f).await?;
            let mut c =
                counters(&mut tx, f.project_id.unwrap_or(Uuid::nil()), f.tracker_id).await?;
            if f.project_id.is_some() {
                c.3 = draft.version;
            } else {
                c.2 = draft.version;
                c.3 = 0;
            }
            FormImpact {
                draft_version: draft.version,
                rules_version_after: None,
                layout_version_after: Some(version_strings(c).1),
                existing_issues: None,
                pending_changesets: None,
                undo_records: None,
                layout_warnings: layout_warnings(&draft.body, &cat),
            }
        }
    };
    tx.commit().await?;
    Ok(out)
}

/// Publishes `body` as version `version` of `part` (the draft row when
/// `from_draft`, else a new row), after the impact and the mode.
#[allow(clippy::too_many_arguments)]
async fn publish_body(
    conn: &mut PgConnection,
    caller: AuthContext,
    f: &FormRow,
    user: Uuid,
    part: Part,
    body: &Json,
    from_draft: Option<i32>,
    mode: Option<&str>,
) -> Result<PublishResult> {
    let rules = check_body(&mut *conn, caller, f, part, body).await?;
    let version = match from_draft {
        Some(v) => v,
        None => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(max(version), 0) + 1 FROM {} WHERE form_id = $1",
                part.table()
            )))
            .bind(f.id)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    let mut marked = 0i64;
    let mut impact_json = None;
    let mode = match part {
        Part::Layout => {
            if mode.is_some() {
                return Err(
                    ApiError::new(ErrorCode::InvalidValue, "a layout has no publish mode")
                        .with_details(json!({"path": "mode"}))
                        .into(),
                );
            }
            None
        }
        Part::Rules => {
            let m = mode.unwrap_or("");
            if !PUBLISH_MODES.contains(&m) {
                return Err(ApiError::new(
                    ErrorCode::InvalidValue,
                    "mode is block or accept_and_mark",
                )
                .with_details(json!({"path": "mode", "allowed": PUBLISH_MODES}))
                .into());
            }
            let (imp, hidden) = rules_impact(&mut *conn, caller, f, &rules, version).await?;
            let ex = imp.existing_issues.as_ref();
            let complete = ex.is_some_and(|e| e.complete);
            let visible = ex.map_or(0, |e| e.violating);
            if m == "block" {
                if hidden {
                    return Err(ApiError::new(
                        ErrorCode::FormPublishBlocked,
                        "cannot prove that no issue breaks the new rules; publish with accept_and_mark",
                    )
                    .with_details(serde_json::to_value(&imp)?)
                    .into());
                }
                if !complete {
                    return Err(ApiError::new(
                        ErrorCode::FormPublishBlocked,
                        "too many issues to prove there is no violation; publish with accept_and_mark",
                    )
                    .with_details(serde_json::to_value(&imp)?)
                    .into());
                }
                if visible > 0 {
                    return Err(ApiError::new(
                        ErrorCode::FormPublishBlocked,
                        "existing issues break the new rules",
                    )
                    .with_details(serde_json::to_value(&imp)?)
                    .into());
                }
            } else {
                // Every issue of the scope, not only the first 20,000.
                let all = scan(&mut *conn, f, &rules, None, None).await?;
                for (issue, ids) in &all.broken {
                    sqlx::query(
                        "INSERT INTO form_rule_marks (issue_id, form_id, rules_version, rule_ids)
                         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                    )
                    .bind(issue)
                    .bind(f.id)
                    .bind(version)
                    .bind(ids)
                    .execute(&mut *conn)
                    .await?;
                }
                marked = all.broken.len() as i64;
            }
            impact_json = Some(stored_impact(&imp, marked)?);
            Some(m.to_string())
        }
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET state = 'superseded' WHERE form_id = $1 AND state = 'published'",
        part.table()
    )))
    .bind(f.id)
    .execute(&mut *conn)
    .await?;
    match from_draft {
        Some(v) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {} SET state = 'published', publish_mode = $3, impact = $4,
                        published_by = $5, published_at = now()
                 WHERE form_id = $1 AND version = $2",
                part.table()
            )))
            .bind(f.id)
            .bind(v)
            .bind(&mode)
            .bind(&impact_json)
            .bind(user)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {} (form_id, version, state, body, publish_mode, impact, created_by,
                                 published_by, published_at)
                 VALUES ($1, $2, 'published', $3, $4, $5, $6, $6, now())",
                part.table()
            )))
            .bind(f.id)
            .bind(version)
            .bind(body)
            .bind(&mode)
            .bind(&impact_json)
            .bind(user)
            .execute(&mut *conn)
            .await?;
        }
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE form_defs SET {} = $2, lock_version = lock_version + 1 WHERE id = $1",
        part.published_col()
    )))
    .bind(f.id)
    .bind(version)
    .execute(&mut *conn)
    .await?;
    let f2 = load_form(&mut *conn, f.id, false).await?;
    let (rv, lv) = layer_versions(&mut *conn, &f2).await?;
    Ok(PublishResult {
        form_id: f.id,
        part: match part {
            Part::Rules => "rules",
            Part::Layout => "layout",
        }
        .into(),
        version,
        rules_version: rv,
        layout_version: lv,
        marked,
    })
}

/// `POST /forms/{id}/drafts/{part}/publish` (step-up).
pub async fn publish(
    app: &App,
    policy: &SessionPolicy,
    caller: AuthContext,
    id: Uuid,
    part: Part,
    input: &PublishInput,
) -> Result<PublishResult> {
    let mut tx = app.pool.begin().await?;
    let (f, user) = managed_form(&mut tx, caller, id, true).await?;
    auth::require_step_up(&mut tx, policy, &caller).await?;
    let changed = |current: Option<i32>| -> anyhow::Error {
        ApiError::new(ErrorCode::Conflict, "the draft changed; review it again")
            .with_details(json!({"current_version": current}))
            .into()
    };
    let Some(draft) = version_row(&mut tx, part, id, "draft").await? else {
        // A draft discarded since (a rollback, a newer draft): conflict.
        let existed: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE form_id = $1 AND version = $2)",
            part.table()
        )))
        .bind(id)
        .bind(input.draft_version)
        .fetch_one(&mut *tx)
        .await?;
        return Err(if existed {
            changed(None)
        } else {
            not_found("draft")
        });
    };
    if draft.version != input.draft_version {
        return Err(changed(Some(draft.version)));
    }
    // §3.6.3: versions only go up. A draft is based on the version
    // published when it was written; one older than what is published now
    // (published since by a rollback) must be rewritten.
    let published = match part {
        Part::Rules => f.published_rules_version,
        Part::Layout => f.published_layout_version,
    };
    if draft.version <= published {
        return Err(changed(Some(draft.version)));
    }
    let r = publish_body(
        &mut tx,
        caller,
        &f,
        user,
        part,
        &draft.body,
        Some(draft.version),
        input.mode.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(r)
}

/// `POST /forms/{id}/rollback` (step-up): an old body published again under
/// a new version number.
pub async fn rollback(
    app: &App,
    policy: &SessionPolicy,
    caller: AuthContext,
    id: Uuid,
    input: &RollbackInput,
) -> Result<PublishResult> {
    let part = Part::parse(&input.part).map_err(|_| {
        ApiError::new(ErrorCode::InvalidValue, "part is rules or layout")
            .with_details(json!({"path": "part"}))
    })?;
    let mut tx = app.pool.begin().await?;
    let (f, user) = managed_form(&mut tx, caller, id, true).await?;
    auth::require_step_up(&mut tx, policy, &caller).await?;
    let body: Option<Json> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT body FROM {} WHERE form_id = $1 AND version = $2 AND state IN ('published','superseded')",
        part.table()
    )))
    .bind(id)
    .bind(input.to_version)
    .fetch_optional(&mut *tx)
    .await?;
    let body = body.ok_or_else(|| not_found("published version"))?;
    // An open draft was based on what is replaced now: discard it.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET state = 'discarded' WHERE form_id = $1 AND state = 'draft'",
        part.table()
    )))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let r = publish_body(
        &mut tx,
        caller,
        &f,
        user,
        part,
        &body,
        None,
        input.mode.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(r)
}

/// Published rules whose conditions name custom field `field`: (form id,
/// rule ids). For the field PATCH warning when the field becomes
/// restricted (people who cannot read it stop being held to those rules).
pub(crate) async fn rules_conditioned_on(
    conn: &mut PgConnection,
    field: i32,
) -> Result<Vec<(Uuid, Vec<String>)>> {
    let rows: Vec<(Uuid, Json)> = sqlx::query_as(
        "SELECT form_id, body FROM form_rule_versions WHERE state = 'published' ORDER BY form_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (form, body) in rows {
        let ids: Vec<String> = body
            .get("rules")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter(|r| {
                let mut cfs = BTreeSet::new();
                rule_cf_ids(&r["when"], &mut cfs);
                cfs.contains(&field)
            })
            .filter_map(|r| r["id"].as_str().map(String::from))
            .collect();
        if !ids.is_empty() {
            out.push((form, ids));
        }
    }
    Ok(out)
}

/// Custom fields named anywhere in a rule (target and conditions).
fn rule_cf_ids(rule: &Json, out: &mut BTreeSet<i32>) {
    match rule {
        Json::Object(m) => {
            for (k, v) in m {
                if k == "field"
                    && let Some(Key::Cf(id)) = v.as_str().and_then(Key::parse)
                {
                    out.insert(id);
                }
                rule_cf_ids(v, out);
            }
        }
        Json::Array(xs) => xs.iter().for_each(|x| rule_cf_ids(x, out)),
        _ => {}
    }
}

/// Open marks of an issue in `project` (`GET /issues/{key}`): rule ids
/// only, and only of rules whose custom fields the caller can read (as
/// `describe_form` lists them).
pub(crate) async fn open_marks(
    conn: &mut PgConnection,
    caller: AuthContext,
    issue: Uuid,
    project: Uuid,
) -> Result<Vec<String>> {
    let rows: Vec<(Vec<String>, Option<Json>)> = sqlx::query_as(
        "SELECT m.rule_ids, v.body FROM form_rule_marks m
         LEFT JOIN form_rule_versions v ON v.form_id = m.form_id AND v.version = m.rules_version
         WHERE m.issue_id = $1 AND m.cleared_at IS NULL
         ORDER BY m.marked_at",
    )
    .bind(issue)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let (admin, roles) = if caller.kind() == crate::auth::PrincipalKind::Agent {
        (false, Vec::new())
    } else {
        (
            crate::db::is_admin(&mut *conn, caller).await?,
            crate::db::role_ids_for(&mut *conn, caller, project).await?,
        )
    };
    let readable: BTreeSet<i32> = sqlx::query_scalar::<_, i32>(
        "SELECT d.id FROM field_defs d
         WHERE d.visible OR $1 OR EXISTS (SELECT 1 FROM field_roles fr
                                          WHERE fr.field_id = d.id AND fr.role_id = ANY($2))",
    )
    .bind(admin)
    .bind(&roles)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let mut out: Vec<String> = Vec::new();
    for (ids, body) in rows {
        for id in ids {
            let rule = body
                .as_ref()
                .and_then(|b| b.get("rules"))
                .and_then(Json::as_array)
                .and_then(|rs| rs.iter().find(|r| r["id"] == json!(id)));
            let mut cfs = BTreeSet::new();
            if let Some(r) = rule {
                rule_cf_ids(r, &mut cfs);
            }
            if rule.is_some() && cfs.is_subset(&readable) && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// A write that passed the checks clears the marks of the rules it was held
/// to and meets (`passed`, §3.6.3): those ids leave the open marks; a mark
/// with none left is cleared (keeping its ids as history).
pub(crate) async fn clear_marks(
    conn: &mut PgConnection,
    issue: Uuid,
    passed: &[String],
) -> Result<()> {
    sqlx::query(
        "UPDATE form_rule_marks m
            SET rule_ids = CASE WHEN cardinality(n.ids) = 0 THEN m.rule_ids ELSE n.ids END,
                cleared_at = CASE WHEN cardinality(n.ids) = 0 THEN now() END
           FROM (SELECT issue_id, form_id, rules_version,
                        ARRAY(SELECT x FROM unnest(rule_ids) x WHERE x <> ALL($2)) AS ids
                   FROM form_rule_marks WHERE issue_id = $1 AND cleared_at IS NULL) n
          WHERE m.issue_id = n.issue_id AND m.form_id = n.form_id
            AND m.rules_version = n.rules_version
            AND cardinality(n.ids) < cardinality(m.rule_ids)",
    )
    .bind(issue)
    .bind(passed)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- validate (§3.6.4)

/// `POST /commands/validate` and MCP `validate`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateCommandInput {
    /// A command document, as for command_submit.
    pub command: Json,
    /// The rules_version from describe_form; overrides one inside `command`.
    #[serde(default)]
    pub expected_rules_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidateError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidateItem {
    pub op_id: Uuid,
    /// `ok`, `no_effect`, `blocked`, or `not_checked` (an earlier op failed).
    pub status: String,
    pub errors: Vec<ValidateError>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidateResult {
    /// `valid` or `invalid`.
    pub outcome: String,
    /// The rules version of the document's targets (null: no issue targets).
    pub rules_version: Option<String>,
    pub items: Vec<ValidateItem>,
    /// Agents: whether command_submit would send it for review (policy).
    /// Always false for people.
    pub would_require_review: bool,
}

/// The command with `expected_rules_version` from the input in it.
pub(crate) fn command_of(input: &ValidateCommandInput) -> Json {
    let mut c = input.command.clone();
    if let (Some(v), Json::Object(m)) = (&input.expected_rules_version, &mut c) {
        m.insert("expected_rules_version".into(), json!(v));
    }
    c
}

/// The result of a validation run: rejections (§6.7 steps 1–8: permission,
/// visibility, malformed input, stale rules) are returned as errors, like a
/// real submit; anything else becomes item errors.
pub(crate) async fn outcome(
    conn: &mut PgConnection,
    parsed: &commands::Parsed,
    sim: Result<commands::Sim>,
    would_require_review: bool,
) -> Result<ValidateResult> {
    let pairs = doc_pairs(&mut *conn, &parsed.norm).await?;
    let rules_version = pairs_version(&mut *conn, &pairs).await?;
    let ops = parsed.op_list();
    let items: Vec<ValidateItem> = match sim {
        Ok(s) => s
            .items
            .iter()
            .map(|i| ValidateItem {
                op_id: i.op_id,
                status: if i.status == "applied" {
                    "ok"
                } else {
                    "no_effect"
                }
                .into(),
                errors: Vec::new(),
            })
            .collect(),
        Err(e) => {
            if commands::is_rejection(&e) {
                return Err(e);
            }
            let Some(a) = crate::error::classify(&e) else {
                return Err(e);
            };
            let d = a.details.clone().unwrap_or(Json::Null);
            let err = ValidateError {
                code: a.code,
                message: a.message.clone(),
                fields: d
                    .get("fields")
                    .cloned()
                    .or_else(|| d.get("field").map(|f| json!([f]))),
                rule_id: d.get("rule_id").cloned(),
                details: a.details.clone(),
            };
            let failing: Option<Uuid> = d["op_id"].as_str().and_then(|s| s.parse().ok());
            let mut seen = false;
            ops.iter()
                .map(|(op_id, _)| {
                    let (status, errors) = match failing {
                        // Without an op (a final-state check): every op.
                        None => ("blocked", vec![err.clone()]),
                        Some(f) if f == *op_id => {
                            seen = true;
                            ("blocked", vec![err.clone()])
                        }
                        Some(_) if seen => ("not_checked", Vec::new()),
                        Some(_) => ("ok", Vec::new()),
                    };
                    ValidateItem {
                        op_id: *op_id,
                        status: status.into(),
                        errors,
                    }
                })
                .collect()
        }
    };
    let valid = items.iter().all(|i| i.errors.is_empty());
    Ok(ValidateResult {
        outcome: if valid { "valid" } else { "invalid" }.into(),
        rules_version,
        items,
        would_require_review,
    })
}

/// `POST /commands/validate` for a person: the same evaluation as
/// `POST /commands`, in a read-only transaction; nothing is written.
pub async fn validate_human(
    app: &App,
    caller: AuthContext,
    input: &ValidateCommandInput,
) -> Result<ValidateResult> {
    commands::executor(&caller)?;
    let mut tx = read_tx(app).await?;
    let raw = command_of(input);
    let parsed = commands::parse(&mut tx, &raw, describe::MAX_OPS as usize).await?;
    let ectx = commands::EvalCtx {
        mode: commands::Mode::Validate,
        caller,
        ceiling: None,
        deadlines: Vec::new(),
        author: None,
    };
    let sim = commands::simulate(&mut tx, &ectx, &parsed).await;
    let r = outcome(&mut tx, &parsed, sim, false).await;
    tx.rollback().await?;
    r
}
