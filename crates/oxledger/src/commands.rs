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

use crate::auth::AuthContext;
#[cfg(feature = "test-support")]
use crate::auth::AuthMethod;
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
/// §8.9: evidence references per document.
const MAX_EVIDENCE: usize = 20;
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
    /// An agent's `command_submit` written directly (§16.5).
    AgentDirect,
}

impl Mode {
    /// Whether this mode takes the row locks and may write.
    pub fn locks(self) -> bool {
        matches!(self, Mode::Direct | Mode::Apply | Mode::AgentDirect)
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
    /// Contract 05 §3.6: the `rules_version` from `describe_form`; when the
    /// rules in force differ, the document fails with `form_rules_changed`.
    /// Not part of the idempotency hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_rules_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpInput {
    CreateIssue(CreateIssueOp),
    UpdateFields(UpdateFieldsOp),
    Transition(TransitionOp),
    AddNote(AddNoteOp),
    DuplicateOf(DuplicateOfOp),
    /// §17.9a: agents only; the only op of its document.
    CreateVersion(CreateVersionOp),
    /// §17.9a: agents only; the only op, one precondition on the version.
    UpdateVersion(UpdateVersionOp),
    /// Contract 05 §3.10.1: decide an issue waiting in Triage.
    TriageDecide(TriageDecideOp),
    /// Contract 05 §3.10.2: log time (with issue ops on the same issue, or alone).
    LogTime(LogTimeOp),
    /// Contract 05 §3.10.2: the only op, one precondition on the entry.
    UpdateTimeEntry(UpdateTimeEntryOp),
    /// Contract 05 §3.10.2: people only; the only op, one precondition.
    DeleteTimeEntry(DeleteTimeEntryOp),
}

const KINDS: &[&str] = &[
    "create_issue",
    "update_fields",
    "transition",
    "add_note",
    "duplicate_of",
    "create_version",
    "update_version",
    "triage_decide",
    "log_time",
    "update_time_entry",
    "delete_time_entry",
];

/// A time entry's fields (contract 05 §3.10.2). `hours` is a decimal
/// string; `author_id`, `created_on_behalf_of_id` and `project_id` cannot be
/// set here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeFields {
    /// `YYYY-MM-DD`; omitted = today in the org time zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_on: Option<String>,
    /// Decimal string, 0–1000, at most two decimals (`"1.50"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<Json>,
    /// Omitted = the project's default activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments: Option<String>,
    /// Whose time; omitted = the caller (an agent: its delegator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
}

/// `update_time_entry.set`: absent = unchanged; `issue: null` detaches.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeEntrySet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments: Option<String>,
    /// People only (with `log_time_for_other_users`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// `{"type": "issue", …}` in the same project, or `null`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub issue: Option<Option<TargetRef>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogTimeOp {
    pub op_id: Uuid,
    /// Accepted for symmetry with create_issue; nothing can refer to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Optional with `issue` (must then match its project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// An existing issue (`{"type": "issue", "id"|"key"}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<TargetRef>,
    pub fields: TimeFields,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTimeEntryOp {
    pub op_id: Uuid,
    /// `{"type": "time_entry", "id": …}`.
    pub target: TargetRef,
    pub set: TimeEntrySet,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteTimeEntryOp {
    pub op_id: Uuid,
    /// `{"type": "time_entry", "id": …}`.
    pub target: TargetRef,
}

/// A milestone's fields (§17.9a). Absent = not set; `effective_date: null`
/// clears the date. `status` only in `update_version`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub effective_date: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateVersionOp {
    pub op_id: Uuid,
    /// Accepted for symmetry with create_issue; nothing can refer to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    pub project_id: Uuid,
    /// `name` (required), `effective_date`, `description`.
    pub fields: VersionFields,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateVersionOp {
    pub op_id: Uuid,
    /// `{"type": "version", "id": …}`.
    pub target: TargetRef,
    pub set: VersionFields,
}

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
    /// `priority_id`, `fixed_version_id`, `start_date`, `due_date`,
    /// `estimated_hours` (decimal string, ≤ 2 decimals), `cf:<id>`.
    pub fields: BTreeMap<String, Json>,
    /// Design 06 R-M9: the description's format; omitted = `markdown`.
    /// Agents may write only `markdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateFieldsOp {
    pub op_id: Uuid,
    pub target: TargetRef,
    /// Field key → new value; `null` clears.
    pub set: BTreeMap<String, Json>,
    /// Design 06 R-M9: the description's format, only with `description` in
    /// `set`; omitted = the current format is kept. Agents may write only
    /// `markdown`, and must say so when the current description is `plain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

/// The format of a description or note (design 06 R-M9, K2): `markdown`
/// (CommonMark with GFM, the default for new text) or `plain` (text written
/// before M2, shown as it is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    Plain,
    Markdown,
}

impl TextFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            TextFormat::Plain => "plain",
            TextFormat::Markdown => "markdown",
        }
    }

    /// A stored value (the 0032 CHECKs allow only these two).
    pub fn from_db(s: &str) -> TextFormat {
        if s == "markdown" {
            TextFormat::Markdown
        } else {
            TextFormat::Plain
        }
    }
}

/// `issues.description_format`, `issue_notes.body_format` (0032 CHECKs).
pub const TEXT_FORMATS: [&str; 2] = ["plain", "markdown"];

/// `issue_events.origin` (0008 CHECK; `integration` from 0032).
pub const EVENT_ORIGINS: [&str; 6] = [
    "human_direct",
    "agent_direct",
    "external_agent",
    "seed",
    "import",
    "integration",
];

/// `command_executions.origin` (0008 CHECK; `integration` from 0032).
pub const EXECUTION_ORIGINS: [&str; 3] = ["agent_direct", "human_direct", "integration"];

/// `execution_field_changes.field_key` of the description's format (undo
/// copies only; it is not a field a document sets).
pub const DESCRIPTION_FORMAT_KEY: &str = "description_format";

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
    /// Design 06 R-M9: omitted = `markdown`; agents may write only
    /// `markdown`. A note's format never changes (notes are not edited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

/// `source` is the duplicate, `target` the original. Must be the only op.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateOfOp {
    pub op_id: Uuid,
    pub source: TargetRef,
    pub target: TargetRef,
}

/// What a `triage_decide` op decides (contract 05 §3.10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriageDecision {
    /// `state = accepted`; may come with update_fields, transition, add_note.
    Accept,
    /// `state = declined`; needs a transition to a closed status.
    Decline,
    /// `state = duplicate` and the duplicate relation to `duplicate_of`;
    /// needs a transition to a closed status.
    Duplicate,
    /// Hide from the inbox until `snooze_until` or new activity.
    Snooze,
    /// End a snooze now.
    Unsnooze,
}

impl TriageDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Duplicate => "duplicate",
            Self::Snooze => "snooze",
            Self::Unsnooze => "unsnooze",
        }
    }
    /// `issue_triage.state` after the decision.
    pub fn new_state(self) -> &'static str {
        match self {
            Self::Accept => "accepted",
            Self::Decline => "declined",
            Self::Duplicate => "duplicate",
            Self::Snooze | Self::Unsnooze => "pending",
        }
    }
}

/// Contract 05 §3.10.1. The A5 flags (`self_triage`, `same_sponsor`) are
/// computed by the server; they are not parameters.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriageDecideOp {
    pub op_id: Uuid,
    /// The issue waiting in Triage (an existing issue).
    pub target: TargetRef,
    pub decision: TriageDecision,
    /// `decision = duplicate` only: the original issue (same project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<TargetRef>,
    /// `decision = snooze` only: RFC 3339, 1 hour to 90 days from now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_until: Option<String>,
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
    /// A milestone (update_version and its precondition only).
    Version { id: Uuid },
    /// A time entry (update_time_entry, delete_time_entry and their
    /// precondition only).
    TimeEntry { id: Uuid },
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
    /// Time entries the document wrote (contract 05 §3.10.2); omitted when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub time_entries: Vec<crate::time_entries::TimeEntryEffect>,
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
    /// The issue the op wrote (issue ops).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<Uuid>,
    /// The milestone the op wrote (version ops).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<Uuid>,
    /// The time entry the op wrote (time ops, contract 05 §3.10.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_entry_id: Option<Uuid>,
}

/// What a version command did (§17.9a: the new id is in the response).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VersionEffect {
    pub version_id: Uuid,
    pub project_id: Uuid,
    pub lock_version: i32,
    pub changed: bool,
    pub created: bool,
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

/// Design 06 R-M9: an agent wrote `plain`, or rewrote a plain description
/// without `format: "markdown"`.
fn agent_format(fail: impl Fn(ErrorCode, String, Json) -> anyhow::Error) -> anyhow::Error {
    fail(
        ErrorCode::ForbiddenOp,
        "agents write Markdown only; send format \"markdown\" (describe text_format)".into(),
        json!({"reason": "agent_format"}),
    )
}

pub(crate) fn issue_not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no issue {what}"))
}

// ---------------------------------------------------------------- field values

/// A field a command may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Subject,
    Description,
    AssignedTo,
    Priority,
    Version,
    StartDate,
    DueDate,
    EstimatedHours,
    Cf(i32),
}

impl Key {
    pub(crate) fn parse(k: &str) -> Option<Key> {
        Some(match k {
            "subject" => Key::Subject,
            "description" => Key::Description,
            "assigned_to_id" => Key::AssignedTo,
            "priority_id" => Key::Priority,
            "fixed_version_id" => Key::Version,
            "start_date" => Key::StartDate,
            "due_date" => Key::DueDate,
            "estimated_hours" => Key::EstimatedHours,
            _ => {
                let n = k.strip_prefix("cf:")?;
                if n.is_empty() || n.len() > 9 || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Key::Cf(n.parse().ok().filter(|v| *v > 0)?)
            }
        })
    }

    pub(crate) fn name(self) -> String {
        match self {
            Key::Subject => "subject".into(),
            Key::Description => "description".into(),
            Key::AssignedTo => "assigned_to_id".into(),
            Key::Priority => "priority_id".into(),
            Key::Version => "fixed_version_id".into(),
            Key::StartDate => "start_date".into(),
            Key::DueDate => "due_date".into(),
            Key::EstimatedHours => "estimated_hours".into(),
            Key::Cf(id) => format!("cf:{id}"),
        }
    }
}

const BUILTIN_KEYS: [Key; 8] = [
    Key::Subject,
    Key::Description,
    Key::AssignedTo,
    Key::Priority,
    Key::Version,
    Key::StartDate,
    Key::DueDate,
    Key::EstimatedHours,
];

/// `issues.estimated_hours` is `numeric(7,2)` (design 05 §2.2).
pub const MAX_ESTIMATED_HOURS: &str = "99999.99";

