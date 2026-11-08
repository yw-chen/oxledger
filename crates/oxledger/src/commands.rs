// SPDX-License-Identifier: GPL-2.0-or-later
//! The command engine (contract 02 §6) and `human_direct` (§6.8).
//!
//! One command document (§6.1) is checked and applied in one READ COMMITTED
//! transaction with explicit row locks, in the order of §6.7 (apply steps
//! 5–19) and §17.6 (step 7b):
//!
//! ```text
//! operation_requests → principal, session FOR SHARE → projects (UUID order;
//! FOR UPDATE when the document creates an issue there) → issues FOR UPDATE
//! (id order) → newly chosen versions FOR SHARE → field_defs FOR SHARE,
//! list options FOR KEY SHARE → search_settings FOR SHARE → deadline
//! (clock_timestamp) → visibility → permissions → preconditions →
//! simulation → final checks → writes → events → lock_version
//! ```
//!
//! A domain failure rolls the whole transaction back, the operation request
//! included, so resending the same key after fixing the input evaluates again
//! (§6.8). Only a success is stored; resending its key replays the stored
//! ids, re-checking visibility, and never runs the document twice.
//!
//! The engine is shared: `evaluate` takes a [`Mode`]; S4 runs `Direct` for
//! people. ChangeSets (S5) and direct agent writes (S6) reuse it.

use crate::auth::{AuthContext, AuthMethod};
use crate::db;
use crate::describe::{
    self, CfDef, IssueCore, MAX_DESCRIPTION_BYTES, MAX_NOTE_BYTES, MAX_OPS, applicable_fields,
    readable, rules_by_status,
};
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, Actor, ProjectPerms};
use crate::service::App;
use anyhow::Result;
use base64::Engine as _;
use chrono::NaiveDate;
use ox_core::IssueFacts;
use ox_core::workflow::{FieldRule, HiddenField, Rule, field_rules};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Postgres};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// The principal that writes seed and import events (migration 0008).
pub const SYSTEM_PRINCIPAL: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0001);
/// `operation_requests.action` of `POST /commands`.
pub const ACTION_COMMAND: &str = "command";
/// How long a stored result replays (§8.7: at least 90 days).
pub(crate) const RESULT_DAYS: i32 = 90;
const MAX_TITLE_CHARS: usize = 255;
const MAX_EVIDENCE: usize = 50;
const MAX_CF_SHORT_BYTES: usize = 2000;
const DEADLOCK_RETRIES: usize = 3;
const MIN_DATE: (i32, u32, u32) = (1900, 1, 1);
const MAX_DATE: (i32, u32, u32) = (2199, 12, 31);

/// Which `evaluate` this is (§6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A person's `POST /commands` (§6.8).
    Direct,
    /// An agent proposes or revises (§6.7 propose step 9): read only.
    Propose,
    /// `POST /changesets/{id}/validations` (§8.5): read only.
    Validate,
    /// The domain transaction of approve-and-apply (§8.6 B).
    Apply,
}

impl Mode {
    /// Whether this mode takes the row locks and may write.
    pub fn locks(self) -> bool {
        matches!(self, Mode::Direct | Mode::Apply)
    }
}

// ---------------------------------------------------------------- input (§6.1)

/// A command document, `command_schema_version = 1`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandDoc {
    /// Always 1.
    pub command_schema_version: u16,
    /// Only `"atomic"` (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 1–10 operations, applied in order, all or nothing.
    pub ops: Vec<OpInput>,
    /// Exactly one per existing target issue, each with `lock_version`.
    #[serde(default)]
    pub preconditions: Vec<Precondition>,
    /// Citations (`{citation_id, quote}`); checked for agent proposals (S6).
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpInput {
    CreateIssue(CreateIssueOp),
    UpdateFields(UpdateFieldsOp),
    Transition(TransitionOp),
    AddNote(AddNoteOp),
    DuplicateOf(DuplicateOfOp),
}

const KINDS: &[&str] = &[
    "create_issue",
    "update_fields",
    "transition",
    "add_note",
    "duplicate_of",
];

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueOp {
    pub op_id: Uuid,
    /// A name later ops use as `{"type": "temp", "ref": …}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    /// `subject` (required), `description`, `is_private`, `assigned_to_id`,
    /// `priority_id`, `fixed_version_id`, `start_date`, `due_date`, `cf:<id>`.
    pub fields: BTreeMap<String, Json>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateFieldsOp {
    pub op_id: Uuid,
    pub target: TargetRef,
    /// Field key → new value; `null` clears.
    pub set: BTreeMap<String, Json>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransitionOp {
    pub op_id: Uuid,
    pub target: TargetRef,
    pub to_status_id: Uuid,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddNoteOp {
    pub op_id: Uuid,
    pub target: TargetRef,
    pub body: String,
    #[serde(default)]
    pub is_private: bool,
}

/// `source` is the duplicate, `target` the original. Must be the only op.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateOfOp {
    pub op_id: Uuid,
    pub source: TargetRef,
    pub target: TargetRef,
}

/// `{"type":"issue","id"|"key"}` or `{"type":"temp","ref"}`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetRef {
    Issue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    Temp {
        #[serde(rename = "ref")]
        reference: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Precondition {
    pub target: TargetRef,
    pub lock_version: i32,
    /// Extra conditions on readable fields (normalized values).
    #[serde(default)]
    pub field_values: BTreeMap<String, Json>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub citation_id: String,
    pub quote: String,
}

// ---------------------------------------------------------------- output (§6.9)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommandResult {
    /// Always `succeeded` (failures are errors).
    pub status: String,
    pub execution_id: Uuid,
    /// One per issue the document touched.
    pub effects: Vec<Effect>,
    /// One per op, in order.
    pub items: Vec<ItemResult>,
    pub search: SearchReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Effect {
    pub issue_id: Uuid,
    pub key: String,
    pub lock_version: i32,
    /// Anything changed (fields, status, note, relation).
    pub changed: bool,
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ItemResult {
    pub op_id: Uuid,
    /// `applied` or `no_effect`.
    pub status: String,
    pub issue_id: Uuid,
}

/// Design 03 §4.3: wait for this before expecting search to show the write.
/// Not a proof of commit; `status` is.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SearchReceipt {
    pub consistent_after: String,
}

// ---------------------------------------------------------------- errors

fn bad(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidRequest, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn invalid_value(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn op_err(
    code: ErrorCode,
    op: usize,
    op_id: Uuid,
    msg: impl Into<String>,
    extra: Json,
) -> anyhow::Error {
    let mut d = json!({"op_index": op, "op_id": op_id});
    if let (Json::Object(m), Json::Object(x)) = (&mut d, extra) {
        m.extend(x);
    }
    ApiError::new(code, msg).with_details(d).into()
}

fn issue_not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no issue {what}"))
}

// ---------------------------------------------------------------- field values

/// A field a command may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Subject,
    Description,
    AssignedTo,
    Priority,
    Version,
    StartDate,
    DueDate,
    Cf(i32),
}

impl Key {
    fn parse(k: &str) -> Option<Key> {
        Some(match k {
            "subject" => Key::Subject,
            "description" => Key::Description,
            "assigned_to_id" => Key::AssignedTo,
            "priority_id" => Key::Priority,
            "fixed_version_id" => Key::Version,
            "start_date" => Key::StartDate,
            "due_date" => Key::DueDate,
            _ => {
                let n = k.strip_prefix("cf:")?;
                if n.is_empty() || n.len() > 9 || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Key::Cf(n.parse().ok().filter(|v| *v > 0)?)
            }
        })
    }

    fn name(self) -> String {
        match self {
            Key::Subject => "subject".into(),
            Key::Description => "description".into(),
            Key::AssignedTo => "assigned_to_id".into(),
            Key::Priority => "priority_id".into(),
            Key::Version => "fixed_version_id".into(),
            Key::StartDate => "start_date".into(),
            Key::DueDate => "due_date".into(),
            Key::Cf(id) => format!("cf:{id}"),
        }
    }
}

const BUILTIN_KEYS: [Key; 7] = [
    Key::Subject,
    Key::Description,
    Key::AssignedTo,
    Key::Priority,
    Key::Version,
    Key::StartDate,
    Key::DueDate,
];

fn date_in_range(d: NaiveDate) -> bool {
    let lo = NaiveDate::from_ymd_opt(MIN_DATE.0, MIN_DATE.1, MIN_DATE.2).unwrap_or(d);
    let hi = NaiveDate::from_ymd_opt(MAX_DATE.0, MAX_DATE.1, MAX_DATE.2).unwrap_or(d);
    (lo..=hi).contains(&d)
}

/// `YYYY-MM-DD` only.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Decimal string → canonical form (no leading zeros, no trailing fraction
/// zeros, no "-0"), or `None` if malformed. `int` allows no fraction.
pub fn canonical_number(s: &str, int: bool) -> Option<String> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (ip, fp) = match digits.split_once('.') {
        Some((i, f)) if !int && !f.is_empty() => (i, f),
        Some(_) => return None,
        None => (digits, ""),
    };
    if ip.is_empty()
        || ip.len() > 20
        || fp.len() > 10
        || !ip.bytes().all(|b| b.is_ascii_digit())
        || !fp.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let ip = ip.trim_start_matches('0');
    let fp = fp.trim_end_matches('0');
    let ip = if ip.is_empty() { "0" } else { ip };
    let mut out = String::new();
    if neg && !(ip == "0" && fp.is_empty()) {
        out.push('-');
    }
    out.push_str(ip);
    if !fp.is_empty() {
        out.push('.');
        out.push_str(fp);
    }
    Some(out)
}

const CF_TYPES: &[&str] = &[
    "int", "float", "date", "string", "text", "list", "user", "bool", "link",
];

/// A custom field value as written: `{"<type>": v}` or `null`. Returns the
/// type and the canonical value (`Json::Null` = no value). Syntax only; the
/// field's own type is checked later.
fn parse_cf(path: &str, v: &Json) -> Result<(Option<&'static str>, Json)> {
    let Json::Object(m) = v else {
        if v.is_null() {
            return Ok((None, Json::Null));
        }
        return Err(bad(
            path,
            "a custom field value is {\"<type>\": value} or null",
        ));
    };
    let [(t, inner)] = m.iter().collect::<Vec<_>>()[..] else {
        return Err(bad(path, "a custom field value has exactly one type key"));
    };
    let Some(ty) = CF_TYPES.iter().find(|x| **x == t.as_str()).copied() else {
        return Err(bad(path, format!("unknown custom field type {t:?}")));
    };
    let s = || {
        inner
            .as_str()
            .ok_or_else(|| bad(path, format!("{ty} values are strings")))
    };
    let canon = match ty {
        "bool" => match inner {
            Json::Bool(b) => json!({"bool": b}),
            _ => return Err(bad(path, "bool values are true or false")),
        },
        _ if s()?.is_empty() => Json::Null, // D14: "" clears
        "int" | "float" => {
            let n = canonical_number(s()?, ty == "int")
                .ok_or_else(|| bad(path, format!("not a decimal {ty} string")))?;
            json!({ ty: n })
        }
        "date" => {
            let d = parse_date(s()?).ok_or_else(|| bad(path, "dates are YYYY-MM-DD"))?;
            json!({"date": d.to_string()})
        }
        "list" | "user" => {
            let u =
                Uuid::parse_str(s()?).map_err(|_| bad(path, format!("{ty} values are uuids")))?;
            json!({ ty: u })
        }
        _ => json!({ ty: s()? }),
    };
    Ok((Some(ty), canon))
}

/// A built-in field value → canonical JSON (`Json::Null` = empty).
fn parse_builtin(path: &str, key: Key, v: &Json) -> Result<Json> {
    let uuid_or_null = |what: &str| -> Result<Json> {
        match v {
            Json::Null => Ok(Json::Null),
            Json::String(s) => Uuid::parse_str(s)
                .map(|u| json!(u))
                .map_err(|_| bad(path, format!("{what} is a uuid or null"))),
            _ => Err(bad(path, format!("{what} is a uuid or null"))),
        }
    };
    match key {
        Key::Subject => match v {
            Json::String(s) => {
                let n = s.chars().count();
                if n == 0 || n > 255 || s.trim().is_empty() {
                    return Err(invalid_value(path, "subject is 1–255 characters"));
                }
                Ok(json!(s))
            }
            Json::Null => Err(invalid_value(path, "subject cannot be cleared")),
            _ => Err(bad(path, "subject is a string")),
        },
        Key::Description => match v {
            Json::Null => Ok(json!("")),
            Json::String(s) if s.len() <= MAX_DESCRIPTION_BYTES as usize => Ok(json!(s)),
            Json::String(_) => Err(invalid_value(path, "description is at most 262144 bytes")),
            _ => Err(bad(path, "description is a string")),
        },
        Key::AssignedTo => uuid_or_null("assigned_to_id"),
        Key::Version => uuid_or_null("fixed_version_id"),
        Key::Priority => match uuid_or_null("priority_id")? {
            Json::Null => Err(invalid_value(path, "priority_id cannot be cleared")),
            u => Ok(u),
        },
        Key::StartDate | Key::DueDate => match v {
            Json::Null => Ok(Json::Null),
            Json::String(s) => {
                let d = parse_date(s).ok_or_else(|| bad(path, "dates are YYYY-MM-DD"))?;
                if !date_in_range(d) {
                    return Err(ApiError::new(
                        ErrorCode::InvalidValue,
                        "dates are between 1900-01-01 and 2199-12-31",
                    )
                    .with_details(json!({"path": path, "kind": "date_out_of_range"}))
                    .into());
                }
                Ok(json!(d.to_string()))
            }
            _ => Err(bad(path, "dates are YYYY-MM-DD strings or null")),
        },
        Key::Cf(_) => unreachable!("custom fields use parse_cf"),
    }
}

fn is_empty(v: &Json) -> bool {
    v.is_null() || v.as_str() == Some("")
}

fn json_uuid(v: &Json) -> Option<Uuid> {
    v.as_str().and_then(|s| Uuid::parse_str(s).ok())
}

fn json_date(v: &Json) -> Option<NaiveDate> {
    v.as_str().and_then(parse_date)
}

// ---------------------------------------------------------------- parsed document

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Issue(Uuid),
    Temp(String),
}

#[derive(Debug, Clone)]
struct Write {
    key: Key,
    /// Canonical value; for cfs the written type is kept for the type check.
    value: Json,
    cf_type: Option<&'static str>,
    path: String,
}

#[derive(Debug, Clone)]
enum Op {
    Create {
        op_id: Uuid,
        bind: Option<String>,
        project_id: Uuid,
        tracker_id: Uuid,
        fields: Vec<Write>,
        is_private: Option<bool>,
    },
    Update {
        op_id: Uuid,
        target: Target,
        set: Vec<Write>,
    },
    Transition {
        op_id: Uuid,
        target: Target,
        to: Uuid,
    },
    Note {
        op_id: Uuid,
        target: Target,
        body: String,
        private: bool,
    },
    Duplicate {
        op_id: Uuid,
        source: Target,
        target: Target,
    },
}

impl Op {
    fn op_id(&self) -> Uuid {
        match self {
            Op::Create { op_id, .. }
            | Op::Update { op_id, .. }
            | Op::Transition { op_id, .. }
            | Op::Note { op_id, .. }
            | Op::Duplicate { op_id, .. } => *op_id,
        }
    }
}

#[derive(Debug, Clone)]
struct Pre {
    lock_version: i32,
    field_values: Vec<(String, Json, String)>, // (key, raw value, path)
}

/// A document after structure checks, with issue keys resolved to ids.
#[derive(Debug)]
pub(crate) struct Parsed {
    ops: Vec<Op>,
    pre: BTreeMap<Uuid, Pre>,
    /// SHA-256 of the canonical normalized document.
    pub hash: [u8; 32],
    /// What the caller wrote for each issue id (for not_found messages).
    refs: BTreeMap<Uuid, String>,
    /// The normalized document (ids for keys, canonical numbers).
    pub norm: Json,
}

impl Parsed {
    /// (op_id, kind) in document order.
    pub fn op_list(&self) -> Vec<(Uuid, &'static str)> {
        self.ops
            .iter()
            .map(|o| {
                let k = match o {
                    Op::Create { .. } => perm::OP_CREATE_ISSUE,
                    Op::Update { .. } => perm::OP_UPDATE_FIELDS,
                    Op::Transition { .. } => perm::OP_TRANSITION,
                    Op::Note { .. } => perm::OP_ADD_NOTE,
                    Op::Duplicate { .. } => perm::OP_DUPLICATE_OF,
                };
                (o.op_id(), k)
            })
            .collect()
    }

    /// Existing target issues.
    pub fn targets(&self) -> Vec<Uuid> {
        self.pre.keys().copied().collect()
    }

    /// Projects where an issue is created, with the tracker.
    pub fn creates(&self) -> Vec<(usize, Uuid, Uuid)> {
        self.ops
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o {
                Op::Create {
                    project_id,
                    tracker_id,
                    ..
                } => Some((i, *project_id, *tracker_id)),
                _ => None,
            })
            .collect()
    }
}