/// An `estimated_hours` value from a document or the database → canonical
/// decimal string, or why not: `Err(None)` = not a decimal string,
/// `Err(Some(kind))` = out of range or too many decimals.
pub fn estimated_hours(s: &str) -> std::result::Result<String, Option<&'static str>> {
    // Decimal syntax first (no length limits here: those are range and
    // decimals questions, answered after the zeros that do not count).
    let (neg, digits) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (ip, fp) = match digits.split_once('.') {
        Some((i, f)) if !f.is_empty() => (i, f),
        Some(_) => return Err(None),
        None => (digits, ""),
    };
    if ip.is_empty()
        || !ip.bytes().all(|b| b.is_ascii_digit())
        || !fp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(None);
    }
    let ip = ip.trim_start_matches('0');
    let fp = fp.trim_end_matches('0');
    let zero = ip.is_empty() && fp.is_empty();
    if (neg && !zero) || ip.len() > 5 {
        return Err(Some("out_of_range"));
    }
    if fp.len() > 2 {
        return Err(Some("too_many_decimals"));
    }
    let mut out = if ip.is_empty() {
        "0".to_string()
    } else {
        ip.to_string()
    };
    if !fp.is_empty() {
        out.push('.');
        out.push_str(fp);
    }
    Ok(out)
}

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
pub(crate) fn parse_cf(path: &str, v: &Json) -> Result<(Option<&'static str>, Json)> {
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
pub(crate) fn parse_builtin(path: &str, key: Key, v: &Json) -> Result<Json> {
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
        Key::EstimatedHours => match v {
            Json::Null => Ok(Json::Null),
            Json::String(s) => match estimated_hours(s) {
                Ok(c) => Ok(json!(c)),
                Err(None) => Err(bad(
                    path,
                    "estimated_hours is a decimal string such as \"1.5\" or null",
                )),
                Err(Some(kind)) => Err(ApiError::new(
                    ErrorCode::InvalidValue,
                    "estimated_hours is between 0 and 99999.99 with at most two decimals",
                )
                .with_details(json!({"path": path, "kind": kind,
                                     "min": "0", "max": MAX_ESTIMATED_HOURS, "max_decimals": 2}))
                .into()),
            },
            _ => Err(bad(
                path,
                "estimated_hours is a decimal string such as \"1.5\" or null",
            )),
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

/// The issue an op writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestIssue {
    Existing(Uuid),
    /// Created by the op at this index of the same document.
    Created(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DestKind {
    /// A new issue: its readers depend on `private` (`None`: the tracker's
    /// default) and the assignee.
    NewIssue {
        project_id: Uuid,
        private: Option<bool>,
        assignee: Option<Uuid>,
    },
    /// Fields, a public note, a relation: readers of the issue.
    Content(DestIssue),
    /// A private note on the issue.
    PrivateNote(DestIssue),
    /// A milestone's name and description: public content of its project
    /// (§17.9a, M8). `New` = created in that project.
    Version(VersionDest),
    /// A time entry (contract 05 §3.5.4): its readers are the project's
    /// time readers (`all`) plus the person, cut to the issue's readers.
    Time(TimeDest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeDest {
    /// `project_id` may be omitted with an issue.
    New {
        project_id: Option<Uuid>,
        issue: Option<Uuid>,
    },
    /// `issue`: `set.issue` when the op moves the entry.
    Existing {
        entry_id: Uuid,
        issue: Option<Option<Uuid>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionDest {
    New(Uuid),
    Existing(Uuid),
}

/// A version document's one op, for `versions::plan`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum VersionCmd<'a> {
    Create {
        op_id: Uuid,
        project_id: Uuid,
        fields: &'a VersionFields,
    },
    Update {
        op_id: Uuid,
        version_id: Uuid,
        lock_version: i32,
        set: &'a VersionFields,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Destination {
    pub op_index: usize,
    pub kind: DestKind,
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
        format: TextFormat,
    },
    Update {
        op_id: Uuid,
        target: Target,
        set: Vec<Write>,
        /// With a description in `set` only; `None` keeps the current one.
        format: Option<TextFormat>,
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
        format: TextFormat,
    },
    Duplicate {
        op_id: Uuid,
        source: Target,
        target: Target,
    },
    CreateVersion {
        op_id: Uuid,
        project_id: Uuid,
        fields: VersionFields,
    },
    UpdateVersion {
        op_id: Uuid,
        version_id: Uuid,
        set: VersionFields,
    },
    Triage {
        op_id: Uuid,
        target: Target,
        decision: TriageDecision,
        dup: Option<Target>,
        snooze_until: Option<chrono::DateTime<chrono::Utc>>,
    },
    LogTime {
        op_id: Uuid,
        project_id: Option<Uuid>,
        issue: Option<Uuid>,
        fields: TimeFields,
    },
    UpdateTime {
        op_id: Uuid,
        entry_id: Uuid,
        set: TimeEntrySet,
        issue: Option<Option<Uuid>>,
    },
    DeleteTime {
        op_id: Uuid,
        entry_id: Uuid,
    },
}

impl Op {
    fn is_time(&self) -> bool {
        matches!(
            self,
            Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. }
        )
    }

    fn op_id(&self) -> Uuid {
        match self {
            Op::Create { op_id, .. }
            | Op::Update { op_id, .. }
            | Op::Transition { op_id, .. }
            | Op::Note { op_id, .. }
            | Op::Duplicate { op_id, .. }
            | Op::CreateVersion { op_id, .. }
            | Op::UpdateVersion { op_id, .. }
            | Op::Triage { op_id, .. }
            | Op::LogTime { op_id, .. }
            | Op::UpdateTime { op_id, .. }
            | Op::DeleteTime { op_id, .. } => *op_id,
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
    /// update_version's precondition.
    version_lock: Option<i32>,
    /// update_time_entry's / delete_time_entry's precondition.
    time_lock: Option<i32>,
    /// Contract 05 §3.6: the rules version the caller saw.
    pub expected_rules: Option<String>,
}

impl Parsed {
    /// `not_found` for issue `id`, named as the caller wrote it.
    pub(crate) fn issue_not_found(&self, id: Uuid) -> anyhow::Error {
        issue_not_found(self.refs.get(&id).map_or(&id.to_string(), |r| r))
    }

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
                    Op::CreateVersion { .. } => perm::OP_CREATE_VERSION,
                    Op::UpdateVersion { .. } => perm::OP_UPDATE_VERSION,
                    Op::Triage { .. } => perm::OP_TRIAGE_DECIDE,
                    Op::LogTime { .. } => perm::OP_LOG_TIME,
                    Op::UpdateTime { .. } => perm::OP_UPDATE_TIME_ENTRY,
                    Op::DeleteTime { .. } => perm::OP_DELETE_TIME_ENTRY,
                };
                (o.op_id(), k)
            })
            .collect()
    }

    /// Design 05 §3.12.4: the existing issues an agent write of this
    /// document counts toward (issue ops; `duplicate_of` counts both ends;
    /// time and version ops do not touch issues). Sorted, distinct.
    pub(crate) fn chain_issues(&self) -> Vec<Uuid> {
        let mut out = BTreeSet::new();
        let mut add = |t: &Target| {
            if let Target::Issue(id) = t {
                out.insert(*id);
            }
        };
        for o in &self.ops {
            match o {
                Op::Update { target, .. }
                | Op::Transition { target, .. }
                | Op::Note { target, .. } => add(target),
                Op::Triage { target, dup, .. } => {
                    add(target);
                    if let Some(d) = dup {
                        add(d);
                    }
                }
                Op::Duplicate { source, target, .. } => {
                    add(source);
                    add(target);
                }
                _ => {}
            }
        }
        out.into_iter().collect()
    }

    /// Existing target issues.
    pub fn targets(&self) -> Vec<Uuid> {
        self.pre.keys().copied().collect()
    }

    /// The op of a version document (§17.9a), if this is one.
    pub(crate) fn version_cmd(&self) -> Option<VersionCmd<'_>> {
        match self.ops.first()? {
            Op::CreateVersion {
                op_id,
                project_id,
                fields,
            } => Some(VersionCmd::Create {
                op_id: *op_id,
                project_id: *project_id,
                fields,
            }),
            Op::UpdateVersion {
                op_id,
                version_id,
                set,
            } => Some(VersionCmd::Update {
                op_id: *op_id,
                version_id: *version_id,
                lock_version: self.version_lock.expect("parsed with its precondition"),
                set,
            }),
            _ => None,
        }
    }

    /// Whether every op is a time op (planned by `time_entries::plan` alone).
    pub(crate) fn time_only(&self) -> bool {
        self.ops.iter().all(Op::is_time)
    }

    /// Whether the document has a time op.
    pub(crate) fn has_time(&self) -> bool {
        self.ops.iter().any(Op::is_time)
    }

    /// Whether the document deletes a time entry (people only).
    pub(crate) fn deletes_time(&self) -> bool {
        self.ops.iter().any(|o| matches!(o, Op::DeleteTime { .. }))
    }

    /// The time ops, for `time_entries::plan`.
    pub(crate) fn time_cmds(&self) -> Vec<crate::time_entries::TimeCmd<'_>> {
        use crate::time_entries::TimeCmd;
        self.ops
            .iter()
            .enumerate()
            .filter_map(|(index, o)| match o {
                Op::LogTime {
                    op_id,
                    project_id,
                    issue,
                    fields,
                } => Some(TimeCmd::Log {
                    index,
                    op_id: *op_id,
                    project_id: *project_id,
                    issue: *issue,
                    fields,
                }),
                Op::UpdateTime {
                    op_id,
                    entry_id,
                    set,
                    issue,
                } => Some(TimeCmd::Update {
                    index,
                    op_id: *op_id,
                    entry_id: *entry_id,
                    lock_version: self.time_lock.expect("parsed with its precondition"),
                    set,
                    issue: *issue,
                }),
                Op::DeleteTime { op_id, entry_id } => Some(TimeCmd::Delete {
                    index,
                    op_id: *op_id,
                    entry_id: *entry_id,
                    lock_version: self.time_lock.expect("parsed with its precondition"),
                }),
                _ => None,
            })
            .collect()
    }

    /// Where each op writes, for the audience check (§3.8 M8, M9; §6.7
    /// step 8). Temp refs resolve to the op that creates the issue.
    pub fn destinations(&self) -> Vec<Destination> {
        let binds: BTreeMap<&str, usize> = self
            .ops
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o {
                Op::Create { bind: Some(b), .. } => Some((b.as_str(), i)),
                _ => None,
            })
            .collect();
        let on = |t: &Target| match t {
            Target::Issue(id) => Some(DestIssue::Existing(*id)),
            Target::Temp(r) => binds.get(r.as_str()).map(|i| DestIssue::Created(*i)),
        };
        let mut out = Vec::new();
        for (i, o) in self.ops.iter().enumerate() {
            match o {
                Op::Create {
                    project_id,
                    fields,
                    is_private,
                    ..
                } => out.push(Destination {
                    op_index: i,
                    kind: DestKind::NewIssue {
                        project_id: *project_id,
                        private: *is_private,
                        assignee: fields
                            .iter()
                            .find(|w| matches!(w.key, Key::AssignedTo))
                            .and_then(|w| w.value.as_str())
                            .and_then(|v| v.parse().ok()),
                    },
                }),
                Op::Update { target, .. } | Op::Transition { target, .. } => {
                    if let Some(t) = on(target) {
                        out.push(Destination {
                            op_index: i,
                            kind: DestKind::Content(t),
                        });
                    }
                }
                Op::Note {
                    target, private, ..
                } => {
                    if let Some(t) = on(target) {
                        out.push(Destination {
                            op_index: i,
                            kind: if *private {
                                DestKind::PrivateNote(t)
                            } else {
                                DestKind::Content(t)
                            },
                        });
                    }
                }
                Op::Duplicate { source, target, .. } => {
                    for t in [source, target].into_iter().filter_map(on) {
                        out.push(Destination {
                            op_index: i,
                            kind: DestKind::Content(t),
                        });
                    }
                }
                Op::CreateVersion { project_id, .. } => out.push(Destination {
                    op_index: i,
                    kind: DestKind::Version(VersionDest::New(*project_id)),
                }),
                Op::UpdateVersion { version_id, .. } => out.push(Destination {
                    op_index: i,
                    kind: DestKind::Version(VersionDest::Existing(*version_id)),
                }),
                Op::Triage { target, dup, .. } => {
                    for t in std::iter::once(target).chain(dup).filter_map(on) {
                        out.push(Destination {
                            op_index: i,
                            kind: DestKind::Content(t),
                        });
                    }
                }
                Op::LogTime {
                    project_id, issue, ..
                } => out.push(Destination {
                    op_index: i,
                    kind: DestKind::Time(TimeDest::New {
                        project_id: *project_id,
                        issue: *issue,
                    }),
                }),
                Op::UpdateTime {
                    entry_id, issue, ..
                } => out.push(Destination {
                    op_index: i,
                    kind: DestKind::Time(TimeDest::Existing {
                        entry_id: *entry_id,
                        issue: *issue,
                    }),
                }),
                Op::DeleteTime { .. } => {}
            }
        }
        out
    }

    /// (op index, command kind, the field keys the agent asked for), for the
    /// delegation policy (§16.3 rule 3: a create's fields count; defaults the
    /// server fills in do not).
    pub(crate) fn policy_items(&self) -> Vec<(usize, &'static str, Vec<String>)> {
        let mut out: Vec<(usize, &'static str, Vec<String>)> = self
            .ops
            .iter()
            .enumerate()
            .map(|(i, o)| match o {
                Op::Create { fields, .. } => (
                    i,
                    perm::OP_CREATE_ISSUE,
                    fields.iter().map(|w| w.key.name()).collect(),
                ),
                Op::Update { set, .. } => (
                    i,
                    perm::OP_UPDATE_FIELDS,
                    set.iter().map(|w| w.key.name()).collect(),
                ),
                Op::Transition { .. } => (i, perm::OP_TRANSITION, Vec::new()),
                Op::Note { .. } => (i, perm::OP_ADD_NOTE, Vec::new()),
                Op::Duplicate { .. } => (i, perm::OP_DUPLICATE_OF, Vec::new()),
                // §17.9a: no field-level policy for versions.
                Op::CreateVersion { .. } => (i, perm::OP_CREATE_VERSION, Vec::new()),
                Op::UpdateVersion { .. } => (i, perm::OP_UPDATE_VERSION, Vec::new()),
                Op::Triage { .. } => (i, perm::OP_TRIAGE_DECIDE, Vec::new()),
                // Contract 05 §4.3: no field-level policy for time.
                Op::LogTime { .. } => (i, perm::OP_LOG_TIME, Vec::new()),
                Op::UpdateTime { .. } => (i, perm::OP_UPDATE_TIME_ENTRY, Vec::new()),
                Op::DeleteTime { .. } => (i, perm::OP_DELETE_TIME_ENTRY, Vec::new()),
            })
            .collect();
        // A duplicate decision also writes the duplicate relation: the
        // duplicate_of policy applies too (the stricter of the two wins).
        for (i, o) in self.ops.iter().enumerate() {
            if let Op::Triage {
                decision: TriageDecision::Duplicate,
                ..
            } = o
            {
                out.push((i, perm::OP_DUPLICATE_OF, Vec::new()));
            }
        }
        out
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
/// Contract 05 §3.10.2 composition: `update_time_entry` and
/// `delete_time_entry` are the only op of their document, with exactly one
/// precondition on the entry; `log_time` ops share one issue (or none, in one
/// project) and may come with `update_fields`, `transition` and `add_note`
/// on that same existing issue. Returns the edit's lock_version.
fn check_time_doc(
    ops: &[Op],
    preconditions: &[Precondition],
    has_time: bool,
    has_time_edit: bool,
) -> Result<Option<i32>> {
    if has_time_edit {
        if ops.len() != 1 {
            return Err(bad(
                "ops",
                "update_time_entry and delete_time_entry are the only op of their document",
            ));
        }
        let entry = match &ops[0] {
            Op::UpdateTime { entry_id, .. } | Op::DeleteTime { entry_id, .. } => *entry_id,
            _ => unreachable!("the only op is the edit"),
        };
        return match preconditions {
            [one]
                if matches!(one.target, TargetRef::TimeEntry { id } if id == entry)
                    && one.field_values.is_empty() =>
            {
                Ok(Some(one.lock_version))
            }
            _ => Err(bad(
                "preconditions",
                "a time entry edit needs exactly one precondition: the entry's lock_version",
            )),
        };
    }
    if preconditions
        .iter()
        .any(|p| matches!(p.target, TargetRef::TimeEntry { .. }))
    {
        return Err(bad(
            "preconditions",
            "a time entry precondition goes with update_time_entry or delete_time_entry",
        ));
    }
    if !has_time {
        return Ok(None);
    }
    let logs: Vec<(Option<Uuid>, Option<Uuid>)> = ops
        .iter()
        .filter_map(|o| match o {
            Op::LogTime {
                project_id, issue, ..
            } => Some((*project_id, *issue)),
            _ => None,
        })
        .collect();
    let issue = logs[0].1;
    if logs.iter().any(|l| l.1 != issue) {
        return Err(bad("ops", "the log_time ops of a document share one issue"));
    }
    if issue.is_none() && logs.iter().any(|l| l.0 != logs[0].0) {
        return Err(bad(
            "ops",
            "the log_time ops of a document share one project",
        ));
    }
    for (i, o) in ops.iter().enumerate() {
        let t = match o {
            Op::LogTime { .. } => continue,
            Op::Update { target, .. } | Op::Transition { target, .. } | Op::Note { target, .. } => {
                target
            }
            _ => {
                return Err(bad(
                    &format!("ops[{i}].kind"),
                    "log_time goes only with update_fields, transition and add_note on its issue",
                ));
            }
        };
        if issue.is_none_or(|id| *t != Target::Issue(id)) {
            return Err(bad(
                &format!("ops[{i}].target"),
                "ops next to log_time target the issue the time is logged on",
            ));
        }
    }
    Ok(None)
}

pub(crate) async fn parse(conn: &mut PgConnection, raw: &Json, max_ops: usize) -> Result<Parsed> {
    parse_seen(conn, raw, max_ops, None).await
}

/// [`parse`] for `caller`: an existing target the caller cannot see is
/// `not_found` before any check about its precondition, the same answer
/// as for an issue that does not exist (看不到＝不存在, M1.11 gate).
pub(crate) async fn parse_for(
    conn: &mut PgConnection,
    raw: &Json,
    max_ops: usize,
    caller: AuthContext,
) -> Result<Parsed> {
    let scope = db::write_scope(&mut *conn, caller).await?;
    parse_seen(conn, raw, max_ops, Some(&scope)).await
}

/// After [`parse_for`] refused `raw`: a resend of the very request
/// `caller_id` already made under `operation_id` replays (the replay re-checks
/// visibility itself, 02 M05), so it is parsed without the cut. Any other
/// document keeps the cut's answer (`None`); `fp` is the caller's fingerprint
/// of a parsed document.
pub(crate) async fn resent(
    conn: &mut PgConnection,
    raw: &Json,
    max_ops: usize,
    caller_id: Uuid,
    operation_id: Uuid,
    fp: impl Fn(&Parsed) -> [u8; 32],
) -> Result<Option<Parsed>> {
    let stored: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT fingerprint FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
    )
    .bind(caller_id)
    .bind(operation_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let Ok(p) = parse(&mut *conn, raw, max_ops).await else {
        return Ok(None);
    };
    Ok((stored.as_slice() == fp(&p).as_slice()).then_some(p))
}

async fn parse_seen(
    conn: &mut PgConnection,
    raw: &Json,
    max_ops: usize,
    seen: Option<&ox_core::visibility::Scope>,
) -> Result<Parsed> {
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
        return Err(bad("evidence", "at most 20 evidence items"));
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
            OpInput::CreateVersion(_) | OpInput::UpdateVersion(_) => {}
            OpInput::LogTime(o) => {
                if let Some(t) = &o.issue {
                    collect(t);
                }
            }
            OpInput::UpdateTimeEntry(o) => {
                if let Some(Some(t)) = &o.set.issue {
                    collect(t);
                }
            }
            OpInput::DeleteTimeEntry(_) => {}
            OpInput::TriageDecide(o) => {
                collect(&o.target);
                if let Some(d) = &o.duplicate_of {
                    collect(d);
                }
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
            TargetRef::Version { .. } | TargetRef::TimeEntry { .. } => {
                Err(bad(path, "this op targets an issue"))
            }
        }
    };

    let mut ops = Vec::new();
    let mut op_ids = BTreeSet::new();
    let mut set_fields: BTreeSet<(Target, Key)> = BTreeSet::new();
    let mut transitions: BTreeSet<Target> = BTreeSet::new();
    let mut has_dup = false;
    let mut has_version = false;
    let mut has_time = false;
    let mut has_time_edit = false;
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
                    format: o.format.unwrap_or(TextFormat::Markdown),
                }
            }
            OpInput::UpdateFields(o) => {
                let t = target(&o.target, &p("target"), &binds)?;
                let (set, _) = parse_writes(&o.set, &p("set"), false)?;
                if set.is_empty() {
                    return Err(bad(&p("set"), "set names at least one field"));
                }
                if o.format.is_some() && !set.iter().any(|w| w.key == Key::Description) {
                    return Err(bad(&p("format"), "format goes with a description in set"));
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
                    format: o.format,
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
                    format: o.format.unwrap_or(TextFormat::Markdown),
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
            OpInput::CreateVersion(o) => {
                has_version = true;
                if o.fields.name.is_none() {
                    return Err(
                        ApiError::new(ErrorCode::RequiredFieldMissing, "name is required")
                            .with_details(
                                json!({"op_index": i, "op_id": o.op_id, "fields": ["name"]}),
                            )
                            .into(),
                    );
                }
                if o.fields.status.is_some() {
                    return Err(bad(
                        &p("fields.status"),
                        "a new version is open; set the status with update_version",
                    ));
                }
                Op::CreateVersion {
                    op_id: o.op_id,
                    project_id: o.project_id,
                    fields: o.fields.clone(),
                }
            }
            OpInput::UpdateVersion(o) => {
                has_version = true;
                let TargetRef::Version { id } = o.target else {
                    return Err(bad(&p("target"), "update_version targets a version"));
                };
                if o.set == VersionFields::default() {
                    return Err(bad(&p("set"), "set names at least one field"));
                }
                Op::UpdateVersion {
                    op_id: o.op_id,
                    version_id: id,
                    set: o.set.clone(),
                }
            }
            OpInput::LogTime(o) => {
                has_time = true;
                if let Some(b) = &o.bind
                    && (b.is_empty() || b.len() > 64)
                {
                    return Err(bad(&p("bind"), "bind is 1–64 characters"));
                }
                let issue = match &o.issue {
                    None => None,
                    Some(t) => match target(t, &p("issue"), &BTreeSet::new())? {
                        Target::Issue(id) => Some(id),
                        Target::Temp(_) => unreachable!("no binds were offered"),
                    },
                };
                Op::LogTime {
                    op_id: o.op_id,
                    project_id: o.project_id,
                    issue,
                    fields: o.fields.clone(),
                }
            }
            OpInput::UpdateTimeEntry(o) => {
                has_time_edit = true;
                let TargetRef::TimeEntry { id } = o.target else {
                    return Err(bad(&p("target"), "update_time_entry targets a time entry"));
                };
                let st = &o.set;
                if st.spent_on.is_none()
                    && st.hours.is_none()
                    && st.activity_id.is_none()
                    && st.comments.is_none()
                    && st.user_id.is_none()
                    && st.issue.is_none()
                {
                    return Err(bad(&p("set"), "set names at least one field"));
                }
                let issue = match &o.set.issue {
                    None => None,
                    Some(None) => Some(None),
                    Some(Some(t)) => match target(t, &p("set.issue"), &BTreeSet::new())? {
                        Target::Issue(id) => Some(Some(id)),
                        Target::Temp(_) => unreachable!("no binds were offered"),
                    },
                };
                Op::UpdateTime {
                    op_id: o.op_id,
                    entry_id: id,
                    set: o.set.clone(),
                    issue,
                }
            }
            OpInput::DeleteTimeEntry(o) => {
                has_time_edit = true;
                let TargetRef::TimeEntry { id } = o.target else {
                    return Err(bad(&p("target"), "delete_time_entry targets a time entry"));
                };
                Op::DeleteTime {
                    op_id: o.op_id,
                    entry_id: id,
                }
            }
            OpInput::TriageDecide(o) => {
                let t = target(&o.target, &p("target"), &binds)?;
                if matches!(t, Target::Temp(_)) {
                    return Err(bad(&p("target"), "triage_decide targets an existing issue"));
                }
                let dup = match (o.decision, &o.duplicate_of) {
                    (TriageDecision::Duplicate, Some(d)) => {
                        let d = target(d, &p("duplicate_of"), &binds)?;
                        if matches!(d, Target::Temp(_)) {
                            return Err(bad(
                                &p("duplicate_of"),
                                "duplicate_of names an existing issue",
                            ));
                        }
                        Some(d)
                    }
                    (TriageDecision::Duplicate, None) => {
                        return Err(bad(
                            &p("duplicate_of"),
                            "a duplicate decision names the original issue in duplicate_of",
                        ));
                    }
                    (_, Some(_)) => {
                        return Err(bad(
                            &p("duplicate_of"),
                            "duplicate_of goes only with decision duplicate",
                        ));
                    }
                    (_, None) => None,
                };
                let snooze_until = match (o.decision, &o.snooze_until) {
                    (TriageDecision::Snooze, Some(s)) => Some(
                        chrono::DateTime::parse_from_rfc3339(s)
                            .map_err(|_| {
                                bad(&p("snooze_until"), "snooze_until is an RFC 3339 time")
                            })?
                            .with_timezone(&chrono::Utc),
                    ),
                    (TriageDecision::Snooze, None) => {
                        return Err(bad(
                            &p("snooze_until"),
                            "a snooze decision needs snooze_until",
                        ));
                    }
                    (_, Some(_)) => {
                        return Err(bad(
                            &p("snooze_until"),
                            "snooze_until goes only with decision snooze",
                        ));
                    }
                    (_, None) => None,
                };
                Op::Triage {
                    op_id: o.op_id,
                    target: t,
                    decision: o.decision,
                    dup,
                    snooze_until,
                }
            }
        };
        if !op_ids.insert(parsed.op_id()) {
            return Err(bad(&p("op_id"), "op_id is unique in the document"));
        }
        ops.push(parsed);
    }
    check_triage_doc(&ops)?;
    let time_lock = check_time_doc(&ops, &doc.preconditions, has_time, has_time_edit)?;
    if has_dup && ops.len() != 1 {
        return Err(bad("ops", "duplicate_of is the only op of its document"));
    }
    if has_version && ops.len() != 1 {
        return Err(bad(
            "ops",
            "a version command is the only op of its document",
        ));
    }
    // §17.9a: create_version has no precondition; update_version exactly
    // one, on its version, with lock_version.
    let mut version_lock = None;
    if has_version {
        match (&ops[0], doc.preconditions.as_slice()) {
            (Op::CreateVersion { .. }, []) => {}
            (Op::CreateVersion { .. }, _) => {
                return Err(bad("preconditions", "create_version has no preconditions"));
            }
            (Op::UpdateVersion { version_id, .. }, [one])
                if matches!(one.target, TargetRef::Version { id } if id == *version_id)
                    && one.field_values.is_empty() =>
            {
                version_lock = Some(one.lock_version);
            }
            _ => {
                return Err(bad(
                    "preconditions",
                    "update_version needs exactly one precondition: its version's lock_version",
                ));
            }
        }
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
            Op::Triage { target, dup, .. } => {
                add(target);
                if let Some(d) = dup {
                    add(d);
                }
            }
            Op::CreateVersion { .. } | Op::UpdateVersion { .. } => {}
            Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. } => {}
        }
    }
    let mut pre: BTreeMap<Uuid, Pre> = BTreeMap::new();
    for (i, p) in doc
        .preconditions
        .iter()
        .enumerate()
        .filter(|_| !has_version && !has_time_edit)
    {
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
    if let Some(scope) = seen {
        let ids: Vec<Uuid> = existing.iter().copied().collect();
        let found = crate::time_entries::seen_issue_projects(&mut *conn, &ids, scope).await?;
        if let Some(unseen) = existing.iter().find(|id| !found.contains_key(id)) {
            return Err(issue_not_found(&refs[unseen]));
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
    if let Json::Object(m) = &mut norm {
        m.remove("expected_rules_version");
    }
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
        version_lock,
        time_lock,
        expected_rules: doc.expected_rules_version.clone(),
    })
}