/// Canonical JSON (§8.1): object keys sorted, arrays in order, no spaces.
pub fn canonical_json(v: &Json) -> String {
    fn go(v: &Json, out: &mut String) {
        match v {
            Json::Object(m) => {
                out.push('{');
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Json::String(k.clone()).to_string());
                    out.push(':');
                    go(&m[k], out);
                }
                out.push('}');
            }
            Json::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    go(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    go(v, &mut s);
    s
}

/// Step 2–4: syntax, structure, issue keys. Needs the connection only to
/// resolve keys (no locks; a key that does not resolve is `not_found`).
pub(crate) async fn parse(conn: &mut PgConnection, raw: &Json, max_ops: usize) -> Result<Parsed> {
    let Json::Object(top) = raw else {
        return Err(bad("", "the body is a command document object"));
    };
    // Classify first, so an unsupported kind or mode is not reported as a
    // generic syntax error.
    if let Some(m) = top.get("exec_mode")
        && m != "atomic"
    {
        return Err(ApiError::new(
            ErrorCode::UnsupportedExecutionMode,
            "only exec_mode \"atomic\" is supported",
        )
        .with_details(json!({"path": "exec_mode", "allowed": ["atomic"]}))
        .into());
    }
    if let Some(Json::Array(ops)) = top.get("ops") {
        for (i, op) in ops.iter().enumerate() {
            if let Some(k) = op.get("kind").and_then(Json::as_str)
                && !KINDS.contains(&k)
            {
                return Err(ApiError::new(
                    ErrorCode::UnsupportedOperation,
                    format!("operation kind {k:?} is not supported"),
                )
                .with_details(json!({"path": format!("ops[{i}].kind"), "allowed": KINDS}))
                .into());
            }
        }
    }
    let doc: CommandDoc = serde_json::from_value(raw.clone())
        .map_err(|e| bad("", format!("not a valid command document: {e}")))?;
    if doc.command_schema_version != 1 {
        return Err(bad(
            "command_schema_version",
            "command_schema_version must be 1",
        ));
    }
    if doc
        .title
        .as_ref()
        .is_some_and(|t| t.chars().count() > MAX_TITLE_CHARS)
    {
        return Err(bad("title", "title is at most 255 characters"));
    }
    if doc.evidence.len() > MAX_EVIDENCE {
        return Err(bad("evidence", "at most 50 evidence items"));
    }
    if doc.ops.is_empty() || doc.ops.len() > max_ops {
        return Err(bad("ops", format!("a document has 1–{max_ops} ops")));
    }

    // Resolve issue keys (ids stay as given; their existence is checked under lock).
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut collect = |t: &TargetRef| {
        if let TargetRef::Issue { key: Some(k), .. } = t {
            keys.insert(k.trim().to_uppercase());
        }
    };
    for op in &doc.ops {
        match op {
            OpInput::CreateIssue(_) => {}
            OpInput::UpdateFields(o) => collect(&o.target),
            OpInput::Transition(o) => collect(&o.target),
            OpInput::AddNote(o) => collect(&o.target),
            OpInput::DuplicateOf(o) => {
                collect(&o.source);
                collect(&o.target);
            }
        }
    }
    for p in &doc.preconditions {
        collect(&p.target);
    }
    let keys: Vec<String> = keys.into_iter().collect();
    let resolved: BTreeMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT rk.key, rk.issue_id FROM issue_keys rk JOIN issues i ON i.id = rk.issue_id
         WHERE rk.key = ANY($1) AND i.archived_at IS NULL",
    )
    .bind(&keys)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let mut refs: BTreeMap<Uuid, String> = BTreeMap::new();
    let mut binds: BTreeSet<String> = BTreeSet::new();
    let mut target = |t: &TargetRef, path: &str, binds: &BTreeSet<String>| -> Result<Target> {
        match t {
            TargetRef::Issue {
                id: Some(id),
                key: None,
            } => {
                refs.entry(*id).or_insert_with(|| id.to_string());
                Ok(Target::Issue(*id))
            }
            TargetRef::Issue {
                id: None,
                key: Some(k),
            } => {
                let norm = k.trim().to_uppercase();
                let id = *resolved.get(&norm).ok_or_else(|| issue_not_found(&norm))?;
                refs.entry(id).or_insert(norm);
                Ok(Target::Issue(id))
            }
            TargetRef::Issue { .. } => Err(bad(path, "an issue target has either id or key")),
            TargetRef::Temp { reference } if binds.contains(reference) => {
                Ok(Target::Temp(reference.clone()))
            }
            TargetRef::Temp { .. } => Err(bad(
                path,
                "a temp ref must name a bind declared by an earlier create_issue",
            )),
        }
    };

    let mut ops = Vec::new();
    let mut op_ids = BTreeSet::new();
    let mut set_fields: BTreeSet<(Target, Key)> = BTreeSet::new();
    let mut transitions: BTreeSet<Target> = BTreeSet::new();
    let mut has_dup = false;
    for (i, op) in doc.ops.iter().enumerate() {
        let p = |s: &str| format!("ops[{i}].{s}");
        let parse_writes = |m: &BTreeMap<String, Json>,
                            base: &str,
                            create: bool|
         -> Result<(Vec<Write>, Option<bool>)> {
            let mut out = Vec::new();
            let mut private = None;
            for (k, v) in m {
                let path = format!("{base}.{k}");
                if create && k == "is_private" {
                    private = Some(
                        v.as_bool()
                            .ok_or_else(|| bad(&path, "is_private is true or false"))?,
                    );
                    continue;
                }
                let key = Key::parse(k).ok_or_else(|| {
                    if k == "status_id" {
                        bad(&path, "change the status with a transition op")
                    } else {
                        bad(&path, format!("unknown field {k:?}"))
                    }
                })?;
                let (value, cf_type) = match key {
                    Key::Cf(_) => {
                        let (t, v) = parse_cf(&path, v)?;
                        (v, t)
                    }
                    _ => (parse_builtin(&path, key, v)?, None),
                };
                out.push(Write {
                    key,
                    value,
                    cf_type,
                    path,
                });
            }
            Ok((out, private))
        };
        let parsed = match op {
            OpInput::CreateIssue(o) => {
                if let Some(b) = &o.bind {
                    if b.is_empty() || b.len() > 64 {
                        return Err(bad(&p("bind"), "bind is 1–64 characters"));
                    }
                    if !binds.insert(b.clone()) {
                        return Err(bad(&p("bind"), format!("bind {b:?} is declared twice")));
                    }
                }
                let (fields, is_private) = parse_writes(&o.fields, &p("fields"), true)?;
                if !fields.iter().any(|w| w.key == Key::Subject) {
                    return Err(ApiError::new(
                        ErrorCode::RequiredFieldMissing,
                        "subject is required",
                    )
                    .with_details(json!({"op_index": i, "op_id": o.op_id, "fields": ["subject"]}))
                    .into());
                }
                Op::Create {
                    op_id: o.op_id,
                    bind: o.bind.clone(),
                    project_id: o.project_id,
                    tracker_id: o.tracker_id,
                    fields,
                    is_private,
                }
            }
            OpInput::UpdateFields(o) => {
                let t = target(&o.target, &p("target"), &binds)?;
                let (set, _) = parse_writes(&o.set, &p("set"), false)?;
                if set.is_empty() {
                    return Err(bad(&p("set"), "set names at least one field"));
                }
                for w in &set {
                    if !set_fields.insert((t.clone(), w.key)) {
                        return Err(bad(
                            &w.path,
                            "a field is set at most once per issue in one document",
                        ));
                    }
                }
                Op::Update {
                    op_id: o.op_id,
                    target: t,
                    set,
                }
            }
            OpInput::Transition(o) => {
                let t = target(&o.target, &p("target"), &binds)?;
                if !transitions.insert(t.clone()) {
                    return Err(bad(
                        &p("target"),
                        "at most one transition per issue in one document",
                    ));
                }
                Op::Transition {
                    op_id: o.op_id,
                    target: t,
                    to: o.to_status_id,
                }
            }
            OpInput::AddNote(o) => {
                let t = target(&o.target, &p("target"), &binds)?;
                if o.body.is_empty() || o.body.len() > MAX_NOTE_BYTES as usize {
                    return Err(bad(&p("body"), "a note body is 1–65536 bytes"));
                }
                Op::Note {
                    op_id: o.op_id,
                    target: t,
                    body: o.body.clone(),
                    private: o.is_private,
                }
            }
            OpInput::DuplicateOf(o) => {
                has_dup = true;
                let s = target(&o.source, &p("source"), &binds)?;
                let t = target(&o.target, &p("target"), &binds)?;
                Op::Duplicate {
                    op_id: o.op_id,
                    source: s,
                    target: t,
                }
            }
        };
        if !op_ids.insert(parsed.op_id()) {
            return Err(bad(&p("op_id"), "op_id is unique in the document"));
        }
        ops.push(parsed);
    }
    if has_dup && ops.len() != 1 {
        return Err(bad("ops", "duplicate_of is the only op of its document"));
    }

    // Preconditions: exactly one per existing target.
    let mut existing: BTreeSet<Uuid> = BTreeSet::new();
    for op in &ops {
        let mut add = |t: &Target| {
            if let Target::Issue(id) = t {
                existing.insert(*id);
            }
        };
        match op {
            Op::Create { .. } => {}
            Op::Update { target, .. } | Op::Transition { target, .. } | Op::Note { target, .. } => {
                add(target)
            }
            Op::Duplicate { source, target, .. } => {
                add(source);
                add(target);
            }
        }
    }
    let mut pre: BTreeMap<Uuid, Pre> = BTreeMap::new();
    for (i, p) in doc.preconditions.iter().enumerate() {
        let path = format!("preconditions[{i}].target");
        if matches!(p.target, TargetRef::Temp { .. }) {
            return Err(bad(&path, "a temp target has no precondition"));
        }
        let Target::Issue(id) = target(&p.target, &path, &binds)? else {
            unreachable!("issue refs resolve to issues")
        };
        if !existing.contains(&id) {
            return Err(bad(&path, "a precondition names an issue no op targets"));
        }
        let fv = p
            .field_values
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.clone(),
                    format!("preconditions[{i}].field_values.{k}"),
                )
            })
            .collect();
        if pre
            .insert(
                id,
                Pre {
                    lock_version: p.lock_version,
                    field_values: fv,
                },
            )
            .is_some()
        {
            return Err(bad(&path, "one precondition per target issue"));
        }
    }
    if let Some(missing) = existing.iter().find(|id| !pre.contains_key(id)) {
        return Err(bad(
            "preconditions",
            format!(
                "every existing target needs a precondition with lock_version (missing for {})",
                refs[missing]
            ),
        ));
    }

    // The hash covers the normalized document (§16.5): keys replaced by ids,
    // numbers in canonical form, so "12.50" and "12.5" are the same request.
    let mut norm = raw.clone();
    normalize_refs(&mut norm, &resolved);
    normalize_numbers(&mut norm);
    let hash: [u8; 32] =
        Sha256::digest(format!("oxledger/command/v1\n{}", canonical_json(&norm))).into();
    Ok(Parsed {
        ops,
        pre,
        hash,
        refs,
        norm,
    })
}

/// Rewrites `{"int"|"float": "…"}` values in ops and preconditions to
/// [`canonical_number`] (values that do not parse are left; parsing rejects them).
fn normalize_numbers(doc: &mut Json) {
    fn fix_map(m: &mut Json) {
        let Json::Object(m) = m else { return };
        for v in m.values_mut() {
            let Json::Object(w) = v else { continue };
            if w.len() != 1 {
                continue;
            }
            for (t, inner) in w.iter_mut() {
                if (t == "int" || t == "float")
                    && let Some(c) = inner.as_str().and_then(|s| canonical_number(s, t == "int"))
                {
                    *inner = Json::String(c);
                }
            }
        }
    }
    if let Some(ops) = doc.get_mut("ops").and_then(Json::as_array_mut) {
        for op in ops {
            for k in ["set", "fields"] {
                if let Some(m) = op.get_mut(k) {
                    fix_map(m);
                }
            }
        }
    }
    if let Some(pre) = doc.get_mut("preconditions").and_then(Json::as_array_mut) {
        for p in pre {
            if let Some(m) = p.get_mut("field_values") {
                fix_map(m);
            }
        }
    }
}

/// Replaces `{"type":"issue","key":K}` by `{"type":"issue","id":…}` in
/// targets, sources and precondition targets.
fn normalize_refs(doc: &mut Json, resolved: &BTreeMap<String, Uuid>) {
    let fix = |t: &mut Json| {
        if let Json::Object(m) = t
            && m.get("type").and_then(Json::as_str) == Some("issue")
            && let Some(Json::String(k)) = m.get("key").cloned()
            && let Some(id) = resolved.get(&k.trim().to_uppercase())
        {
            m.remove("key");
            m.insert("id".into(), json!(id));
        }
    };
    if let Some(Json::Array(ops)) = doc.get_mut("ops") {
        for op in ops {
            for f in ["target", "source"] {
                if let Some(t) = op.get_mut(f) {
                    fix(t);
                }
            }
        }
    }
    if let Some(Json::Array(ps)) = doc.get_mut("preconditions") {
        for p in ps {
            if let Some(t) = p.get_mut("target") {
                fix(t);
            }
        }
    }
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, sqlx::FromRow)]
struct IssueRow {
    id: Uuid,
    key: String,
    project_id: Uuid,
    tracker_id: Uuid,
    status_id: Uuid,
    subject: String,
    description: String,
    author_id: Uuid,
    created_on_behalf_of_id: Option<Uuid>,
    assigned_to_id: Option<Uuid>,
    priority_id: Uuid,
    fixed_version_id: Option<Uuid>,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
    is_private: bool,
    lock_version: i32,
    last_event_seq: i64,
}

impl IssueRow {
    fn builtin(&self) -> BTreeMap<Key, Json> {
        BTreeMap::from([
            (Key::Subject, json!(self.subject)),
            (Key::Description, json!(self.description)),
            (Key::AssignedTo, json!(self.assigned_to_id)),
            (Key::Priority, json!(self.priority_id)),
            (Key::Version, json!(self.fixed_version_id)),
            (
                Key::StartDate,
                json!(self.start_date.map(|d| d.to_string())),
            ),
            (Key::DueDate, json!(self.due_date.map(|d| d.to_string()))),
        ])
    }

    fn core(&self) -> IssueCore {
        IssueCore {
            id: self.id,
            key: self.key.clone(),
            project_id: self.project_id,
            tracker_id: self.tracker_id,
            status_id: self.status_id,
            author_id: self.author_id,
            created_on_behalf_of_id: self.created_on_behalf_of_id,
            assigned_to_id: self.assigned_to_id,
            is_private: self.is_private,
            lock_version: self.lock_version,
        }
    }
}

#[derive(Debug)]
struct PendingEvent {
    kind: &'static str,
    op_id: Uuid,
    payload: Json,
    note_id: Option<Uuid>,
    /// Field keys whose origin becomes this event.
    fields: Vec<String>,
}

/// One issue as the simulation sees it.
#[derive(Debug)]
struct Work {
    id: Uuid,
    /// Existing issues: the locked row (also the workflow baseline).
    orig: Option<IssueRow>,
    project_id: Uuid,
    tracker_id: Uuid,
    author_id: Uuid,
    on_behalf_of: Option<Uuid>,
    is_private: bool,
    status: Uuid,
    values: BTreeMap<Key, Json>,
    orig_values: BTreeMap<Key, Json>,
    events: Vec<PendingEvent>,
    notes: Vec<(Uuid, String, bool)>,
    dups: Vec<(Uuid, Uuid, Uuid)>, // (relation id, source, target)
    fields_touched: bool,
    effect: bool,
}

/// Per (project, tracker): what the actor's roles and the field definitions say.
struct Rules {
    roles: Vec<Uuid>,
    by_status: BTreeMap<Uuid, Vec<FieldRule>>,
    cfs: Vec<CfDef>,
    hidden: Vec<HiddenField>,
}

impl Rules {
    fn at(&self, status: Uuid) -> BTreeMap<String, Rule> {
        field_rules(
            self.by_status
                .get(&status)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            &self.roles,
            &self.hidden,
        )
    }
    fn cf(&self, id: i32) -> Option<&CfDef> {
        self.cfs.iter().find(|c| c.id == id)
    }
}

/// What the locks of steps 7b and 8 read.
#[derive(Default)]
struct Locked {
    versions: BTreeMap<Uuid, (Uuid, String, bool)>, // id → (project, status, deleted)
    options: BTreeMap<Uuid, (i32, bool)>,           // option → (field, active)
    priorities: BTreeMap<Uuid, bool>,               // id → active
    default_priority: Option<Uuid>,
}

// ---------------------------------------------------------------- entry point

/// Who is acting. People only in S4.
/// Who runs a human write: the person and their browser session.
pub(crate) struct Executor {
    pub user_id: Uuid,
    pub session_id: Option<Uuid>,
}

pub(crate) fn executor(ctx: &AuthContext) -> Result<Executor> {
    #[cfg(feature = "test-support")]
    if ctx.method() == AuthMethod::TestUser {
        return Ok(Executor {
            user_id: ctx.principal_id().unwrap_or_default(),
            session_id: None,
        });
    }
    let (user_id, session_id) = ctx.require_human_session()?;
    Ok(Executor {
        user_id,
        session_id: Some(session_id),
    })
}

pub(crate) fn is_retryable_db(e: &anyhow::Error) -> bool {
    #[cfg(feature = "test-support")]
    if e.downcast_ref::<crate::changesets::faults::InjectedDeadlock>()
        .is_some()
    {
        return true;
    }
    e.chain().any(|c| {
        matches!(c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::Database(d)) if matches!(d.code().as_deref(), Some("40P01" | "40001")))
    })
}

pub(crate) fn is_lock_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::Database(d)) if d.code().as_deref() == Some("55P03"))
    })
}

/// `POST /api/v1/commands` (§6.8): runs `doc` for the signed-in person under
/// the idempotency key `operation_id`.
pub async fn submit(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    doc: &Json,
) -> Result<CommandResult> {
    let who = executor(&caller)?;
    let mut attempt = 0;
    loop {
        let mut tx = app.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '10s'")
            .execute(&mut *tx)
            .await?;
        match run(&mut tx, caller, &who, operation_id, doc).await {
            Ok(Outcome::Done(r)) => {
                tx.commit().await?;
                return Ok(r);
            }
            Ok(Outcome::Replay(r)) => {
                tx.commit().await?;
                return Ok(r);
            }
            Err(e) => {
                drop(tx); // rollback, the operation request included
                if is_retryable_db(&e) && attempt < DEADLOCK_RETRIES {
                    attempt += 1;
                    continue;
                }
                if is_lock_timeout(&e) {
                    return Err(api(
                        ErrorCode::TemporarilyUnavailable,
                        "the issues are busy; send the same request again",
                    ));
                }
                return Err(e);
            }
        }
    }
}

enum Outcome {
    Done(CommandResult),
    Replay(CommandResult),
}

async fn run(
    tx: &mut sqlx::Transaction<'static, Postgres>,
    caller: AuthContext,
    who: &Executor,
    operation_id: Uuid,
    doc: &Json,
) -> Result<Outcome> {
    let parsed = parse(&mut *tx, doc, MAX_OPS as usize).await?;
    let fingerprint = fingerprint(ACTION_COMMAND, &parsed.hash);

    // Step 0: the operation request. A concurrent request with the same key
    // waits here on the unique index and then sees the committed row.
    let inserted = open_request(
        &mut *tx,
        who.user_id,
        operation_id,
        ACTION_COMMAND,
        &fingerprint,
    )
    .await?;
    let Some(request_id) = inserted else {
        return replay(&mut *tx, caller, who.user_id, operation_id, &fingerprint)
            .await
            .map(Outcome::Replay);
    };

    let result = evaluate(&mut *tx, caller, who, Mode::Direct, &parsed, request_id).await?;
    let result_ref = json!({
        "execution_id": result.execution_id,
        "consistent_after": result.search.consistent_after,
        "effects": result.effects.iter().map(|e| json!({
            "issue_id": e.issue_id, "lock_version": e.lock_version,
            "changed": e.changed, "created": e.created,
        })).collect::<Vec<_>>(),
        "items": result.items,
    });
    complete_request(&mut *tx, request_id, &result_ref).await?;
    Ok(Outcome::Done(result))
}