/// Contract 05 §3.10.1 structure: at most one `triage_decide`; the other
/// ops of its document target the same issue and are only those its
/// decision allows; decline and duplicate carry a transition (whether the
/// status is closed is checked at step 14); the duplicate relation comes
/// with the decision, never as a separate `duplicate_of` op.
fn check_triage_doc(ops: &[Op]) -> Result<()> {
    let mut found = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Op::Triage { .. }));
    let Some((
        ti,
        Op::Triage {
            target: tt,
            decision,
            ..
        },
    )) = found.next()
    else {
        return Ok(());
    };
    if let Some((j, _)) = found.next() {
        return Err(bad(
            &format!("ops[{j}]"),
            "at most one triage_decide per document",
        ));
    }
    let mut has_transition = false;
    for (j, o) in ops.iter().enumerate() {
        if j == ti {
            continue;
        }
        let pj = format!("ops[{j}]");
        let on = match (o, decision) {
            (Op::Duplicate { .. }, _) => {
                return Err(bad(
                    &pj,
                    "the duplicate relation comes with triage_decide (decision duplicate); no separate duplicate_of op",
                ));
            }
            (Op::Note { target, .. }, _) => target,
            (Op::Update { target, .. }, TriageDecision::Accept) => target,
            (
                Op::Transition { target, .. },
                TriageDecision::Accept | TriageDecision::Decline | TriageDecision::Duplicate,
            ) => {
                has_transition |= target == tt;
                target
            }
            _ => {
                let allowed = match decision {
                    TriageDecision::Accept => "update_fields, transition and add_note",
                    TriageDecision::Decline | TriageDecision::Duplicate => {
                        "a transition and add_note"
                    }
                    TriageDecision::Snooze | TriageDecision::Unsnooze => "add_note",
                };
                return Err(bad(
                    &pj,
                    format!(
                        "triage_decide {} goes only with {allowed} on the same issue",
                        decision.as_str()
                    ),
                ));
            }
        };
        if on != tt {
            return Err(bad(
                &format!("{pj}.target"),
                "every op of a triage document targets the triaged issue",
            ));
        }
    }
    // A decline may come without a transition when the issue is already
    // closed (a plain transition closed it while it waited); whether it is
    // is a semantic check (`triage_requires_close`). A duplicate always
    // brings its transition.
    if *decision == TriageDecision::Duplicate && !has_transition {
        return Err(bad(
            "ops",
            format!(
                "triage_decide {} needs a transition of the same issue to a closed status",
                decision.as_str()
            ),
        ));
    }
    Ok(())
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
            for f in ["target", "source", "duplicate_of"] {
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
    /// `plain` or `markdown` (design 06 R-M9).
    description_format: String,
    author_id: Uuid,
    created_on_behalf_of_id: Option<Uuid>,
    assigned_to_id: Option<Uuid>,
    priority_id: Uuid,
    fixed_version_id: Option<Uuid>,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
    /// `trim_scale(estimated_hours)::text`.
    estimated_hours: Option<String>,
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
            (
                Key::EstimatedHours,
                json!(
                    self.estimated_hours
                        .as_deref()
                        .and_then(|h| estimated_hours(h).ok())
                ),
            ),
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
    /// The description's format (design 06 R-M9).
    desc_format: TextFormat,
    events: Vec<PendingEvent>,
    notes: Vec<(Uuid, String, bool, TextFormat)>,
    dups: Vec<(Uuid, Uuid, Uuid)>, // (relation id, source, target)
    fields_touched: bool,
    effect: bool,
    /// A new issue that enters the Triage inbox (contract 05 §3.4).
    enters_triage: bool,
    /// The triage decision on this issue (§3.10.1).
    triage: Option<TriagePlan>,
    /// Form rules the actor was held to and the final state meets (step
    /// 14): a write clears only their marks (§3.6.3).
    form_passed: Vec<String>,
}