/// Step 0 (§6.8): inserts the operation request; `None` when the key was
/// used before (a concurrent request with the same key waits here on the
/// unique index and then sees the committed row).
pub(crate) async fn open_request(
    conn: &mut PgConnection,
    caller_id: Uuid,
    operation_id: Uuid,
    action: &str,
    fingerprint: &[u8; 32],
) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO operation_requests (caller_id, operation_id, action, fingerprint, status)
         VALUES ($1, $2, $3, $4, 'in_progress')
         ON CONFLICT (caller_id, operation_id) DO NOTHING RETURNING id",
    )
    .bind(caller_id)
    .bind(operation_id)
    .bind(action)
    .bind(&fingerprint[..])
    .fetch_optional(&mut *conn)
    .await?)
}

/// Marks the request completed with its ids-only result, kept [`RESULT_DAYS`].
pub(crate) async fn complete_request(
    conn: &mut PgConnection,
    request_id: Uuid,
    result_ref: &Json,
) -> Result<()> {
    sqlx::query(
        "UPDATE operation_requests SET status = 'completed', result_ref = $2,
                result_expires_at = now() + make_interval(days => $3)
         WHERE id = $1",
    )
    .bind(request_id)
    .bind(result_ref)
    .bind(RESULT_DAYS)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `sha256("oxledger/fp/v1\n" ‖ action ‖ "\n" ‖ hash)`.
pub(crate) fn fingerprint(action: &str, hash: &[u8]) -> [u8; 32] {
    Sha256::digest(
        [
            b"oxledger/fp/v1\n".as_slice(),
            action.as_bytes(),
            b"\n",
            hash,
        ]
        .concat(),
    )
    .into()
}

/// The stored result of a used key (§8.7): same action and content → its
/// `result_ref`; otherwise `idempotency_key_reused`, `operation_result_expired`
/// or `execution_in_progress`.
pub(crate) async fn stored_result(
    conn: &mut PgConnection,
    caller_id: Uuid,
    operation_id: Uuid,
    action: &str,
    fingerprint: &[u8; 32],
) -> Result<Json> {
    let (stored_action, fp, status, result_ref, expired): (
        String,
        Vec<u8>,
        String,
        Option<Json>,
        bool,
    ) = sqlx::query_as(
        "SELECT action, fingerprint, status, result_ref,
                    COALESCE(result_expires_at < now(), false)
             FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
    )
    .bind(caller_id)
    .bind(operation_id)
    .fetch_one(&mut *conn)
    .await?;
    if stored_action != action || fp.as_slice() != fingerprint.as_slice() {
        return Err(api(
            ErrorCode::IdempotencyKeyReused,
            "this Idempotency-Key was already used for a different request",
        ));
    }
    if expired || (status == "completed" && result_ref.is_none()) {
        return Err(api(
            ErrorCode::OperationResultExpired,
            "the result of this operation is no longer kept; it is not run again",
        ));
    }
    if status != "completed" {
        return Err(api(
            ErrorCode::ExecutionInProgress,
            "this operation is still running",
        ));
    }
    Ok(result_ref.unwrap_or(Json::Null))
}

/// Step 5: the executor and their session FOR SHARE, still valid. Returns the
/// session deadline, checked after every lock ([`check_deadline`]).
pub(crate) async fn lock_executor(
    conn: &mut PgConnection,
    who: &Executor,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let principal_ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active' FOR SHARE",
    )
    .bind(who.user_id)
    .fetch_optional(&mut *conn)
    .await?;
    if principal_ok.is_none() {
        return Err(api(
            ErrorCode::Unauthenticated,
            "the credentials are no longer valid",
        ));
    }
    let Some(s) = who.session_id else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT LEAST(s.idle_expires_at, s.absolute_expires_at)
         FROM browser_sessions s JOIN principals p ON p.id = s.user_id
         JOIN user_passwords up ON up.user_id = s.user_id
         WHERE s.id = $1 AND s.user_id = $2 AND {} FOR SHARE OF s",
        crate::auth::SESSION_VALID
    );
    let d: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(s)
        .bind(who.user_id)
        .fetch_optional(&mut *conn)
        .await?;
    d.map(Some).ok_or_else(|| {
        api(
            ErrorCode::Unauthenticated,
            "the credentials are no longer valid",
        )
    })
}

/// Step 9: the deadline against `clock_timestamp()`, after every lock.
pub(crate) async fn check_deadline(
    conn: &mut PgConnection,
    deadline: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    if let Some(d) = deadline {
        let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *conn)
            .await?;
        if now >= d {
            return Err(api(ErrorCode::Unauthenticated, "the session expired"));
        }
    }
    Ok(())
}

/// Step 9 with several deadlines, each raising its own error.
pub(crate) async fn check_deadlines(
    conn: &mut PgConnection,
    deadlines: &[(chrono::DateTime<chrono::Utc>, ErrorCode)],
) -> Result<()> {
    if deadlines.is_empty() {
        return Ok(());
    }
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    for (d, code) in deadlines {
        if now >= *d {
            let msg = match code {
                ErrorCode::ChangesetExpired => "the proposal expired",
                ErrorCode::DelegationUnavailable => "the delegation expired",
                _ => "the session expired",
            };
            return Err(api(*code, msg));
        }
    }
    Ok(())
}

/// Step 8b: the search epoch FOR SHARE and this transaction's xid, as a token.
pub(crate) async fn search_token(conn: &mut PgConnection) -> Result<String> {
    let (epoch, xid): (Uuid, String) = sqlx::query_as(
        "SELECT data_epoch, pg_current_xact_id()::text FROM search_settings FOR SHARE",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(token(epoch, &xid))
}

/// A `human_direct` execution: its ref and the append-only record.
pub(crate) async fn record_human_execution(
    conn: &mut PgConnection,
    request_id: Uuid,
    user_id: Uuid,
    hash: &[u8],
    consistent_after: &str,
) -> Result<Uuid> {
    let execution_id = Uuid::now_v7();
    sqlx::query("INSERT INTO execution_refs (id, kind) VALUES ($1, 'command')")
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO command_executions (id, operation_request_id, actor_id, origin, command_hash, consistent_after)
         VALUES ($1, $2, $3, 'human_direct', $4, $5)",
    )
    .bind(execution_id)
    .bind(request_id)
    .bind(user_id)
    .bind(hash)
    .bind(consistent_after)
    .execute(&mut *conn)
    .await?;
    Ok(execution_id)
}