/// A planned `issue_triage` change.
#[derive(Debug)]
struct TriagePlan {
    decision: TriageDecision,
    /// The row before ([`crate::triage::SNAPSHOT_SQL`]).
    before: Json,
    snooze_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// How a form rule binds the caller ([`Rules::rule_hold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Held to it, and may see it named.
    Bound,
    /// An agent and a field of the rule is restricted: blocked, unnamed.
    Blocked,
    /// Not held to it.
    Free,
}

/// Per (project, tracker): what the actor's roles and the field definitions say.
struct Rules {
    roles: Vec<Uuid>,
    by_status: BTreeMap<Uuid, Vec<FieldRule>>,
    cfs: Vec<CfDef>,
    hidden: Vec<HiddenField>,
    /// The form rules in force (contract 05 §3.6).
    form: crate::forms::Effective,
    /// Custom fields a running type conversion copies from: read-only until
    /// it ends (§3.7).
    converting: BTreeSet<i32>,
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
    /// The caller may read `key` (built-ins always; a custom field that
    /// applies and is readable).
    fn key_readable(&self, key: &str, actor: &Actor, pp: &ProjectPerms) -> bool {
        match Key::parse(key) {
            Some(Key::Cf(id)) => self.cf(id).is_some_and(|d| readable(d, actor, pp)),
            Some(_) => true,
            None => false,
        }
    }
    /// Whether the caller is held to a form rule (§3.6): only when its
    /// target and every condition field are readable. A person is never
    /// held to a rule naming a field they cannot read; an agent is blocked
    /// (without names) when such a field is restricted here. A rule naming
    /// a field that no longer applies (inactive, not for this tracker) holds
    /// no one.
    fn rule_hold(&self, r: &crate::forms::Rule, actor: &Actor, pp: &ProjectPerms) -> Hold {
        let mut hold = Hold::Bound;
        for k in std::iter::once(&r.field).chain(r.when.fields.iter()) {
            if let Some(Key::Cf(id)) = Key::parse(k) {
                match self.cf(id) {
                    None => return Hold::Free,
                    Some(d) if readable(d, actor, pp) => {}
                    Some(_) => {
                        hold = if actor.is_agent() {
                            Hold::Blocked
                        } else {
                            Hold::Free
                        }
                    }
                }
            }
        }
        hold
    }
    /// A state for form conditions.
    fn form_state(
        &self,
        tracker: Uuid,
        status: Uuid,
        is_private: bool,
        values: &BTreeMap<Key, Json>,
    ) -> crate::forms::State {
        crate::forms::State::of(
            tracker,
            status,
            self.form.category(status),
            is_private,
            values.iter().map(|(k, v)| (k.name(), v)),
        )
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
    let parsed = match parse_for(&mut *tx, doc, MAX_OPS as usize, caller).await {
        Ok(p) => p,
        Err(e) => {
            let fp = |p: &Parsed| fingerprint(ACTION_COMMAND, &p.hash);
            match resent(
                &mut *tx,
                doc,
                MAX_OPS as usize,
                who.user_id,
                operation_id,
                fp,
            )
            .await?
            {
                Some(p) => p,
                None => return Err(e),
            }
        }
    };
    if parsed.version_cmd().is_some() {
        return Err(ApiError::new(
            ErrorCode::UnsupportedOperation,
            "people write versions through /projects/{id}/versions and /versions/{id}",
        )
        .with_details(json!({"path": "ops[0].kind"}))
        .into());
    }
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
        return replay(
            &mut *tx,
            caller,
            who.user_id,
            operation_id,
            ACTION_COMMAND,
            &fingerprint,
        )
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
        "time_entries": result.time_entries,
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

/// An undo (§16.7): a `human_direct` execution that names what it reverts.
pub(crate) async fn record_undo_execution(
    conn: &mut PgConnection,
    request_id: Uuid,
    user_id: Uuid,
    hash: &[u8],
    consistent_after: &str,
    reverts: Uuid,
) -> Result<Uuid> {
    let execution_id = Uuid::now_v7();
    sqlx::query("INSERT INTO execution_refs (id, kind) VALUES ($1, 'command')")
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO command_executions (id, operation_request_id, actor_id, origin, command_hash,
                                         consistent_after, reverts_execution_id)
         VALUES ($1, $2, $3, 'human_direct', $4, $5, $6)",
    )
    .bind(execution_id)
    .bind(request_id)
    .bind(user_id)
    .bind(hash)
    .bind(consistent_after)
    .bind(reverts)
    .execute(&mut *conn)
    .await?;
    Ok(execution_id)
}

/// Runs an undo document as `who` (steps 5–17, Mode::Direct): a
/// `human_direct` execution with `reverts_execution_id`, keeping before and
/// after values so the undo itself can be undone (§16.7).
pub(crate) async fn evaluate_undo(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &Executor,
    doc: &Parsed,
    request_id: Uuid,
    reverts: Uuid,
) -> Result<CommandResult> {
    let deadline = lock_executor(&mut *conn, who).await?;
    let ectx = EvalCtx {
        mode: Mode::Direct,
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
    let execution_id = record_undo_execution(
        &mut *conn,
        request_id,
        who.user_id,
        &doc.hash[..],
        &consistent_after,
        reverts,
    )
    .await?;
    let mut at = Attribution::human(who.user_id, request_id, execution_id);
    at.record_changes = true;
    let effects = write_effects(&mut *conn, doc, &sim, &at).await?;
    Ok(CommandResult {
        status: "succeeded".into(),
        execution_id,
        effects,
        items: sim.items,
        search: SearchReceipt { consistent_after },
        time_entries: sim.time.as_ref().map(|t| t.effects()).unwrap_or_default(),
    })
}

/// A document checked as `caller` without writing (Mode::Validate).
pub(crate) async fn check_as(
    conn: &mut PgConnection,
    caller: AuthContext,
    doc: &Json,
) -> Result<()> {
    let parsed = parse_for(&mut *conn, doc, MAX_OPS as usize, caller).await?;
    let ectx = EvalCtx {
        mode: Mode::Validate,
        caller,
        ceiling: None,
        deadlines: Vec::new(),
        author: None,
    };
    simulate(&mut *conn, &ectx, &parsed).await.map(|_| ())
}

/// An issue's current lock_version, status and field values by key name
/// (the encoding of `execution_field_changes`).
pub(crate) async fn current_values(
    conn: &mut PgConnection,
    issue: Uuid,
) -> Result<Option<(i32, Uuid, BTreeMap<String, Json>)>> {
    let r: Option<IssueRow> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.status_id, i.subject, i.description,
                i.description_format, i.author_id, i.created_on_behalf_of_id, i.assigned_to_id, i.priority_id,
                i.fixed_version_id, i.start_date, i.due_date,
                trim_scale(i.estimated_hours)::text AS estimated_hours, i.is_private, i.lock_version,
                i.last_event_seq
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = $1 AND i.archived_at IS NULL",
    )
    .bind(issue)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = r else { return Ok(None) };
    let mut m: BTreeMap<String, Json> = r
        .builtin()
        .into_iter()
        .map(|(k, v)| (k.name(), v))
        .collect();
    for (k, v) in load_cf_values(&mut *conn, r.id).await? {
        m.insert(k.name(), v);
    }
    m.insert("status_id".into(), json!(r.status_id));
    Ok(Some((r.lock_version, r.status_id, m)))
}

/// An issue's tracker, status, privacy and field values by key name (the
/// state form conditions see, contract 05 §3.6). Archived issues too.
pub(crate) async fn issue_state(
    conn: &mut PgConnection,
    issue: Uuid,
) -> Result<Option<(Uuid, Uuid, bool, BTreeMap<String, Json>)>> {
    let r: Option<IssueRow> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.status_id, i.subject, i.description,
                i.description_format, i.author_id, i.created_on_behalf_of_id, i.assigned_to_id, i.priority_id,
                i.fixed_version_id, i.start_date, i.due_date,
                trim_scale(i.estimated_hours)::text AS estimated_hours, i.is_private, i.lock_version,
                i.last_event_seq
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = $1",
    )
    .bind(issue)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = r else { return Ok(None) };
    let mut m: BTreeMap<String, Json> = r
        .builtin()
        .into_iter()
        .map(|(k, v)| (k.name(), v))
        .collect();
    for (k, v) in load_cf_values(&mut *conn, r.id).await? {
        m.insert(k.name(), v);
    }
    Ok(Some((r.tracker_id, r.status_id, r.is_private, m)))
}