/// The key was used before (§8.7). Same content → the stored result, with
/// visibility re-checked; different content → `idempotency_key_reused`.
async fn replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    caller_id: Uuid,
    operation_id: Uuid,
    fingerprint: &[u8; 32],
) -> Result<CommandResult> {
    let r = stored_result(
        &mut *conn,
        caller_id,
        operation_id,
        ACTION_COMMAND,
        fingerprint,
    )
    .await?;
    let scope = db::load_scope(&mut *conn, caller).await?;
    let stored: Vec<Json> = r["effects"].as_array().cloned().unwrap_or_default();
    let ids: Vec<Uuid> = stored
        .iter()
        .filter_map(|e| json_uuid(&e["issue_id"]))
        .collect();
    type ReplayRow = (Uuid, String, Uuid, Uuid, bool, Uuid, Option<Uuid>);
    let rows: Vec<ReplayRow> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = ANY($1) AND i.archived_at IS NULL",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let visible: BTreeMap<Uuid, String> = rows
        .into_iter()
        .filter(|r| {
            scope.can_see(&IssueFacts {
                project_id: r.2,
                tracker_id: r.3,
                is_private: r.4,
                author_id: r.5,
                assigned_to_id: r.6,
            })
        })
        .map(|r| (r.0, r.1))
        .collect();
    let effects = stored
        .iter()
        .filter_map(|e| {
            let id = json_uuid(&e["issue_id"])?;
            Some(Effect {
                issue_id: id,
                key: visible.get(&id)?.clone(),
                lock_version: e["lock_version"].as_i64()? as i32,
                changed: e["changed"].as_bool()?,
                created: e["created"].as_bool()?,
            })
        })
        .collect();
    let items = serde_json::from_value::<Vec<ItemResult>>(r["items"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|i| visible.contains_key(&i.issue_id))
        .collect();
    Ok(CommandResult {
        status: "succeeded".into(),
        execution_id: json_uuid(&r["execution_id"]).unwrap_or_default(),
        effects,
        items,
        search: SearchReceipt {
            consistent_after: r["consistent_after"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
    })
}

/// The consistent_after token (design 03 §4.3): `c1.<epoch b64url>.<xid8>`.
pub fn token(epoch: Uuid, xid: &str) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(epoch.as_bytes());
    format!("c1.{e}.{xid}")
}

// ---------------------------------------------------------------- evaluate

/// Who writes, for events, notes and field origins (§16.2).
#[derive(Debug, Clone)]
pub(crate) struct Attribution {
    pub origin: &'static str,
    pub actor_id: Uuid,
    pub actor_kind: &'static str,
    /// Author of new issues and notes, and their `created_on_behalf_of_id`.
    pub author_id: Uuid,
    pub author_on_behalf: Option<Uuid>,
    pub request_id: Uuid,
    pub command_execution_id: Option<Uuid>,
    /// The `execution_refs` id (field origins, relations, undo copies).
    pub execution_ref: Uuid,
    pub changeset: Option<(Uuid, i32)>,
    pub changeset_execution_id: Option<Uuid>,
    pub proposer_id: Option<Uuid>,
    pub approver_id: Option<Uuid>,
    pub on_behalf_of_id: Option<Uuid>,
    pub delegation_id: Option<Uuid>,
    pub agent_client_id: Option<Uuid>,
    /// Agent writes keep before/after values (§16.7).
    pub record_changes: bool,
}

impl Attribution {
    fn human(user: Uuid, request_id: Uuid, execution_id: Uuid) -> Self {
        Self {
            origin: "human_direct",
            actor_id: user,
            actor_kind: "user",
            author_id: user,
            author_on_behalf: None,
            request_id,
            command_execution_id: Some(execution_id),
            execution_ref: execution_id,
            changeset: None,
            changeset_execution_id: None,
            proposer_id: None,
            approver_id: None,
            on_behalf_of_id: None,
            delegation_id: None,
            agent_client_id: None,
            record_changes: false,
        }
    }
}

/// What a simulation needs beyond the document (§6.7).
pub(crate) struct EvalCtx {
    pub mode: Mode,
    /// Whose visibility and permissions apply.
    pub caller: AuthContext,
    /// A proposal's authority ceiling: replaces the caller's delegations.
    pub ceiling: Option<BTreeMap<Uuid, perm::DelegationScope>>,
    /// Checked at step 9, after the locks, each with the error it raises.
    pub deadlines: Vec<(chrono::DateTime<chrono::Utc>, ErrorCode)>,
    /// Author of new issues: (author, created_on_behalf_of). `None` = the actor.
    pub author: Option<(Uuid, Option<Uuid>)>,
}

/// The simulated end state (steps 6–14).
pub(crate) struct Sim {
    work: BTreeMap<Uuid, Work>,
    pub items: Vec<ItemResult>,
    /// The receipt token (step 8b); only when locking.
    pub token: Option<String>,
}

/// Errors §6.7 never stores with a proposal (steps 1–8): the caller may not
/// do this at all, or the request is malformed. Anything else is a domain
/// problem of the moment (step 9) and is kept as a blocked validation.
pub(crate) fn is_rejection(e: &anyhow::Error) -> bool {
    match crate::error::classify(e) {
        None => true,
        Some(a) => matches!(
            a.code,
            ErrorCode::ForbiddenOp
                | ErrorCode::NotFound
                | ErrorCode::InvalidRequest
                | ErrorCode::Unauthenticated
                | ErrorCode::ContextUnavailable
                | ErrorCode::DelegationUnavailable
                | ErrorCode::UnsupportedOperation
                | ErrorCode::UnsupportedExecutionMode
                | ErrorCode::TemporarilyUnavailable
        ),
    }
}

/// §6.7 apply steps 5–19 for `human_direct` (it starts at 5).
async fn evaluate(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    mode: Mode,
    doc: &Parsed,
    request_id: Uuid,
) -> Result<CommandResult> {
    debug_assert_eq!(mode, Mode::Direct);
    // Step 5: the executor and the session, FOR SHARE, still valid.
    let deadline = lock_executor(&mut *conn, who).await?;
    let ectx = EvalCtx {
        mode,
        caller,
        ceiling: None,
        deadlines: deadline
            .map(|d| (d, ErrorCode::Unauthenticated))
            .into_iter()
            .collect(),
        author: None,
    };
    let sim = simulate(&mut *conn, &ectx, doc).await?;
    let consistent_after = sim.token.clone().unwrap_or_default();
    let execution_id = record_human_execution(
        &mut *conn,
        request_id,
        who.user_id,
        &doc.hash[..],
        &consistent_after,
    )
    .await?;
    let at = Attribution::human(who.user_id, request_id, execution_id);
    let effects = write_effects(&mut *conn, doc, &sim, &at).await?;
    Ok(CommandResult {
        status: "succeeded".into(),
        execution_id,
        effects,
        items: sim.items,
        search: SearchReceipt { consistent_after },
    })
}

/// Steps 6–14. Direct and Apply lock (and read the receipt token, and check
/// the deadlines); Propose and Validate only read, and check preconditions
/// after the ops so a permission problem is reported before a stale one.
pub(crate) async fn simulate(conn: &mut PgConnection, ectx: &EvalCtx, doc: &Parsed) -> Result<Sim> {
    let locks = ectx.mode.locks();
    // Targets and their projects (project_id never changes, so reading it
    // before the locks is safe).
    let mut existing: BTreeSet<Uuid> = BTreeSet::new();
    let mut create_projects: BTreeSet<Uuid> = BTreeSet::new();
    for op in &doc.ops {
        let mut add = |t: &Target| {
            if let Target::Issue(id) = t {
                existing.insert(*id);
            }
        };
        match op {
            Op::Create { project_id, .. } => {
                create_projects.insert(*project_id);
            }
            Op::Update { target, .. } | Op::Transition { target, .. } | Op::Note { target, .. } => {
                add(target)
            }
            Op::Duplicate { source, target, .. } => {
                add(source);
                add(target);
            }
        }
    }
    let existing: Vec<Uuid> = existing.into_iter().collect();
    let issue_projects: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, project_id FROM issues WHERE id = ANY($1) AND archived_at IS NULL",
    )
    .bind(&existing)
    .fetch_all(&mut *conn)
    .await?;
    if let Some(missing) = existing
        .iter()
        .find(|id| !issue_projects.iter().any(|r| r.0 == **id))
    {
        return Err(issue_not_found(&doc.refs[missing]));
    }
    let mut projects: BTreeSet<Uuid> = issue_projects.iter().map(|r| r.1).collect();
    projects.extend(create_projects.iter().copied());

    // Step 6: projects in UUID order; FOR UPDATE where an issue is created.
    for p in &projects {
        let sql = if !locks {
            "SELECT 1 FROM projects WHERE id = $1"
        } else if create_projects.contains(p) {
            "SELECT 1 FROM projects WHERE id = $1 FOR UPDATE"
        } else {
            "SELECT 1 FROM projects WHERE id = $1 FOR SHARE"
        };
        let found: Option<i32> = sqlx::query_scalar(sql)
            .bind(p)
            .fetch_optional(&mut *conn)
            .await?;
        if found.is_none() {
            return Err(api(ErrorCode::NotFound, "no project"));
        }
    }

    // Step 7: issues FOR UPDATE in id order.
    let rows: Vec<IssueRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.status_id, i.subject, i.description,
                i.author_id, i.created_on_behalf_of_id, i.assigned_to_id, i.priority_id,
                i.fixed_version_id, i.start_date, i.due_date, i.is_private, i.lock_version,
                i.last_event_seq
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = ANY($1) ORDER BY i.id{}",
        if locks { " FOR UPDATE OF i" } else { "" }
    )))
    .bind(&existing)
    .fetch_all(&mut *conn)
    .await?;
    let rows: BTreeMap<Uuid, IssueRow> = rows.into_iter().map(|r| (r.id, r)).collect();

    // Step 7b: newly chosen versions FOR SHARE (§17.6).
    let mut vids: BTreeSet<Uuid> = BTreeSet::new();
    let mut cf_ids: BTreeSet<i32> = BTreeSet::new();
    let mut option_ids: BTreeSet<Uuid> = BTreeSet::new();
    for op in &doc.ops {
        let (writes, target) = match op {
            Op::Create { fields, .. } => (fields, None),
            Op::Update { set, target, .. } => (set, Some(target)),
            _ => continue,
        };
        for w in writes {
            match w.key {
                Key::Version => {
                    if let Some(v) = json_uuid(&w.value) {
                        let orig = match target {
                            Some(Target::Issue(id)) => {
                                rows.get(id).and_then(|r| r.fixed_version_id)
                            }
                            _ => None,
                        };
                        if orig != Some(v) {
                            vids.insert(v);
                        }
                    }
                }
                Key::Cf(id) => {
                    cf_ids.insert(id);
                    if let Some(o) = w.value.get("list").and_then(json_uuid_ref) {
                        option_ids.insert(o);
                    }
                }
                _ => {}
            }
        }
    }
    let mut locked = Locked::default();
    let vrows: Vec<(Uuid, Uuid, String, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, project_id, status, deleted_at IS NOT NULL FROM versions
         WHERE id = ANY($1) ORDER BY id{}",
        if locks { " FOR SHARE" } else { "" }
    )))
    .bind(vids.iter().copied().collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    locked.versions = vrows
        .into_iter()
        .map(|(i, p, s, d)| (i, (p, s, d)))
        .collect();

    // Step 8: custom field definitions FOR SHARE, list options FOR KEY SHARE.
    let dropping: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM field_defs WHERE id = ANY($1) AND index_state = 'dropping' ORDER BY id",
    )
    .bind(cf_ids.iter().copied().collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    if locks {
        sqlx::query("SELECT id FROM field_defs WHERE id = ANY($1) ORDER BY id FOR SHARE")
            .bind(cf_ids.iter().copied().collect::<Vec<_>>())
            .execute(&mut *conn)
            .await?;
    }
    let orows: Vec<(Uuid, i32, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, field_id, active FROM field_options WHERE id = ANY($1) ORDER BY id{}",
        if locks { " FOR KEY SHARE" } else { "" }
    )))
    .bind(option_ids.iter().copied().collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    locked.options = orows.into_iter().map(|(i, f, a)| (i, (f, a))).collect();
    let prows: Vec<(Uuid, bool, bool)> =
        sqlx::query_as("SELECT id, active, is_default FROM issue_priorities")
            .fetch_all(&mut *conn)
            .await?;
    locked.default_priority = prows.iter().find(|p| p.2).map(|p| p.0);
    locked.priorities = prows.into_iter().map(|p| (p.0, p.1)).collect();

    // Step 8b: the search epoch FOR SHARE and this transaction's xid.
    let token = if locks {
        Some(search_token(&mut *conn).await?)
    } else {
        None
    };

    // Step 9: deadlines, after every lock above.
    if locks {
        check_deadlines(&mut *conn, &ectx.deadlines).await?;
    }

    // Step 10: visibility (invisible and missing look the same).
    let mut actor = perm::actor(&mut *conn, ectx.caller).await?;
    if let Some(c) = &ectx.ceiling {
        // A proposal runs within the authority it was proposed under.
        actor.delegations = Some(c.clone());
    }
    let scope = db::load_scope(&mut *conn, ectx.caller).await?;
    for (id, r) in &rows {
        let facts = IssueFacts {
            project_id: r.project_id,
            tracker_id: r.tracker_id,
            is_private: r.is_private,
            author_id: r.author_id,
            assigned_to_id: r.assigned_to_id,
        };
        if !scope.can_see(&facts) {
            return Err(issue_not_found(&doc.refs[id]));
        }
    }
    let visible_projects: BTreeSet<Uuid> = scope.project_ids().collect();
    for p in &create_projects {
        if !visible_projects.contains(p) {
            return Err(api(ErrorCode::NotFound, "no project"));
        }
    }
    // Locked at step 8, reported only now: before visibility it would tell
    // an invisible issue from a missing one.
    if let Some(f) = dropping.first() {
        return Err(invalid_value(
            &format!("cf:{f}"),
            format!("custom field cf:{f} is being deleted"),
        ));
    }

    let mut perms: BTreeMap<Uuid, ProjectPerms> = BTreeMap::new();
    for p in &projects {
        let pp = perm::project_perms(&mut *conn, &actor, *p)
            .await?
            .ok_or_else(|| api(ErrorCode::NotFound, "no project"))?;
        perms.insert(*p, pp);
    }
    let mut rules: BTreeMap<(Uuid, Uuid), Rules> = BTreeMap::new();
    let mut work: BTreeMap<Uuid, Work> = BTreeMap::new();
    for r in rows.values() {
        load_rules(&mut *conn, &mut rules, &perms, r.project_id, r.tracker_id).await?;
        let mut values = r.builtin();
        values.extend(load_cf_values(&mut *conn, r.id).await?);
        work.insert(
            r.id,
            Work {
                id: r.id,
                orig: Some(r.clone()),
                project_id: r.project_id,
                tracker_id: r.tracker_id,
                author_id: r.author_id,
                on_behalf_of: r.created_on_behalf_of_id,
                is_private: r.is_private,
                status: r.status_id,
                orig_values: values.clone(),
                values,
                events: Vec::new(),
                notes: Vec::new(),
                dups: Vec::new(),
                fields_touched: false,
                effect: false,
            },
        );
    }

    // Step 11: op-level authority on existing targets (the actor's current
    // permissions, §6.2), before preconditions so a stale document for an
    // operation the caller may not do at all says so.
    for (i, op) in doc.ops.iter().enumerate() {
        let check = |t: &Target, need: &str| -> Result<()> {
            let Target::Issue(id) = t else { return Ok(()) };
            let w = &work[id];
            let pp = &perms[&w.project_id];
            let own = perm::is_own(actor.user_id, w.author_id, w.on_behalf_of);
            let ok = match need {
                "edit" => pp.attributes_editable(w.tracker_id, own),
                "note" => pp.writable() && pp.allowed(perm::ADD_ISSUE_NOTES, w.tracker_id),
                _ => true,
            };
            let kind = match op {
                Op::Update { .. } => perm::OP_UPDATE_FIELDS,
                Op::Transition { .. } => perm::OP_TRANSITION,
                Op::Note { .. } => perm::OP_ADD_NOTE,
                Op::Duplicate { .. } => perm::OP_DUPLICATE_OF,
                Op::Create { .. } => perm::OP_CREATE_ISSUE,
            };
            if !ok || !actor.delegation_allows(w.project_id, kind, None) {
                let reason = if pp.writable() {
                    "permission"
                } else {
                    "project_not_writable"
                };
                return Err(op_err(
                    ErrorCode::ForbiddenOp,
                    i,
                    op.op_id(),
                    "you may not do this to this issue",
                    json!({"reason": reason}),
                ));
            }
            Ok(())
        };
        match op {
            Op::Update { target, .. } | Op::Transition { target, .. } => check(target, "edit")?,
            Op::Note { target, .. } => check(target, "note")?,
            Op::Duplicate { source, target, .. } => {
                check(source, "edit")?;
                check(target, "edit")?;
            }
            Op::Create { .. } => {}
        }
    }

    let pre = check_preconditions(doc, &work, &rules, &perms, &actor);
    let pre = if locks {
        pre?;
        Ok(())
    } else {
        pre
    };
    let author = ectx
        .author
        .unwrap_or((actor.user_id.expect("a person or a delegator"), None));
    let ran = run_ops(
        &mut *conn, doc, &actor, &perms, &mut rules, &mut work, &locked, author,
    )
    .await;
    let items = match (pre, ran) {
        (_, Err(e)) if is_rejection(&e) => return Err(e),
        (Err(p), _) => return Err(p),
        (Ok(()), r) => r?,
    };
    Ok(Sim { work, items, token })
}