/// The key was used before (§8.7). Same content → the stored result, with
/// visibility re-checked; different content → `idempotency_key_reused`.
pub(crate) async fn replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    caller_id: Uuid,
    operation_id: Uuid,
    action: &str,
    fingerprint: &[u8; 32],
) -> Result<CommandResult> {
    let r = stored_result(&mut *conn, caller_id, operation_id, action, fingerprint).await?;
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
        .filter(|i| {
            i.issue_id.is_some_and(|id| visible.contains_key(&id)) || i.time_entry_id.is_some()
        })
        .collect();
    // A replay repeats the time receipts as written (ids and lock versions
    // only; the entries' contents are read through GET /time-entries).
    let time_entries = serde_json::from_value(r["time_entries"].clone()).unwrap_or_default();
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
        time_entries,
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
    pub(crate) fn human(user: Uuid, request_id: Uuid, execution_id: Uuid) -> Self {
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
    /// A version document's planned write (§17.9a).
    pub version: Option<crate::versions::Plan>,
    /// The time ops' planned writes (contract 05 §3.10.2).
    pub time: Option<crate::time_entries::TimePlan>,
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
                | ErrorCode::FormRulesChanged
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
        time_entries: sim.time.as_ref().map(|t| t.effects()).unwrap_or_default(),
    })
}