/// Step 12: preconditions — readable fields first, then versions and values.
fn check_preconditions(
    doc: &Parsed,
    work: &BTreeMap<Uuid, Work>,
    rules: &BTreeMap<(Uuid, Uuid), Rules>,
    perms: &BTreeMap<Uuid, ProjectPerms>,
    actor: &Actor,
) -> Result<()> {
    for (id, p) in &doc.pre {
        let w = &work[id];
        let r = &rules[&(w.project_id, w.tracker_id)];
        let pp = &perms[&w.project_id];
        let mut expected: Vec<(String, Json, Json)> = Vec::new();
        for (k, raw, path) in &p.field_values {
            let (key_name, want, have) = if k == "status_id" {
                let want = match raw {
                    Json::String(s) => Uuid::parse_str(s)
                        .map(|u| json!(u))
                        .map_err(|_| bad(path, "status_id is a uuid"))?,
                    _ => return Err(bad(path, "status_id is a uuid")),
                };
                (k.clone(), want, json!(w.status))
            } else {
                let key = Key::parse(k).ok_or_else(|| bad(path, format!("unknown field {k:?}")))?;
                let want = match key {
                    Key::Cf(cf) => {
                        match r.cf(cf) {
                            Some(def) if readable(def, actor, pp) => {}
                            _ => return Err(invalid_value(path, format!("unknown field {k:?}"))),
                        }
                        parse_cf(path, raw)?.1
                    }
                    Key::Description => match raw {
                        Json::Null => json!(""),
                        _ => parse_builtin(path, key, raw)?,
                    },
                    Key::Priority if raw.is_null() => Json::Null,
                    Key::Subject => match raw {
                        Json::String(s) => json!(s),
                        _ => return Err(bad(path, "subject is a string")),
                    },
                    _ => parse_builtin(path, key, raw)?,
                };
                let have = w.values.get(&key).cloned().unwrap_or(Json::Null);
                (key.name(), want, have)
            };
            expected.push((key_name, want, have));
        }
        let actual = w.orig.as_ref().map(|o| o.lock_version).unwrap_or(0);
        if actual != p.lock_version {
            let o = w.orig.as_ref().expect("existing");
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!(
                    "{} changed since you read it (lock_version {} → {})",
                    o.key, p.lock_version, actual
                ),
            )
            .with_details(
                json!({"issue_id": id, "key": o.key, "expected_lock_version": p.lock_version,
                                 "lock_version": actual}),
            )
            .into());
        }
        for (k, want, have) in expected {
            if want != have {
                return Err(ApiError::new(
                    ErrorCode::PreconditionFailed,
                    format!("{k} does not have the expected value"),
                )
                .with_details(json!({"issue_id": id, "field": k}))
                .into());
            }
        }
    }
    Ok(())
}

/// Steps 13–14: every op in order on the loaded state, then the final checks.
#[allow(clippy::too_many_arguments)]
async fn run_ops(
    conn: &mut PgConnection,
    doc: &Parsed,
    actor: &Actor,
    perms: &BTreeMap<Uuid, ProjectPerms>,
    rules: &mut BTreeMap<(Uuid, Uuid), Rules>,
    work: &mut BTreeMap<Uuid, Work>,
    locked: &Locked,
    author_of: (Uuid, Option<Uuid>),
) -> Result<Vec<ItemResult>> {
    // Step 13: simulate every op in order.
    let mut temps: BTreeMap<String, Uuid> = BTreeMap::new();
    let mut items: Vec<ItemResult> = Vec::new();
    for (i, op) in doc.ops.iter().enumerate() {
        let resolve = |t: &Target| -> Uuid {
            match t {
                Target::Issue(id) => *id,
                Target::Temp(name) => temps[name],
            }
        };
        let fail =
            |code: ErrorCode, msg: String, extra: Json| op_err(code, i, op.op_id(), msg, extra);
        // Step 11 for an issue created earlier in this document (it did not
        // exist at step 11): the creator is the author, so edit_own suffices.
        let temp_need = match op {
            Op::Update {
                target: t @ Target::Temp(_),
                ..
            } => Some((t, perm::OP_UPDATE_FIELDS)),
            Op::Note {
                target: t @ Target::Temp(_),
                ..
            } => Some((t, perm::OP_ADD_NOTE)),
            _ => None,
        };
        if let Some((t, kind)) = temp_need {
            let w = &work[&resolve(t)];
            let pp = &perms[&w.project_id];
            let ok = if kind == perm::OP_ADD_NOTE {
                pp.writable() && pp.allowed(perm::ADD_ISSUE_NOTES, w.tracker_id)
            } else {
                pp.attributes_editable(
                    w.tracker_id,
                    perm::is_own(actor.user_id, w.author_id, w.on_behalf_of),
                )
            };
            if !ok || !actor.delegation_allows(w.project_id, kind, None) {
                return Err(fail(
                    ErrorCode::ForbiddenOp,
                    "you may not do this to this issue".into(),
                    json!({"reason": "permission"}),
                ));
            }
        }
        match op {
            Op::Create {
                op_id,
                bind,
                project_id,
                tracker_id,
                fields,
                is_private,
            } => {
                let pp = &perms[project_id];
                let tracker: Option<(Option<Uuid>, bool)> = sqlx::query_as(
                    "SELECT t.default_status_id, t.private_by_default FROM trackers t
                     JOIN project_trackers pt ON pt.tracker_id = t.id AND pt.project_id = $1
                     WHERE t.id = $2",
                )
                .bind(project_id)
                .bind(tracker_id)
                .fetch_optional(&mut *conn)
                .await?;
                let Some((Some(initial), private_default)) = tracker else {
                    return Err(fail(
                        ErrorCode::InvalidValue,
                        "the tracker is not enabled in this project".into(),
                        json!({"path": format!("ops[{i}].tracker_id")}),
                    ));
                };
                if !(pp.writable() && pp.allowed(perm::ADD_ISSUES, *tracker_id))
                    || !actor.delegation_allows(*project_id, perm::OP_CREATE_ISSUE, None)
                {
                    let reason = if pp.writable() {
                        "permission"
                    } else {
                        "project_not_writable"
                    };
                    return Err(fail(
                        ErrorCode::ForbiddenOp,
                        "you may not create issues of this tracker here".into(),
                        json!({"reason": reason}),
                    ));
                }
                let private = match is_private {
                    Some(true) => {
                        if !(pp.allowed(perm::SET_ISSUES_PRIVATE, *tracker_id)
                            || pp.allowed(perm::SET_OWN_ISSUES_PRIVATE, *tracker_id))
                        {
                            return Err(fail(
                                ErrorCode::ForbiddenOp,
                                "you may not create private issues".into(),
                                json!({"reason": "permission", "field": "is_private"}),
                            ));
                        }
                        true
                    }
                    Some(false) => false,
                    None => private_default,
                };
                load_rules(&mut *conn, rules, perms, *project_id, *tracker_id).await?;
                let id = Uuid::now_v7();
                let (author, on_behalf) = author_of;
                let mut values: BTreeMap<Key, Json> = BTreeMap::from([
                    (Key::Subject, Json::Null),
                    (Key::Description, json!("")),
                    (Key::AssignedTo, Json::Null),
                    (Key::Priority, json!(locked.default_priority)),
                    (Key::Version, Json::Null),
                    (Key::StartDate, Json::Null),
                    (Key::DueDate, Json::Null),
                ]);
                let mut w = Work {
                    id,
                    orig: None,
                    project_id: *project_id,
                    tracker_id: *tracker_id,
                    author_id: author,
                    on_behalf_of: on_behalf,
                    is_private: private,
                    status: initial,
                    orig_values: BTreeMap::new(),
                    values: BTreeMap::new(),
                    events: Vec::new(),
                    notes: Vec::new(),
                    dups: Vec::new(),
                    fields_touched: true,
                    effect: true,
                };
                let r = &rules[&(*project_id, *tracker_id)];
                let ro = r.at(initial);
                for wr in fields {
                    let v = check_write(
                        &mut *conn,
                        actor,
                        pp,
                        r,
                        &ro,
                        locked,
                        &w,
                        wr,
                        None,
                        i,
                        *op_id,
                        perm::OP_CREATE_ISSUE,
                    )
                    .await?;
                    values.insert(wr.key, v);
                }
                w.values = values;
                let mut payload: Map<String, Json> = w
                    .values
                    .iter()
                    .filter(|(_, v)| !is_empty(v))
                    .map(|(k, v)| (k.name(), v.clone()))
                    .collect();
                payload.insert("status_id".into(), json!(initial));
                payload.insert("is_private".into(), json!(private));
                let fields_list: Vec<String> = payload.keys().cloned().collect();
                w.events.push(PendingEvent {
                    kind: "created",
                    op_id: *op_id,
                    payload: json!({"fields": payload}),
                    note_id: None,
                    fields: fields_list,
                });
                if let Some(b) = bind {
                    temps.insert(b.clone(), id);
                }
                work.insert(id, w);
                items.push(ItemResult {
                    op_id: *op_id,
                    status: "applied".into(),
                    issue_id: id,
                });
            }
            Op::Update { op_id, target, set } => {
                let id = resolve(target);
                let (project_id, tracker_id, status) = {
                    let w = &work[&id];
                    (w.project_id, w.tracker_id, w.status)
                };
                let pp = &perms[&project_id];
                let r = &rules[&(project_id, tracker_id)];
                // §6.3: after a transition earlier in the document, the new status' rules.
                let ro = r.at(status);
                // Built-in and custom field values as loaded (§6.6: keeping an inactive option is no_effect).
                let orig = work[&id]
                    .orig
                    .as_ref()
                    .map(|_| work[&id].orig_values.clone());
                let mut changes = Map::new();
                let mut new_values = Vec::new();
                for wr in set {
                    let before = work[&id].values.get(&wr.key).cloned().unwrap_or(Json::Null);
                    let v = check_write(
                        &mut *conn,
                        actor,
                        pp,
                        r,
                        &ro,
                        locked,
                        &work[&id],
                        wr,
                        orig.as_ref(),
                        i,
                        *op_id,
                        perm::OP_UPDATE_FIELDS,
                    )
                    .await?;
                    if v != before {
                        changes.insert(wr.key.name(), json!({"old": before, "new": v}));
                    }
                    new_values.push((wr.key, v));
                }
                let w = work.get_mut(&id).expect("target");
                for (k, v) in new_values {
                    w.values.insert(k, v);
                }
                let applied = !changes.is_empty();
                if applied {
                    w.fields_touched = true;
                    w.effect = true;
                    let fields = changes.keys().cloned().collect();
                    w.events.push(PendingEvent {
                        kind: "fields_changed",
                        op_id: *op_id,
                        payload: json!({"changes": changes}),
                        note_id: None,
                        fields,
                    });
                }
                items.push(ItemResult {
                    op_id: *op_id,
                    status: if applied { "applied" } else { "no_effect" }.into(),
                    issue_id: id,
                });
            }
            Op::Transition { op_id, target, to } => {
                let id = resolve(target);
                let w = &work[&id];
                let pp = &perms[&w.project_id];
                let core = match &w.orig {
                    Some(o) => o.core(),
                    None => IssueCore {
                        id,
                        key: String::new(),
                        project_id: w.project_id,
                        tracker_id: w.tracker_id,
                        status_id: w.status,
                        author_id: w.author_id,
                        created_on_behalf_of_id: w.on_behalf_of,
                        assigned_to_id: w.values.get(&Key::AssignedTo).and_then(json_uuid),
                        is_private: w.is_private,
                        lock_version: 0,
                    },
                };
                if !pp.attributes_editable(w.tracker_id, core.is_own(actor)) {
                    return Err(fail(
                        ErrorCode::ForbiddenOp,
                        "you may not change this issue".into(),
                        json!({"reason": "permission"}),
                    ));
                }
                // Baseline (§6.3): the status and assignee at the start.
                let allowed = describe::allowed_transitions(&mut *conn, actor, pp, &core).await?;
                if !allowed.iter().any(|(s, _)| s == to) {
                    return Err(fail(
                        ErrorCode::InvalidTransition,
                        "this status is not an allowed transition".into(),
                        json!({"allowed": allowed.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>()}),
                    ));
                }
                let w = work.get_mut(&id).expect("target");
                let applied = *to != w.status;
                if applied {
                    w.events.push(PendingEvent {
                        kind: "status_changed",
                        op_id: *op_id,
                        payload: json!({"old": w.status, "new": to}),
                        note_id: None,
                        fields: vec!["status_id".into()],
                    });
                    w.status = *to;
                    w.fields_touched = true;
                    w.effect = true;
                }
                items.push(ItemResult {
                    op_id: *op_id,
                    status: if applied { "applied" } else { "no_effect" }.into(),
                    issue_id: id,
                });
            }
            Op::Note {
                op_id,
                target,
                body,
                private,
            } => {
                let id = resolve(target);
                let w = &work[&id];
                let pp = &perms[&w.project_id];
                if !(pp.writable() && pp.allowed(perm::ADD_ISSUE_NOTES, w.tracker_id))
                    || !actor.delegation_allows(w.project_id, perm::OP_ADD_NOTE, None)
                {
                    return Err(fail(
                        ErrorCode::ForbiddenOp,
                        "you may not add notes to this issue".into(),
                        json!({"reason": "permission"}),
                    ));
                }
                if *private && !pp.allowed(perm::SET_NOTES_PRIVATE, w.tracker_id) {
                    return Err(fail(
                        ErrorCode::ForbiddenOp,
                        "you may not add private notes here".into(),
                        json!({"reason": "permission", "field": "is_private"}),
                    ));
                }
                let note_id = Uuid::now_v7();
                let w = work.get_mut(&id).expect("target");
                w.notes.push((note_id, body.clone(), *private));
                w.events.push(PendingEvent {
                    kind: "note_added",
                    op_id: *op_id,
                    payload: json!({"is_private": private}),
                    note_id: Some(note_id),
                    fields: Vec::new(),
                });
                w.effect = true;
                items.push(ItemResult {
                    op_id: *op_id,
                    status: "applied".into(),
                    issue_id: id,
                });
            }
            Op::Duplicate {
                op_id,
                source,
                target,
            } => {
                let (s, t) = (resolve(source), resolve(target));
                if s == t {
                    return Err(fail(
                        ErrorCode::RelationNotAllowed,
                        "an issue cannot duplicate itself".into(),
                        json!({"reason": "self"}),
                    ));
                }
                let (ws, wt) = (&work[&s], &work[&t]);
                if ws.project_id != wt.project_id {
                    return Err(fail(
                        ErrorCode::RelationNotAllowed,
                        "duplicates must be in the same project".into(),
                        json!({"reason": "cross_project"}),
                    ));
                }
                let pp = &perms[&ws.project_id];
                if !pp.allowed(perm::MANAGE_ISSUE_RELATIONS, ws.tracker_id) {
                    return Err(fail(
                        ErrorCode::ForbiddenOp,
                        "you may not manage relations of this issue".into(),
                        json!({"reason": "permission"}),
                    ));
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM issue_duplicates WHERE source_issue_id = $1 AND target_issue_id = $2)",
                )
                .bind(s)
                .bind(t)
                .fetch_one(&mut *conn)
                .await?;
                if !exists {
                    let rel = Uuid::now_v7();
                    let src = work.get_mut(&s).expect("source");
                    src.dups.push((rel, s, t));
                    src.effect = true;
                    src.events.push(PendingEvent {
                        kind: "duplicate_added",
                        op_id: *op_id,
                        payload: json!({"role": "source", "other_issue_id": t}),
                        note_id: None,
                        fields: Vec::new(),
                    });
                    let tgt = work.get_mut(&t).expect("target");
                    tgt.effect = true;
                    tgt.events.push(PendingEvent {
                        kind: "duplicate_added",
                        op_id: *op_id,
                        payload: json!({"role": "target", "other_issue_id": s}),
                        note_id: None,
                        fields: Vec::new(),
                    });
                }
                items.push(ItemResult {
                    op_id: *op_id,
                    status: if exists { "no_effect" } else { "applied" }.into(),
                    issue_id: s,
                });
            }
        }
    }

    // Step 14: final state — date order (R4) and required fields (§6.3).
    for w in work.values() {
        if !w.fields_touched {
            continue;
        }
        let at = |k: Key| w.values.get(&k).and_then(json_date);
        if let (Some(s), Some(d)) = (at(Key::StartDate), at(Key::DueDate))
            && s > d
        {
            return Err(
                ApiError::new(ErrorCode::InvalidValue, "start_date is after due_date")
                    .with_details(json!({"issue_id": w.id, "kind": "date_order",
                                     "fields": ["start_date", "due_date"]}))
                    .into(),
            );
        }
        let r = &rules[&(w.project_id, w.tracker_id)];
        let pp = &perms[&w.project_id];
        let at_final = r.at(w.status);
        let mut missing = Vec::new();
        let mut blocked = false;
        for k in BUILTIN_KEYS {
            let name = k.name();
            let required = k == Key::Subject || at_final.get(&name) == Some(&Rule::Required);
            if required && is_empty(w.values.get(&k).unwrap_or(&Json::Null)) {
                missing.push(name);
            }
        }
        for cf in &r.cfs {
            let key = cf.key();
            let required = cf.is_required || at_final.get(&key) == Some(&Rule::Required);
            if !required || !is_empty(w.values.get(&Key::Cf(cf.id)).unwrap_or(&Json::Null)) {
                continue;
            }
            if readable(cf, actor, pp) {
                missing.push(key);
            } else if actor.is_agent() && at_final.get(&key) == Some(&Rule::Required) {
                // Only agents can be blocked by a field they cannot read (§5.2).
                blocked = true;
            }
        }
        if !missing.is_empty() || blocked {
            let mut d = json!({"issue_id": w.id, "fields": missing});
            if blocked {
                d["blocked"] = json!(true);
            }
            return Err(ApiError::new(
                ErrorCode::RequiredFieldMissing,
                "required fields are empty",
            )
            .with_details(d)
            .into());
        }
    }
    Ok(items)
}