/// Steps 6–14. Direct and Apply lock (and read the receipt token, and check
/// the deadlines); Propose and Validate only read, and check preconditions
/// after the ops so a permission problem is reported before a stale one.
pub(crate) async fn simulate(conn: &mut PgConnection, ectx: &EvalCtx, doc: &Parsed) -> Result<Sim> {
    if let Some(cmd) = doc.version_cmd() {
        let (plan, token) = crate::versions::plan(&mut *conn, ectx, cmd).await?;
        return Ok(Sim {
            work: BTreeMap::new(),
            items: vec![plan.item()],
            token,
            version: Some(plan),
            time: None,
        });
    }
    if doc.time_only() {
        let (plan, token) =
            crate::time_entries::plan(&mut *conn, ectx, &doc.time_cmds(), None).await?;
        return Ok(Sim {
            work: BTreeMap::new(),
            items: plan.items(),
            token,
            version: None,
            time: Some(plan),
        });
    }
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
            Op::CreateVersion { .. } | Op::UpdateVersion { .. } => {}
            Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. } => {}
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
            Op::Triage { target, dup, .. } => {
                add(target);
                if let Some(d) = dup {
                    add(d);
                }
            }
        }
    }
    let existing: Vec<Uuid> = existing.into_iter().collect();
    let issue_projects: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, project_id, tracker_id FROM issues WHERE id = ANY($1) AND archived_at IS NULL",
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
    // Contract 05 §3.6: the (project, tracker) pairs whose forms apply. A
    // tracker never changes, so reading it before the locks is safe.
    let mut form_pairs: BTreeSet<(Uuid, Uuid)> =
        issue_projects.iter().map(|r| (r.1, r.2)).collect();
    for op in &doc.ops {
        if let Op::Create {
            project_id,
            tracker_id,
            ..
        } = op
        {
            form_pairs.insert((*project_id, *tracker_id));
        }
    }

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
    // Step 6b (contract 05 §3.6): the forms of those pairs FOR SHARE, so a
    // publish (FOR UPDATE) lands wholly before or after this write.
    if locks {
        crate::forms::lock_forms(&mut *conn, &form_pairs).await?;
    }

    // Step 7: issues FOR UPDATE in id order.
    let rows: Vec<IssueRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.status_id, i.subject, i.description,
                i.description_format, i.author_id, i.created_on_behalf_of_id, i.assigned_to_id, i.priority_id,
                i.fixed_version_id, i.start_date, i.due_date,
                trim_scale(i.estimated_hours)::text AS estimated_hours, i.is_private, i.lock_version,
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

    // §3.5.5: a mixed document's daily totals come right after the issues
    // (before the field options and the search token below).
    if locks && doc.has_time() {
        crate::time_entries::prelock_totals(&mut *conn, ectx, &doc.time_cmds()).await?;
    }

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
    // Contract 05 §3.6: the rules the caller saw are still the rules.
    if let Some(expected) = &doc.expected_rules
        && let Some(current) = crate::forms::pairs_version(&mut *conn, &form_pairs).await?
        && *expected != current
    {
        return Err(ApiError::new(
            ErrorCode::FormRulesChanged,
            "the form rules changed since you read them; describe the form again",
        )
        .with_details(json!({"expected": expected, "current": current}))
        .into());
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
                desc_format: TextFormat::from_db(&r.description_format),
                events: Vec::new(),
                notes: Vec::new(),
                dups: Vec::new(),
                fields_touched: false,
                effect: false,
                enters_triage: false,
                triage: None,
                form_passed: Vec::new(),
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
                Op::CreateVersion { .. } => perm::OP_CREATE_VERSION,
                Op::UpdateVersion { .. } => perm::OP_UPDATE_VERSION,
                Op::Update { .. } => perm::OP_UPDATE_FIELDS,
                Op::Transition { .. } => perm::OP_TRANSITION,
                Op::Note { .. } => perm::OP_ADD_NOTE,
                Op::Duplicate { .. } => perm::OP_DUPLICATE_OF,
                Op::Create { .. } => perm::OP_CREATE_ISSUE,
                Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. } => {
                    unreachable!("time ops are checked by time_entries::plan")
                }
                // The relation of a duplicate decision (checked as
                // duplicate_of on both ends).
                Op::Triage { .. } => perm::OP_DUPLICATE_OF,
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
            Op::CreateVersion { .. } | Op::UpdateVersion { .. } => {}
            // Planned by time_entries::plan after the issue ops.
            Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. } => {}
            Op::Update { target, .. } | Op::Transition { target, .. } => check(target, "edit")?,
            Op::Note { target, .. } => check(target, "note")?,
            Op::Duplicate { source, target, .. } => {
                check(source, "edit")?;
                check(target, "edit")?;
            }
            Op::Triage {
                target,
                decision,
                dup,
                ..
            } => {
                // §3.10.1: perm(triage_issues), project level, plus the
                // delegation's triage_decide.
                let Target::Issue(id) = target else {
                    unreachable!("parse rejects temp targets")
                };
                let w = &work[id];
                let pp = &perms[&w.project_id];
                if !(pp.writable() && pp.allowed_any(perm::TRIAGE_ISSUES))
                    || !actor.delegation_allows(w.project_id, perm::OP_TRIAGE_DECIDE, None)
                {
                    let reason = if pp.writable() {
                        "permission"
                    } else {
                        "project_not_writable"
                    };
                    return Err(op_err(
                        ErrorCode::ForbiddenOp,
                        i,
                        op.op_id(),
                        "you may not triage this issue",
                        json!({"reason": reason}),
                    ));
                }
                if *decision == TriageDecision::Duplicate {
                    check(target, "edit")?;
                    if let Some(d) = dup {
                        check(d, "edit")?;
                    }
                }
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
    // Contract 05 §3.10.2: log_time next to issue ops on the same issue,
    // after them (T5 compares the status before and after the document).
    let mut time = None;
    let mut items = items;
    if doc.has_time() {
        let cmds = doc.time_cmds();
        let issue = cmds.iter().find_map(|c| match c {
            crate::time_entries::TimeCmd::Log { issue, .. } => *issue,
            _ => None,
        });
        let ctx = match issue.and_then(|i| work.get(&i)) {
            Some(w) => {
                let before = w.orig.as_ref().map_or(w.status, |o| o.status_id);
                let (closed_before, closed_after): (bool, bool) = sqlx::query_as(
                    "SELECT COALESCE((SELECT category = 'done' FROM issue_statuses WHERE id = $1), false),
                            COALESCE((SELECT category = 'done' FROM issue_statuses WHERE id = $2), false)",
                )
                .bind(before)
                .bind(w.status)
                .fetch_one(&mut *conn)
                .await?;
                Some(crate::time_entries::IssueCtx {
                    id: w.id,
                    closed_before,
                    closed_after,
                })
            }
            None => None,
        };
        let (plan, _) = crate::time_entries::plan(&mut *conn, ectx, &cmds, ctx).await?;
        let mut by_op: BTreeMap<Uuid, ItemResult> = items
            .into_iter()
            .chain(plan.items())
            .map(|it| (it.op_id, it))
            .collect();
        items = doc
            .ops
            .iter()
            .filter_map(|o| by_op.remove(&o.op_id()))
            .collect();
        time = Some(plan);
    }
    Ok(Sim {
        work,
        items,
        token,
        version: None,
        time,
    })
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
            Op::CreateVersion { .. } | Op::UpdateVersion { .. } => {
                unreachable!("version documents are planned apart")
            }
            Op::LogTime { .. } | Op::UpdateTime { .. } | Op::DeleteTime { .. } => {}
            Op::Create {
                op_id,
                bind,
                project_id,
                tracker_id,
                fields,
                is_private,
                format,
            } => {
                if actor.is_agent() && *format != TextFormat::Markdown {
                    return Err(agent_format(fail));
                }
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
                    (Key::EstimatedHours, Json::Null),
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
                    desc_format: *format,
                    events: Vec::new(),
                    notes: Vec::new(),
                    dups: Vec::new(),
                    fields_touched: true,
                    effect: true,
                    enters_triage: false,
                    triage: None,
                    form_passed: Vec::new(),
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
                // Contract 05 §3.4: the project's Triage is on, and the
                // creator is an agent, or every new issue enters, or the
                // creator may not triage.
                let settings: Option<(bool, String)> = sqlx::query_as(
                    "SELECT enabled, entry_rule FROM project_triage_settings WHERE project_id = $1",
                )
                .bind(project_id)
                .fetch_optional(&mut *conn)
                .await?;
                w.enters_triage = settings.is_some_and(|(enabled, rule)| {
                    enabled
                        && (on_behalf.is_some()
                            || rule == "all_new"
                            || !pp.allowed_any(perm::TRIAGE_ISSUES))
                });
                let mut payload: Map<String, Json> = w
                    .values
                    .iter()
                    .filter(|(_, v)| !is_empty(v))
                    .map(|(k, v)| (k.name(), v.clone()))
                    .collect();
                payload.insert("status_id".into(), json!(initial));
                payload.insert("is_private".into(), json!(private));
                let fields_list: Vec<String> = payload.keys().cloned().collect();
                let mut created = json!({"fields": payload, "description_format": format.as_str()});
                if w.enters_triage {
                    created["triage"] = json!("pending");
                }
                w.events.push(PendingEvent {
                    kind: "created",
                    op_id: *op_id,
                    payload: created,
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
                    issue_id: Some(id),
                    version_id: None,
                    time_entry_id: None,
                });
            }
            Op::Update {
                op_id,
                target,
                set,
                format,
            } => {
                let id = resolve(target);
                let (project_id, tracker_id, status, cur_format) = {
                    let w = &work[&id];
                    (w.project_id, w.tracker_id, w.status, w.desc_format)
                };
                // R-M9: agents write Markdown only, and say so when they
                // rewrite a plain description (no silent conversion).
                if actor.is_agent()
                    && set.iter().any(|w| w.key == Key::Description)
                    && format.unwrap_or(cur_format) != TextFormat::Markdown
                {
                    return Err(agent_format(fail));
                }
                let new_format = format.unwrap_or(cur_format);
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
                let format_changed = new_format != cur_format;
                w.desc_format = new_format;
                let applied = !changes.is_empty() || format_changed;
                if applied {
                    w.fields_touched = true;
                    w.effect = true;
                    let mut fields: Vec<String> = changes.keys().cloned().collect();
                    let mut payload = json!({"changes": changes});
                    if format_changed {
                        // The format belongs to the description: its origin
                        // moves with it (R-M3 judges agent content by it).
                        payload["description_format"] =
                            json!({"old": cur_format.as_str(), "new": new_format.as_str()});
                        let d = Key::Description.name();
                        if !fields.contains(&d) {
                            fields.push(d);
                        }
                    }
                    w.events.push(PendingEvent {
                        kind: "fields_changed",
                        op_id: *op_id,
                        payload,
                        note_id: None,
                        fields,
                    });
                }
                items.push(ItemResult {
                    op_id: *op_id,
                    status: if applied { "applied" } else { "no_effect" }.into(),
                    issue_id: Some(id),
                    version_id: None,
                    time_entry_id: None,
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
                    issue_id: Some(id),
                    version_id: None,
                    time_entry_id: None,
                });
            }
            Op::Note {
                op_id,
                target,
                body,
                private,
                format,
            } => {
                if actor.is_agent() && *format != TextFormat::Markdown {
                    return Err(agent_format(fail));
                }
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
                w.notes.push((note_id, body.clone(), *private, *format));
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
                    issue_id: Some(id),
                    version_id: None,
                    time_entry_id: None,
                });
            }
            Op::Duplicate {
                op_id,
                source,
                target,
            } => {
                let (s, t) = (resolve(source), resolve(target));
                let exists = add_duplicate(&mut *conn, work, perms, s, t, *op_id)
                    .await?
                    .map_err(|(code, msg, extra)| fail(code, msg.into(), extra))?;
                items.push(ItemResult {
                    op_id: *op_id,
                    status: if exists { "no_effect" } else { "applied" }.into(),
                    issue_id: Some(s),
                    version_id: None,
                    time_entry_id: None,
                });
            }
            Op::Triage {
                op_id,
                target,
                decision,
                dup,
                snooze_until,
            } => {
                let id = resolve(target);
                let row: Option<(String, bool, Json, chrono::DateTime<chrono::Utc>)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(format!(
                        "SELECT t.state,
                                COALESCE(t.snoozed_until > now() AND i.updated_at <= t.snoozed_at, false),
                                {}, now()
                         FROM issue_triage t JOIN issues i ON i.id = t.issue_id
                         WHERE t.issue_id = $1",
                        crate::triage::SNAPSHOT_SQL
                    )))
                    .bind(id)
                    .fetch_optional(&mut *conn)
                    .await?;
                let Some((state, snoozed, before, now)) = row else {
                    return Err(fail(
                        ErrorCode::TriageStateInvalid,
                        "this issue is not waiting in Triage".into(),
                        json!({"state": "none"}),
                    ));
                };
                if state != "pending" {
                    return Err(fail(
                        ErrorCode::TriageStateInvalid,
                        format!("this issue was already triaged ({state})"),
                        json!({"state": state}),
                    ));
                }
                if *decision == TriageDecision::Unsnooze && !snoozed {
                    return Err(fail(
                        ErrorCode::TriageStateInvalid,
                        "this issue is not snoozed".into(),
                        json!({"state": state, "snoozed": false}),
                    ));
                }
                if let Some(until) = snooze_until
                    // One minute of slack: a client's "now + 1 hour" passes.
                    && (*until < now + chrono::Duration::minutes(59)
                        || *until > now + chrono::Duration::days(90))
                {
                    return Err(fail(
                        ErrorCode::InvalidValue,
                        "snooze_until is 1 hour to 90 days from now".into(),
                        json!({"path": format!("ops[{i}].snooze_until"), "kind": "snooze_range"}),
                    ));
                }
                if let (TriageDecision::Duplicate, Some(d)) = (decision, dup) {
                    add_duplicate(&mut *conn, work, perms, id, resolve(d), *op_id)
                        .await?
                        .map_err(|(code, msg, extra)| fail(code, msg.into(), extra))?;
                }
                let w = work.get_mut(&id).expect("target");
                let mut payload = json!({"old": state, "new": decision.new_state()});
                match decision {
                    TriageDecision::Snooze => payload["snoozed_until"] = json!(snooze_until),
                    TriageDecision::Unsnooze => payload["snoozed_until"] = Json::Null,
                    _ => {}
                }
                w.events.push(PendingEvent {
                    kind: "triage_changed",
                    op_id: *op_id,
                    payload,
                    note_id: None,
                    fields: vec![crate::triage::FIELD_KEY.into()],
                });
                w.triage = Some(TriagePlan {
                    decision: *decision,
                    before,
                    snooze_until: *snooze_until,
                });
                w.effect = true;
                if *decision == TriageDecision::Accept {
                    // C3/R7: an accepted issue gets the final-state checks.
                    w.fields_touched = true;
                }
                items.push(ItemResult {
                    op_id: *op_id,
                    status: "applied".into(),
                    issue_id: Some(id),
                    version_id: None,
                    time_entry_id: None,
                });
            }
        }
    }

    // §3.10.1: decline and duplicate end in a closed status (R27: a
    // semantic check, invalid_value).
    for w in work.values() {
        if let Some(TriagePlan {
            decision: TriageDecision::Decline | TriageDecision::Duplicate,
            ..
        }) = &w.triage
        {
            let done: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM issue_statuses WHERE id = $1 AND category = 'done')",
            )
            .bind(w.status)
            .fetch_one(&mut *conn)
            .await?;
            if !done {
                return Err(ApiError::new(
                    ErrorCode::InvalidValue,
                    "declining or marking a duplicate needs the issue to end in a closed status (send a transition to one)",
                )
                .with_details(
                    json!({"issue_id": w.id, "kind": "triage_requires_close", "path": "ops"}),
                )
                .into());
            }
        }
    }

    // Step 14: final state — date order (R4) and required fields (§6.3).
    let mut form_passed: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
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
        // Contract 05 §3.6: the L1 form rules on the final state.
        if r.form.rules.is_empty() {
            continue;
        }
        let names: BTreeMap<String, Json> = w
            .values
            .iter()
            .map(|(k, v)| (k.name(), v.clone()))
            .collect();
        let fin = r.form_state(w.tracker_id, w.status, w.is_private, &w.values);
        let mut fields: Vec<String> = Vec::new();
        let mut rule_ids: Vec<String> = Vec::new();
        let mut blocked = false;
        for rule in crate::forms::missing(&r.form, &fin, &names) {
            match r.rule_hold(rule, actor, pp) {
                Hold::Free => {}
                Hold::Blocked => blocked = true,
                Hold::Bound => {
                    if !fields.contains(&rule.field) {
                        fields.push(rule.field.clone());
                    }
                    rule_ids.push(rule.id.clone());
                }
            }
        }
        if !fields.is_empty() || blocked {
            let mut d = json!({"issue_id": w.id, "fields": fields, "reason": "form_rule"});
            if let Some(first) = rule_ids.first() {
                d["rule_id"] = json!(first);
                d["rule_ids"] = json!(rule_ids);
            }
            if blocked {
                d["blocked"] = json!(true);
            }
            return Err(ApiError::new(
                ErrorCode::RequiredFieldMissing,
                "the form rules require fields that are empty",
            )
            .with_details(d)
            .into());
        }
        let orig_names: Option<BTreeMap<String, Json>> = w.orig.as_ref().map(|_| {
            w.orig_values
                .iter()
                .map(|(k, v)| (k.name(), v.clone()))
                .collect()
        });
        let mut agent_blocked: Option<&crate::forms::Rule> = None;
        for rule in crate::forms::disallowed(&r.form, &fin, &names, orig_names.as_ref()) {
            match r.rule_hold(rule, actor, pp) {
                Hold::Free => {}
                Hold::Blocked => {
                    // The agent wrote the value: name the field when it can
                    // read it, never the rule.
                    agent_blocked.get_or_insert(rule);
                }
                Hold::Bound => {
                    return Err(ApiError::new(
                        ErrorCode::InvalidValue,
                        format!("the form rules do not allow this value of {}", rule.field),
                    )
                    .with_details(json!({"issue_id": w.id, "field": rule.field,
                                         "kind": "rule_disallowed", "rule_id": rule.id,
                                         "allowed": rule.values}))
                    .into());
                }
            }
        }
        if let Some(rule) = agent_blocked {
            let mut d = json!({"issue_id": w.id, "kind": "rule_disallowed", "blocked": true});
            if r.key_readable(&rule.field, actor, pp) {
                d["field"] = json!(rule.field);
            }
            return Err(ApiError::new(
                ErrorCode::InvalidValue,
                "the form rules do not allow a value here",
            )
            .with_details(d)
            .into());
        }
        // The rules this write was held to and meets: only their marks clear.
        let broken = crate::forms::violations(&r.form.rules, &fin, &names);
        let passed: Vec<String> = r
            .form
            .rules
            .iter()
            .filter(|x| {
                x.kind != crate::forms::RuleKind::ReadonlyIf
                    && !broken.contains(&x.id)
                    && r.rule_hold(x, actor, pp) == Hold::Bound
            })
            .map(|x| x.id.clone())
            .collect();
        form_passed.insert(w.id, passed);
    }
    for (id, p) in form_passed {
        if let Some(w) = work.get_mut(&id) {
            w.form_passed = p;
        }
    }
    Ok(items)
}

type OpFailure = (ErrorCode, &'static str, Json);

/// The duplicate relation `s` → `t` (02 §6.2: not itself, same project,
/// `manage_issue_relations`), planned on both issues. `Ok(Ok(true))` when
/// it exists already (no effect).
async fn add_duplicate(
    conn: &mut PgConnection,
    work: &mut BTreeMap<Uuid, Work>,
    perms: &BTreeMap<Uuid, ProjectPerms>,
    s: Uuid,
    t: Uuid,
    op_id: Uuid,
) -> Result<std::result::Result<bool, OpFailure>> {
    if s == t {
        return Ok(Err((
            ErrorCode::RelationNotAllowed,
            "an issue cannot duplicate itself",
            json!({"reason": "self"}),
        )));
    }
    let (ws, wt) = (&work[&s], &work[&t]);
    if ws.project_id != wt.project_id {
        return Ok(Err((
            ErrorCode::RelationNotAllowed,
            "duplicates must be in the same project",
            json!({"reason": "cross_project"}),
        )));
    }
    let pp = &perms[&ws.project_id];
    if !pp.allowed(perm::MANAGE_ISSUE_RELATIONS, ws.tracker_id) {
        return Ok(Err((
            ErrorCode::ForbiddenOp,
            "you may not manage relations of this issue",
            json!({"reason": "permission"}),
        )));
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
            op_id,
            payload: json!({"role": "source", "other_issue_id": t}),
            note_id: None,
            fields: Vec::new(),
        });
        let tgt = work.get_mut(&t).expect("target");
        tgt.effect = true;
        tgt.events.push(PendingEvent {
            kind: "duplicate_added",
            op_id,
            payload: json!({"role": "target", "other_issue_id": s}),
            note_id: None,
            fields: Vec::new(),
        });
    }
    Ok(Ok(exists))
}