/// Steps 15–17: issues, custom fields, notes, relations, events, field
/// origins and one `lock_version` bump per issue with an effect.
pub(crate) async fn write_effects(
    conn: &mut PgConnection,
    doc: &Parsed,
    sim: &Sim,
    at: &Attribution,
) -> Result<Vec<Effect>> {
    let (work, items) = (&sim.work, &sim.items);
    let execution_id = at.execution_ref;

    let mut effects = Vec::new();
    // New issues in op order (keys are numbered in document order).
    let created: Vec<Uuid> = doc
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Op::Create { .. }))
        .map(|(i, _)| items[i].issue_id)
        .collect();
    let mut order: Vec<Uuid> = created.clone();
    order.extend(work.keys().filter(|k| !created.contains(k)).copied());
    for id in order {
        let w = &work[&id];
        let key;
        let lock_version;
        if let Some(o) = &w.orig {
            key = o.key.clone();
            lock_version = if w.effect {
                o.lock_version + 1
            } else {
                o.lock_version
            };
            if w.effect {
                write_cfs(&mut *conn, w).await?;
            }
        } else {
            key = insert_issue(&mut *conn, w).await?;
            lock_version = 0;
        }
        if w.effect {
            for (note_id, body, private) in &w.notes {
                sqlx::query(
                    "INSERT INTO issue_notes (id, issue_id, author_id, created_on_behalf_of_id, body, is_private)
                     VALUES ($1,$2,$3,$4,$5,$6)",
                )
                .bind(note_id)
                .bind(id)
                .bind(at.author_id)
                .bind(at.author_on_behalf)
                .bind(body)
                .bind(private)
                .execute(&mut *conn)
                .await?;
            }
            for (rel, s, t) in &w.dups {
                sqlx::query(
                    "INSERT INTO issue_duplicates (id, source_issue_id, target_issue_id, created_by_execution_id)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(rel)
                .bind(s)
                .bind(t)
                .bind(execution_id)
                .execute(&mut *conn)
                .await?;
                sqlx::query(
                    "INSERT INTO execution_relation_changes (execution_id, relation_id, action) VALUES ($1, $2, 'created')",
                )
                .bind(execution_id)
                .bind(rel)
                .execute(&mut *conn)
                .await?;
            }
            let base = w.orig.as_ref().map(|o| o.last_event_seq).unwrap_or(0);
            for (n, ev) in w.events.iter().enumerate() {
                let event_id: Uuid = sqlx::query_scalar(
                    "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                               proposer_id, approver_id, on_behalf_of_id, delegation_id,
                                               agent_client_id, operation_request_id, command_execution_id,
                                               changeset_id, changeset_version, execution_id, op_id,
                                               note_id, payload)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) RETURNING id",
                )
                .bind(id)
                .bind(base + 1 + n as i64)
                .bind(ev.kind)
                .bind(at.origin)
                .bind(at.actor_id)
                .bind(at.actor_kind)
                .bind(at.proposer_id)
                .bind(at.approver_id)
                .bind(at.on_behalf_of_id)
                .bind(at.delegation_id)
                .bind(at.agent_client_id)
                .bind(at.request_id)
                .bind(at.command_execution_id)
                .bind(at.changeset.map(|c| c.0))
                .bind(at.changeset.map(|c| c.1))
                .bind(at.changeset_execution_id)
                .bind(ev.op_id)
                .bind(ev.note_id)
                .bind(&ev.payload)
                .fetch_one(&mut *conn)
                .await?;
                for f in &ev.fields {
                    sqlx::query(
                        "INSERT INTO issue_field_origins (issue_id, field_key, origin_execution_id, origin_event_id)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (issue_id, field_key) DO UPDATE
                           SET origin_execution_id = EXCLUDED.origin_execution_id,
                               origin_event_id = EXCLUDED.origin_event_id",
                    )
                    .bind(id)
                    .bind(f)
                    .bind(execution_id)
                    .bind(event_id)
                    .execute(&mut *conn)
                    .await?;
                }
            }
            if let (true, Some(o)) = (at.record_changes, w.orig.as_ref()) {
                // Agent writes keep before/after copies for undo (§16.7).
                let mut changed: Vec<(String, Json, Json)> = w
                    .values
                    .iter()
                    .filter(|(k, v)| w.orig_values.get(*k).unwrap_or(&Json::Null) != *v)
                    .map(|(k, v)| {
                        (
                            k.name(),
                            w.orig_values.get(k).cloned().unwrap_or(Json::Null),
                            v.clone(),
                        )
                    })
                    .collect();
                if o.status_id != w.status {
                    changed.push(("status_id".into(), json!(o.status_id), json!(w.status)));
                }
                for (k, before, after) in changed {
                    sqlx::query(
                        "INSERT INTO execution_field_changes (execution_id, issue_id, field_key, before_value, after_value)
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(execution_id)
                    .bind(id)
                    .bind(k)
                    .bind(before)
                    .bind(after)
                    .execute(&mut *conn)
                    .await?;
                }
            }
            let last = base + w.events.len() as i64;
            if w.orig.is_some() {
                // Step 17: one UPDATE per issue with an effect (the outbox trigger fires once).
                update_issue(&mut *conn, w, last).await?;
            }
        }
        effects.push(Effect {
            issue_id: id,
            key,
            lock_version,
            changed: w.effect,
            created: w.orig.is_none(),
        });
    }

    Ok(effects)
}

fn json_uuid_ref(v: &Json) -> Option<Uuid> {
    json_uuid(v)
}

async fn load_rules(
    conn: &mut PgConnection,
    cache: &mut BTreeMap<(Uuid, Uuid), Rules>,
    perms: &BTreeMap<Uuid, ProjectPerms>,
    project: Uuid,
    tracker: Uuid,
) -> Result<()> {
    if cache.contains_key(&(project, tracker)) {
        return Ok(());
    }
    let pp = &perms[&project];
    let roles = pp.workflow_roles();
    let by_status = rules_by_status(&mut *conn, tracker, &roles).await?;
    let cfs = applicable_fields(&mut *conn, project, tracker).await?;
    let hidden = cfs
        .iter()
        .filter(|c| !c.visible)
        .map(|c| HiddenField {
            field_key: c.key(),
            roles: c.roles.clone(),
        })
        .collect();
    cache.insert(
        (project, tracker),
        Rules {
            roles,
            by_status,
            cfs,
            hidden,
        },
    );
    Ok(())
}

/// Current custom field values of an issue, canonical.
async fn load_cf_values(conn: &mut PgConnection, issue: Uuid) -> Result<BTreeMap<Key, Json>> {
    type CfRow = (
        i32,
        String,
        Option<String>,
        Option<NaiveDate>,
        Option<String>,
        Option<Uuid>,
        Option<bool>,
    );
    let rows: Vec<CfRow> = sqlx::query_as(
        "SELECT c.field_id, d.ftype, c.v_num::text, c.v_date, c.v_text, c.v_ref, c.v_bool
             FROM issue_cf c JOIN field_defs d ON d.id = c.field_id WHERE c.issue_id = $1",
    )
    .bind(issue)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(f, t, num, date, text, r, b)| {
            let v = match t.as_str() {
                "int" | "float" => json!({ t.clone(): canonical_number(&num?, false)? }),
                "date" => json!({"date": date?.to_string()}),
                "list" | "user" => json!({ t.clone(): r? }),
                "bool" => json!({"bool": b?}),
                _ => json!({ t.clone(): text? }),
            };
            Some((Key::Cf(f), v))
        })
        .collect())
}

/// §6.4 and §6.6 for one field write; returns the value to store.
#[allow(clippy::too_many_arguments)]
async fn check_write(
    conn: &mut PgConnection,
    actor: &Actor,
    pp: &ProjectPerms,
    rules: &Rules,
    readonly: &BTreeMap<String, Rule>,
    locked: &Locked,
    w: &Work,
    wr: &Write,
    orig: Option<&BTreeMap<Key, Json>>,
    i: usize,
    op_id: Uuid,
    op_kind: &str,
) -> Result<Json> {
    let name = wr.key.name();
    let detail = |extra: Json| {
        let mut d = json!({"op_index": i, "op_id": op_id, "field": name, "path": wr.path});
        if let (Json::Object(m), Json::Object(x)) = (&mut d, extra) {
            m.extend(x);
        }
        d
    };
    let unknown = || {
        ApiError::new(ErrorCode::InvalidValue, format!("unknown field {name:?}"))
            .with_details(detail(json!({})))
            .into()
    };
    // Exists, applies to this project × tracker, readable (§6.4; U2: an
    // unreadable field is never writable, workflow rules or not).
    let cf = match wr.key {
        Key::Cf(id) => match rules.cf(id) {
            Some(def) if readable(def, actor, pp) => Some(def),
            _ => return Err(unknown()),
        },
        _ => None,
    };
    let op = op_kind;
    if !actor.delegation_allows(w.project_id, op, Some(&name)) {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            format!("{name} is outside the delegation"),
        )
        .with_details(detail(json!({"reason": "delegation"})))
        .into());
    }
    if readonly.get(&name) == Some(&Rule::Readonly) {
        return Err(ApiError::new(
            ErrorCode::ReadonlyField,
            format!("{name} is read-only here"),
        )
        .with_details(detail(json!({})))
        .into());
    }
    let value = wr.value.clone();
    let original = orig.and_then(|o| o.get(&wr.key)).cloned();
    let unchanged = original.as_ref() == Some(&value)
        || (orig.is_some()
            && matches!(wr.key, Key::Cf(_))
            && value.is_null()
            && original.is_none());
    let invalid = |msg: String, extra: Json| -> anyhow::Error {
        ApiError::new(ErrorCode::InvalidValue, msg)
            .with_details(detail(extra))
            .into()
    };
    if let Some(def) = cf {
        if let Some(t) = wr.cf_type
            && t != def.ftype
        {
            return Err(invalid(
                format!("{name} is a {} field", def.ftype),
                json!({"allowed_type": def.ftype}),
            ));
        }
        if unchanged || value.is_null() {
            return Ok(value);
        }
        match def.ftype.as_str() {
            "list" => {
                let o = value.get("list").and_then(json_uuid);
                match o.and_then(|o| locked.options.get(&o)) {
                    Some((f, true)) if *f == def.id => {}
                    _ => {
                        let allowed: Vec<Uuid> = sqlx::query_scalar(
                            "SELECT id FROM field_options WHERE field_id = $1 AND active ORDER BY position, label",
                        )
                        .bind(def.id)
                        .fetch_all(&mut *conn)
                        .await?;
                        return Err(invalid(
                            format!("not an active option of {name}"),
                            json!({"allowed": allowed}),
                        ));
                    }
                }
            }
            "user" => {
                let u = value.get("user").and_then(json_uuid);
                let ok: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active')",
                )
                .bind(u)
                .fetch_one(&mut *conn)
                .await?;
                if !ok {
                    return Err(invalid(format!("{name} takes an active user"), json!({})));
                }
            }
            "string" | "link" => {
                if value[def.ftype.as_str()]
                    .as_str()
                    .is_some_and(|s| s.len() > MAX_CF_SHORT_BYTES)
                {
                    return Err(invalid(format!("{name} is at most 2000 bytes"), json!({})));
                }
            }
            "text"
                if value["text"]
                    .as_str()
                    .is_some_and(|s| s.len() > MAX_DESCRIPTION_BYTES as usize) =>
            {
                return Err(invalid(
                    format!("{name} is at most 262144 bytes"),
                    json!({}),
                ));
            }
            _ => {}
        }
        return Ok(value);
    }
    if unchanged {
        return Ok(value);
    }
    match wr.key {
        Key::AssignedTo => {
            if let Some(u) = json_uuid(&value) {
                let allowed =
                    describe::assignable(&mut *conn, w.project_id, Some(w.author_id)).await?;
                if !allowed.iter().any(|a| a.id == u) {
                    return Err(invalid(
                        "not someone issues here can be assigned to".into(),
                        json!({"options_source": format!("/api/v1/projects/{}/assignees", w.project_id)}),
                    ));
                }
            }
        }
        Key::Priority => {
            let p = json_uuid(&value);
            if !p
                .and_then(|p| locked.priorities.get(&p))
                .copied()
                .unwrap_or(false)
            {
                let allowed: Vec<Uuid> = locked
                    .priorities
                    .iter()
                    .filter(|(_, a)| **a)
                    .map(|(id, _)| *id)
                    .collect();
                return Err(invalid(
                    "not an active priority".into(),
                    json!({"allowed": allowed}),
                ));
            }
        }
        Key::Version => {
            // R1: exists, not deleted, same project (one shape for all three), open.
            if let Some(v) = json_uuid(&value) {
                match locked.versions.get(&v) {
                    Some((p, status, false)) if *p == w.project_id => {
                        if status != "open" {
                            return Err(ApiError::new(
                                ErrorCode::VersionNotOpen,
                                format!("the version is {status}"),
                            )
                            .with_details(detail(json!({"version_id": v, "status": status})))
                            .into());
                        }
                    }
                    _ => return Err(invalid("no such version in this project".into(), json!({}))),
                }
            }
        }
        _ => {}
    }
    Ok(value)
}

async fn insert_issue(conn: &mut PgConnection, w: &Work) -> Result<String> {
    let (pkey, number): (String, i32) = sqlx::query_as(
        "UPDATE projects SET issue_counter = issue_counter + 1 WHERE id = $1 RETURNING key, issue_counter",
    )
    .bind(w.project_id)
    .fetch_one(&mut *conn)
    .await?;
    let v = |k: Key| w.values.get(&k).cloned().unwrap_or(Json::Null);
    sqlx::query(
        "INSERT INTO issues (id, project_id, tracker_id, status_id, subject, description, author_id,
                             created_on_behalf_of_id, assigned_to_id, priority_id, fixed_version_id,
                             start_date, due_date, is_private, last_event_seq)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
    )
    .bind(w.id)
    .bind(w.project_id)
    .bind(w.tracker_id)
    .bind(w.status)
    .bind(v(Key::Subject).as_str().unwrap_or_default())
    .bind(v(Key::Description).as_str().unwrap_or_default())
    .bind(w.author_id)
    .bind(w.on_behalf_of)
    .bind(json_uuid(&v(Key::AssignedTo)))
    .bind(json_uuid(&v(Key::Priority)))
    .bind(json_uuid(&v(Key::Version)))
    .bind(json_date(&v(Key::StartDate)))
    .bind(json_date(&v(Key::DueDate)))
    .bind(w.is_private)
    // Every event of a new issue is known now, so its single INSERT sets the
    // final sequence and the outbox trigger fires once (§6.9).
    .bind(w.events.len() as i64)
    .execute(&mut *conn)
    .await?;
    let key = format!("{pkey}-{number}");
    sqlx::query("INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ($1,$2,$3,$4)")
        .bind(&key)
        .bind(w.id)
        .bind(w.project_id)
        .bind(number)
        .execute(&mut *conn)
        .await?;
    write_cfs(&mut *conn, w).await?;
    Ok(key)
}

async fn update_issue(conn: &mut PgConnection, w: &Work, last_event_seq: i64) -> Result<()> {
    let v = |k: Key| w.values.get(&k).cloned().unwrap_or(Json::Null);
    sqlx::query(
        "UPDATE issues SET status_id = $2, subject = $3, description = $4, assigned_to_id = $5,
                priority_id = $6, fixed_version_id = $7, start_date = $8, due_date = $9,
                lock_version = lock_version + 1, last_event_seq = $10, updated_at = now()
         WHERE id = $1",
    )
    .bind(w.id)
    .bind(w.status)
    .bind(v(Key::Subject).as_str().unwrap_or_default())
    .bind(v(Key::Description).as_str().unwrap_or_default())
    .bind(json_uuid(&v(Key::AssignedTo)))
    .bind(json_uuid(&v(Key::Priority)))
    .bind(json_uuid(&v(Key::Version)))
    .bind(json_date(&v(Key::StartDate)))
    .bind(json_date(&v(Key::DueDate)))
    .bind(last_event_seq)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Custom field values that differ from the original (all, for a new issue).
async fn write_cfs(conn: &mut PgConnection, w: &Work) -> Result<()> {
    for (k, v) in &w.values {
        let Key::Cf(field) = *k else { continue };
        let before = w.orig_values.get(k).unwrap_or(&Json::Null);
        if v == before {
            continue;
        }
        let Json::Object(m) = v else {
            sqlx::query("DELETE FROM issue_cf WHERE issue_id = $1 AND field_id = $2")
                .bind(w.id)
                .bind(field)
                .execute(&mut *conn)
                .await?;
            continue;
        };
        let Some((t, inner)) = m.iter().next() else {
            continue;
        };
        let (mut num, mut date, mut text, mut r, mut b) = (None, None, None, None, None);
        match t.as_str() {
            "int" | "float" => num = inner.as_str().map(String::from),
            "date" => date = inner.as_str().and_then(parse_date),
            "list" | "user" => r = json_uuid(inner),
            "bool" => b = inner.as_bool(),
            _ => text = inner.as_str().map(String::from),
        }
        sqlx::query(
            "INSERT INTO issue_cf (issue_id, field_id, v_num, v_date, v_text, v_ref, v_bool)
             VALUES ($1, $2, $3::numeric, $4, $5, $6, $7)
             ON CONFLICT (issue_id, field_id) DO UPDATE SET v_num = EXCLUDED.v_num,
               v_date = EXCLUDED.v_date, v_text = EXCLUDED.v_text, v_ref = EXCLUDED.v_ref,
               v_bool = EXCLUDED.v_bool",
        )
        .bind(w.id)
        .bind(field)
        .bind(num)
        .bind(date)
        .bind(text)
        .bind(r)
        .bind(b)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_canonical() {
        assert_eq!(canonical_number("12.50", false).as_deref(), Some("12.5"));
        assert_eq!(canonical_number("007", true).as_deref(), Some("7"));
        assert_eq!(canonical_number("-0.0", false).as_deref(), Some("0"));
        assert_eq!(canonical_number("-3.000", false).as_deref(), Some("-3"));
        assert_eq!(canonical_number("1.5", true), None);
        assert_eq!(canonical_number("1.", false), None);
        assert_eq!(canonical_number(".5", false), None);
        assert_eq!(canonical_number("1e3", false), None);
        assert_eq!(canonical_number("", false), None);
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v = json!({"b": 1, "a": [{"d": null, "c": "x"}]});
        assert_eq!(canonical_json(&v), r#"{"a":[{"c":"x","d":null}],"b":1}"#);
    }

    #[test]
    fn token_format() {
        let t = token(Uuid::nil(), "1234");
        assert_eq!(t, "c1.AAAAAAAAAAAAAAAAAAAAAA.1234");
    }

    #[test]
    fn field_keys() {
        assert_eq!(Key::parse("cf:7"), Some(Key::Cf(7)));
        assert_eq!(Key::parse("cf:07").map(Key::name).as_deref(), Some("cf:7"));
        assert_eq!(Key::parse("cf:0"), None);
        assert_eq!(Key::parse("cf:x"), None);
        assert_eq!(Key::parse("status_id"), None);
        assert_eq!(Key::parse("author_id"), None);
    }

    #[test]
    fn dates() {
        assert!(parse_date("2026-02-30").is_none());
        assert!(parse_date("2026-2-3").is_none());
        assert!(date_in_range(parse_date("1900-01-01").unwrap()));
        assert!(!date_in_range(parse_date("2200-01-01").unwrap()));
    }
}