/// Steps 15–17: issues, custom fields, notes, relations, events, field
/// origins and one `lock_version` bump per issue with an effect.
pub(crate) async fn write_effects(
    conn: &mut PgConnection,
    doc: &Parsed,
    sim: &Sim,
    at: &Attribution,
) -> Result<Vec<Effect>> {
    if let Some(plan) = &sim.version {
        crate::versions::write_plan(&mut *conn, plan, at).await?;
        return Ok(Vec::new());
    }
    if let Some(plan) = &sim.time {
        crate::time_entries::write_plan(&mut *conn, plan, at).await?;
        if doc.time_only() {
            return Ok(Vec::new());
        }
    }
    let (work, items) = (&sim.work, &sim.items);
    let execution_id = at.execution_ref;

    let mut effects = Vec::new();
    // New issues in op order (keys are numbered in document order).
    let created: Vec<Uuid> = doc
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Op::Create { .. }))
        .map(|(i, _)| items[i].issue_id.expect("a create item names its issue"))
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
                // Contract 05 §3.6.3: a write that passed the form checks
                // clears the issue's accept_and_mark marks.
                if w.fields_touched && !w.form_passed.is_empty() {
                    crate::forms::clear_marks(&mut *conn, w.id, &w.form_passed).await?;
                }
            }
        } else {
            key = insert_issue(&mut *conn, w).await?;
            lock_version = 0;
        }
        if w.effect {
            for (note_id, body, private, format) in &w.notes {
                sqlx::query(
                    "INSERT INTO issue_notes (id, issue_id, author_id, created_on_behalf_of_id, body, is_private,
                                              body_format)
                     VALUES ($1,$2,$3,$4,$5,$6,$7)",
                )
                .bind(note_id)
                .bind(id)
                .bind(at.author_id)
                .bind(at.author_on_behalf)
                .bind(body)
                .bind(private)
                .bind(format.as_str())
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
            // §3.10.1: the triage row first, so the event carries the A5 flags.
            let triage_after = match &w.triage {
                Some(plan) => Some(write_triage(&mut *conn, id, plan, at).await?),
                None => None,
            };
            let base = w.orig.as_ref().map(|o| o.last_event_seq).unwrap_or(0);
            for (n, ev) in w.events.iter().enumerate() {
                let payload = match (&triage_after, ev.kind) {
                    (Some(after), "triage_changed") => {
                        let mut p = ev.payload.clone();
                        p["self_triage"] = after["self_triage"].clone();
                        p["same_sponsor"] = after["same_sponsor"].clone();
                        p
                    }
                    _ => ev.payload.clone(),
                };
                // The issue_field_changes rows (design 05 §2.2) in the same
                // statement, through the mapping the 0020 backfill uses.
                let event_id: Uuid = sqlx::query_scalar(
                    "WITH ev AS (
                       INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                                 proposer_id, approver_id, on_behalf_of_id, delegation_id,
                                                 agent_client_id, operation_request_id, command_execution_id,
                                                 changeset_id, changeset_version, execution_id, op_id,
                                                 note_id, payload)
                       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
                       RETURNING id, issue_id, kind, payload, at
                     ), projected AS (
                       INSERT INTO issue_field_changes (event_id, issue_id, field_key, old_ref, new_ref, at)
                       SELECT ev.id, ev.issue_id, r.field_key, r.old_ref, r.new_ref, ev.at
                       FROM ev, issue_field_change_rows(ev.kind, ev.payload) r
                     )
                     SELECT id FROM ev",
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
                .bind(&payload)
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
                // Design 06 R-MN3, R-MN4: who a person's text mentions (an
                // agent's mentions render but are never recorded).
                if at.actor_kind != "agent"
                    && let Some((new, old)) = event_texts(w, ev)
                {
                    let new = crate::mentions::candidates(&new.0, new.1);
                    if !new.is_empty() {
                        let ids = crate::mentions::resolve(&mut *conn, w.project_id, &new).await?;
                        // A rewritten description: only who was not mentioned before.
                        let before = match old {
                            Some((t, f)) => {
                                let c = crate::mentions::candidates(&t, f);
                                crate::mentions::resolve(&mut *conn, w.project_id, &c).await?
                            }
                            None => Vec::new(),
                        };
                        crate::mentions::record(&mut *conn, event_id, at.author_id, &ids, &before)
                            .await?;
                    }
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
                // R-M9: undo restores the description's format with its text
                // (undo.rs reads this row with the description, never alone).
                if o.description_format != w.desc_format.as_str() {
                    changed.push((
                        DESCRIPTION_FORMAT_KEY.into(),
                        json!(o.description_format),
                        json!(w.desc_format.as_str()),
                    ));
                }
                if let (Some(plan), Some(after)) = (&w.triage, &triage_after) {
                    changed.push((
                        crate::triage::FIELD_KEY.into(),
                        plan.before.clone(),
                        after.clone(),
                    ));
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

type Text = (String, TextFormat);

/// The text an event writes that may mention people (design 06 R-MN3): a
/// note's body, a new issue's description, or a rewritten description with
/// the text it replaced.
fn event_texts(w: &Work, ev: &PendingEvent) -> Option<(Text, Option<Text>)> {
    let fmt = |v: &Json| v.as_str().map(TextFormat::from_db);
    match ev.kind {
        "note_added" => {
            let (_, body, _, f) = w.notes.iter().find(|n| Some(n.0) == ev.note_id)?;
            Some(((body.clone(), *f), None))
        }
        "created" => {
            let d = ev.payload["fields"]["description"].as_str()?;
            let f = fmt(&ev.payload["description_format"]).unwrap_or(w.desc_format);
            Some(((d.to_string(), f), None))
        }
        "fields_changed" => {
            let c = &ev.payload["changes"]["description"];
            let new = c["new"].as_str()?;
            let old = c["old"].as_str().unwrap_or_default();
            let f = &ev.payload["description_format"];
            let new_f = fmt(&f["new"]).unwrap_or(w.desc_format);
            let old_f = fmt(&f["old"]).unwrap_or(new_f);
            Some(((new.to_string(), new_f), Some((old.to_string(), old_f))))
        }
        _ => None,
    }
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
            form: crate::forms::effective(&mut *conn, project, tracker).await?,
            converting: sqlx::query_scalar::<_, i32>(
                "SELECT from_field_id FROM custom_field_conversions WHERE state = 'running'",
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect(),
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
    if let Key::Cf(id) = wr.key
        && rules.converting.contains(&id)
        && orig.and_then(|o| o.get(&wr.key)) != Some(&wr.value)
        && !(wr.value.is_null() && orig.is_some_and(|o| !o.contains_key(&wr.key)))
    {
        return Err(ApiError::new(
            ErrorCode::InvalidValue,
            format!("{name} is being converted into a new field; it is read-only until that ends"),
        )
        .with_details(detail(json!({"kind": "field_converting"})))
        .into());
    }
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
    // Contract 05 §3.6: readonly_if, judged on the issue as it was at the
    // start of the transaction (an issue created in this document has none).
    if let Some(o) = &w.orig {
        let base = rules.form_state(o.tracker_id, o.status_id, o.is_private, &w.orig_values);
        let mut blocked = false;
        for r in crate::forms::readonly_rules(&rules.form, &name, &base) {
            match rules.rule_hold(r, actor, pp) {
                Hold::Free => {}
                Hold::Blocked => blocked = true,
                Hold::Bound => {
                    return Err(ApiError::new(
                        ErrorCode::ReadonlyField,
                        format!("{name} is read-only here under the form rules"),
                    )
                    .with_details(detail(json!({"reason": "form_rule", "rule_id": r.id})))
                    .into());
                }
            }
        }
        if blocked {
            return Err(ApiError::new(
                ErrorCode::ReadonlyField,
                format!("{name} is read-only here under the form rules"),
            )
            .with_details(detail(json!({"reason": "form_rule", "blocked": true})))
            .into());
        }
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

/// Writes a decision to `issue_triage` and returns the new row
/// ([`crate::triage::SNAPSHOT_SQL`]). A5 (§2.4): `self_triage` when an
/// agent decides an issue it created; `same_sponsor` when an agent decides
/// another agent's issue and both act for the same person. Only decisions
/// (not snooze) set them.
async fn write_triage(
    conn: &mut PgConnection,
    issue: Uuid,
    plan: &TriagePlan,
    at: &Attribution,
) -> Result<Json> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE issue_triage t SET
             state = $2::text,
             snoozed_until = $3::timestamptz,
             snoozed_at = CASE WHEN $3::timestamptz IS NULL THEN NULL ELSE now() END,
             snoozed_by = CASE WHEN $3::timestamptz IS NULL THEN NULL ELSE $4::uuid END,
             decided_at = CASE WHEN $2::text = 'pending' THEN NULL ELSE now() END,
             decided_by = CASE WHEN $2::text = 'pending' THEN NULL ELSE $4::uuid END,
             self_triage = ($2::text <> 'pending' AND $5::text = 'agent' AND i.author_id = $4::uuid),
             same_sponsor = ($2::text <> 'pending' AND $5::text = 'agent' AND i.author_id <> $4::uuid
                             AND EXISTS (SELECT 1 FROM agent_principals a
                                         JOIN agent_principals b ON b.delegator_user_id = a.delegator_user_id
                                         WHERE a.agent_principal_id = $4::uuid
                                           AND b.agent_principal_id = i.author_id))
         FROM issues i WHERE i.id = t.issue_id AND t.issue_id = $1
         RETURNING {}",
        crate::triage::SNAPSHOT_SQL
    )))
    .bind(issue)
    .bind(plan.decision.new_state())
    .bind(plan.snooze_until)
    .bind(at.actor_id)
    .bind(at.actor_kind)
    .fetch_one(&mut *conn)
    .await?)
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
                             start_date, due_date, is_private, last_event_seq, estimated_hours,
                             description_format)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16::numeric,$17)",
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
    .bind(v(Key::EstimatedHours).as_str().map(String::from))
    .bind(w.desc_format.as_str())
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
    if w.enters_triage {
        sqlx::query(
            "INSERT INTO issue_triage (issue_id, project_id, state, entered_at)
             VALUES ($1, $2, 'pending', now())",
        )
        .bind(w.id)
        .bind(w.project_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(key)
}

async fn update_issue(conn: &mut PgConnection, w: &Work, last_event_seq: i64) -> Result<()> {
    let v = |k: Key| w.values.get(&k).cloned().unwrap_or(Json::Null);
    sqlx::query(
        "UPDATE issues SET status_id = $2, subject = $3, description = $4, assigned_to_id = $5,
                priority_id = $6, fixed_version_id = $7, start_date = $8, due_date = $9,
                estimated_hours = $11::numeric, description_format = $12,
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
    .bind(v(Key::EstimatedHours).as_str().map(String::from))
    .bind(w.desc_format.as_str())
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
    fn estimated_hours_values() {
        assert_eq!(estimated_hours("1.50").as_deref(), Ok("1.5"));
        assert_eq!(estimated_hours("0.00").as_deref(), Ok("0"));
        assert_eq!(estimated_hours("-0").as_deref(), Ok("0"));
        assert_eq!(estimated_hours("99999.99").as_deref(), Ok("99999.99"));
        assert_eq!(estimated_hours("100000"), Err(Some("out_of_range")));
        assert_eq!(estimated_hours("-1"), Err(Some("out_of_range")));
        assert_eq!(estimated_hours("1.255"), Err(Some("too_many_decimals")));
        assert_eq!(estimated_hours("1.250").as_deref(), Ok("1.25"));
        assert_eq!(estimated_hours("1e2"), Err(None));
        // Beyond canonical_number's length limits: decided by value.
        assert_eq!(
            estimated_hours("1.123456789012"),
            Err(Some("too_many_decimals"))
        );
        assert_eq!(estimated_hours("1.00000000000").as_deref(), Ok("1"));
        assert_eq!(
            estimated_hours("123456789012345678901"),
            Err(Some("out_of_range"))
        );
        assert_eq!(estimated_hours("000000000000000000001").as_deref(), Ok("1"));
        assert_eq!(estimated_hours("1."), Err(None));
        assert_eq!(estimated_hours(".5"), Err(None));
        assert_eq!(estimated_hours("-"), Err(None));
        assert_eq!(estimated_hours("+1"), Err(None));
        assert_eq!(estimated_hours("-0.00").as_deref(), Ok("0"));
        assert_eq!(estimated_hours(""), Err(None));
        assert_eq!(Key::parse("estimated_hours"), Some(Key::EstimatedHours));
        assert_eq!(Key::EstimatedHours.name(), "estimated_hours");
    }

    #[test]
    fn dates() {
        assert!(parse_date("2026-02-30").is_none());
        assert!(parse_date("2026-2-3").is_none());
        assert!(date_in_range(parse_date("1900-01-01").unwrap()));
        assert!(!date_in_range(parse_date("2200-01-01").unwrap()));
    }
}
