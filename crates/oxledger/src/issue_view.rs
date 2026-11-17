// SPDX-License-Identifier: GPL-2.0-or-later
//! `get` with field selection, citations and attached blocks, and
//! `get_many` (design 03 §3.6, contract 02 §8.8, §14.1 用法 2 and 4).
//!
//! `fields` really limits what is read: a body that was not asked for is not
//! selected. Visibility comes first and uses the same read scope as search
//! and citations (people: their own; agents: the delegated read scope).
//! Invisible and missing issues give the same `not_found`.

use crate::auth::AuthContext;
use crate::citations::{self, Reader, Source, TextField};
use crate::describe::{self, IssueActions};
use crate::error::{ApiError, ErrorCode, NextAction, api};
use crate::evidence::CpText;
use crate::service::{
    self, App, CfTarget, CustomFieldValue, IssueBrief, read_tx, readable_cf_names, unknown_field,
};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::IssueFacts;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Issue fields `get` may select (design 03 §3.6); `cf:<name>` besides.
pub const GET_FIELDS: &[&str] = &[
    "key",
    "subject",
    "description",
    "project",
    "tracker",
    "status",
    "status_category",
    "status_since",
    "assignee",
    "priority",
    "fixed_version",
    "estimated_hours",
    "spent_hours",
    "is_private",
    "created_at",
    "updated_at",
    "last_activity_at",
    "lock_version",
    "triage",
    "attachments",
];
pub const INCLUDES: &[&str] = &["actions", "recent_notes", "recent_events", "dev"];
pub const MAX_NOTE_LIMIT: u32 = 10;
pub const DEFAULT_NOTE_LIMIT: u32 = 5;
pub const MAX_EVENT_LIMIT: u32 = 20;
pub const DEFAULT_EVENT_LIMIT: u32 = 10;
pub const MAX_GET_MANY: usize = 10;
/// Serialized size of one `get_many` response before items are cut to
/// `{key, truncated: true}`.
pub const GET_MANY_BUDGET: usize = 256 * 1024;
const PAGE: i64 = 50;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    /// Issue key such as "AUTH-12". Old keys of moved issues still work.
    pub key: String,
    /// Issue fields to read: key, subject, description, project, tracker, status,
    /// status_category, status_since, assignee, priority, fixed_version,
    /// estimated_hours, is_private, created_at, updated_at, last_activity_at,
    /// lock_version, triage, spent_hours (people only), cf:<custom field name>.
    /// Without `fields`: the full current detail (no notes or history).
    pub fields: Option<Vec<String>>,
    /// "subject" and/or "description" (must also be in `fields`): a citation_id for
    /// the whole field. Quote ≤2000 characters directly; else use get_source(selection: "subquote").
    pub citations: Option<Vec<String>>,
    /// Extra blocks: "actions" (what you may propose now), "recent_notes" (readable
    /// notes with citation_id), "recent_events" (readable history).
    pub include: Option<Vec<String>>,
    /// recent_notes: 0–10, default 5.
    pub note_limit: Option<u32>,
    /// recent_events: 0–20, default 10.
    pub event_limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetManyParams {
    /// 1–10 issue keys; each gets its own result.
    pub keys: Vec<String>,
    /// As in `get`, applied to every key.
    pub fields: Option<Vec<String>>,
    pub citations: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub note_limit: Option<u32>,
    pub event_limit: Option<u32>,
}

/// The web app's description of [`NoteView::body_html`] (its OpenAPI).
pub const NOTE_BODY_HTML_DOC: &str = "Web app only (`GET /issues/{key}/view`; never over MCP, design 06\nR-M10): the body as sanitized HTML, references resolved for the\ncaller. The only note field to put in the page as HTML.";

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NoteView {
    pub note_id: Uuid,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub is_private: bool,
    pub body: String,
    /// `markdown`, or `plain` for notes written before M2 (design 06 R-M9).
    pub body_format: String,
    /// Web app only (`GET /issues/{key}/view`; never over MCP, design 06
    /// R-M10): the body as sanitized HTML, references resolved for the
    /// caller. The only note field to put in the page as HTML. Not in the
    /// type's schema (MCP's); the web app's OpenAPI document adds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub body_html: Option<String>,
    /// An agent wrote the note (R-M3: stricter links); rendering only.
    #[serde(skip)]
    pub(crate) by_agent: bool,
    /// The body was redacted (D12); there is no citation for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    /// Retracted by an undo (§16.7): the body is hidden, the note stays.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retracted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retracted_by: Option<String>,
    /// Cites the whole note (source kind issue_note).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation_id: Option<String>,
    /// Design 06 R-A5, R-A9: the note's attachments (none on a retracted
    /// note); omitted when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::read::AttachmentMeta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecentNotes {
    /// Newest first.
    pub items: Vec<NoteView>,
    /// More readable notes exist.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventView {
    /// Opaque id (for `POST /issues/{key}/events/{event_id}/redact`); the
    /// internal sequence number is never returned.
    pub event_id: Uuid,
    /// created | fields_changed | status_changed | note_added | duplicate_added
    /// | triage_changed (contract 05 §3.4) | …
    pub kind: String,
    pub at: DateTime<Utc>,
    pub actor: String,
    /// The event's content as the caller may see it (readable fields only;
    /// ids shown as names). Absent when redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Json>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    /// `user`, `agent` or `system` (P3 gap G41; 02 §16.2).
    pub actor_kind: String,
    /// The person an agent acted for (`claude-code@alice〔agent〕`, tooltip
    /// 「代表 Alice」; G41). Absent for people's own writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<crate::time_reports::Named>,
    /// Who approved the applied proposal (「Alice 核准」, 02 §16.10; G41).
    /// Only when the viewer may know it: the approver is the person the
    /// agent acted for (the delegator, who alone reviews proposals), or the
    /// viewer is the approver or a site administrator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<crate::time_reports::Named>,
    /// The agent's direct write extended this issue's chain to at least 2
    /// agent writes with no person in between (「agent 連續寫入中」, §6.14;
    /// G40). Only this issue's own chain (a write that also touched other
    /// issues says nothing about theirs); writes from before migration 0033
    /// show no flag. A flag,
    /// never the count: the chain may hold writes the viewer cannot read
    /// (appendix D V21).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub agent_chain: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecentEvents {
    /// Newest first.
    pub items: Vec<EventView>,
    /// More readable events exist.
    pub truncated: bool,
}

/// A milestone as `get` names it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct VersionBrief {
    pub id: Uuid,
    /// The current name; show "（已刪除）" after it when `deleted` (§17.14).
    pub name: String,
    /// open | locked | closed.
    pub status: String,
    /// YYYY-MM-DD or null.
    pub effective_date: Option<String>,
    /// Soft-deleted (D-11): the issue keeps pointing at it until changed.
    pub deleted: bool,
}

/// One issue. Without `fields`, the full current detail; with `fields`, only
/// those (and `key`).
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct IssueView {
    pub key: String,
    /// Set when the requested key is an old key of a moved issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// With `description`: `markdown`, or `plain` for a description written
    /// before M2 and not rewritten since (design 06 R-M9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_category: Option<String>,
    /// When the issue last entered its current status (a transition that changed
    /// nothing does not count); the creation time if it never changed. null when
    /// the history is missing or redacted (then status_since_known is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_since: Option<Option<DateTime<Utc>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_since_known: Option<bool>,
    /// null when unassigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Option<String>>,
    /// user | group (a group assignee is not a person).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_kind: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// The milestone (`fixed_version_id`); null when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_version: Option<Option<VersionBrief>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_private: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<IssueBrief>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<Option<NaiveDate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<Option<NaiveDate>>,
    /// Decimal string such as "1.5"; null when not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_hours: Option<Option<String>>,
    /// People only (contract 05 §3.5.4): the visible time on this issue; null
    /// when the caller cannot read time in the project. Agents asking for it
    /// get `invalid_argument` (`details.reason = "unavailable_to_agent"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_hours: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// The latest domain activity the caller can see (not `updated_at`, which
    /// also moves for notes the caller cannot read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_version: Option<i32>,
    /// Contract 05 §3.4: `pending`, `snoozed`, `accepted`, `declined`,
    /// `duplicate`; null when the issue never entered the Triage inbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage: Option<Option<String>>,
    /// Contract 05 §3.6.3: full detail only, when rules published with
    /// `accept_and_mark` found this issue breaking them: the rule ids. The
    /// next write that passes the rules clears them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_rule_marks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_statuses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_fields: Option<Vec<CustomFieldValue>>,
    /// Field → citation_id of the whole field (`citations`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<IssueActions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_notes: Option<RecentNotes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_events: Option<RecentEvents>,
    /// Design 06 R-A9: the issue's own attachments (not its notes'), oldest
    /// first. In the full detail only when there are any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<crate::attachments::read::AttachmentMeta>>,
    /// `include: ["dev"]`: pull requests that name this issue (§19.7), a
    /// read-only projection that may lag GitHub by minutes (`synced_at`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev: Option<crate::github::read::IssueDev>,
    /// get_many: left out to keep the response small; call `get` for this key.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// What to read, checked once per request.
struct Plan {
    /// `None`: the full current detail.
    fields: Option<Vec<&'static str>>,
    cf: Vec<String>,
    citations: Vec<TextField>,
    actions: bool,
    notes: Option<u32>,
    events: Option<u32>,
    dev: bool,
}

fn arg_err(path: &str, msg: String, allowed: Option<&[&str]>) -> anyhow::Error {
    let details = match allowed {
        Some(a) => json!({"path": path, "allowed": a}),
        None => json!({"path": path}),
    };
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(details)
        .into()
}

/// The shared request parameters of `get` and `get_many`.
struct Ask<'a> {
    fields: Option<&'a Vec<String>>,
    citations: Option<&'a Vec<String>>,
    include: Option<&'a Vec<String>>,
    note_limit: Option<u32>,
    event_limit: Option<u32>,
}

async fn plan(conn: &mut PgConnection, caller: AuthContext, a: Ask<'_>) -> Result<Plan> {
    let mut sel: Option<Vec<&'static str>> = None;
    let mut cf: Vec<String> = Vec::new();
    if let Some(list) = a.fields {
        let mut v: Vec<&'static str> = Vec::new();
        let mut bad = None;
        for f in list {
            if let Some(name) = f.strip_prefix("cf:") {
                if !cf.iter().any(|c| c == name) {
                    cf.push(name.into());
                }
            } else if let Some(k) = GET_FIELDS.iter().find(|k| **k == f.as_str()) {
                if *k == "spent_hours" && caller.user_id().is_none() {
                    return Err(ApiError::new(
                        ErrorCode::InvalidArgument,
                        "spent_hours is not available to agents (contract 05 §4); read your delegator's own time with time_list",
                    )
                    .with_details(serde_json::json!({
                        "reason": "unavailable_to_agent",
                        "path": "fields",
                        "field": "spent_hours",
                    }))
                    .into());
                }
                if !v.contains(k) {
                    v.push(k);
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
        if let Some(b) = bad {
            return Err(unknown_field("fields", &b, GET_FIELDS, &readable));
        }
        sel = Some(v);
    }
    let mut citations = Vec::new();
    for c in a.citations.map(|c| c.as_slice()).unwrap_or_default() {
        let in_fields = sel.as_ref().is_some_and(|s| s.contains(&c.as_str()));
        match TextField::parse(c) {
            Some(f) if in_fields => {
                if !citations.contains(&f) {
                    citations.push(f)
                }
            }
            _ => {
                return Err(arg_err(
                    "citations",
                    format!(
                        "citations {c:?}: only subject or description, and the field must also be in fields"
                    ),
                    Some(&["subject", "description"]),
                ));
            }
        }
    }
    let inc: Vec<&str> = a
        .include
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if let Some(bad) = inc.iter().find(|i| !INCLUDES.contains(i)) {
        return Err(arg_err(
            "include",
            format!("unknown include {bad:?}"),
            Some(INCLUDES),
        ));
    }
    let limit = |v: Option<u32>, max: u32, default: u32, path: &str| -> Result<u32> {
        match v {
            None => Ok(default),
            Some(n) if n <= max => Ok(n),
            Some(_) => Err(arg_err(path, format!("{path} must be 0–{max}"), None)),
        }
    };
    let note_limit = limit(
        a.note_limit,
        MAX_NOTE_LIMIT,
        DEFAULT_NOTE_LIMIT,
        "note_limit",
    )?;
    let event_limit = limit(
        a.event_limit,
        MAX_EVENT_LIMIT,
        DEFAULT_EVENT_LIMIT,
        "event_limit",
    )?;
    Ok(Plan {
        fields: sel,
        cf,
        citations,
        actions: inc.contains(&"actions"),
        notes: inc.contains(&"recent_notes").then_some(note_limit),
        events: inc.contains(&"recent_events").then_some(event_limit),
        dev: inc.contains(&"dev"),
    })
}

fn column(f: &str) -> Option<&'static str> {
    Some(match f {
        "subject" => "i.subject",
        "description" => "i.description, i.description_format",
        "project" => "p.name AS project",
        "tracker" => "t.name AS tracker",
        "status" => "s.name AS status",
        "status_category" => "s.category AS status_category",
        "assignee" => "a.name AS assignee, a.kind AS assignee_kind",
        "priority" => "pr.name AS priority",
        "is_private" => "i.is_private",
        "updated_at" => "i.updated_at",
        "lock_version" => "i.lock_version",
        "triage" => {
            "(SELECT CASE WHEN tr.state = 'pending' AND tr.snoozed_until > now()
                               AND i.updated_at <= tr.snoozed_at
                          THEN 'snoozed' ELSE tr.state END
              FROM issue_triage tr WHERE tr.issue_id = i.id) AS triage"
        }
        "estimated_hours" => "trim_scale(i.estimated_hours)::text AS estimated_hours",
        "fixed_version" => "fv.id AS fixed_version_id, fv.name AS fixed_version_name, fv.status AS fixed_version_status,
             fv.effective_date AS fixed_version_date, fv.deleted_at IS NOT NULL AS fixed_version_deleted",
        _ => return None,
    })
}

/// `get` (design 03 §3.6, contract 02 §8.8).
pub async fn get(app: &App, caller: AuthContext, p: GetParams) -> Result<IssueView> {
    let mut tx = read_tx(app).await?;
    let mut reader = Reader::new(&mut tx, caller).await?;
    let plan = plan(
        &mut tx,
        caller,
        Ask {
            fields: p.fields.as_ref(),
            citations: p.citations.as_ref(),
            include: p.include.as_ref(),
            note_limit: p.note_limit,
            event_limit: p.event_limit,
        },
    )
    .await?;
    let v = get_one(&mut tx, &mut reader, caller, &p.key, &plan).await?;
    tx.commit().await?;
    Ok(v)
}

/// The issue page of the SPA (`GET /issues/{key}/view`, P1 gap G1): every
/// `get` field plus the full detail (dates, author, children, custom fields,
/// allowed statuses) and the ids an edit form sends back, read in one
/// transaction so the parts agree.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SpaIssue {
    #[serde(flatten)]
    pub view: IssueView,
    /// Web app only (never over MCP, design 06 R-M10): the description as
    /// sanitized HTML, references resolved for the caller. The only
    /// description field to put in the page as HTML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_html: Option<String>,
    /// Web app only: the description is an agent's (its last writer was an
    /// agent, or a person applied an agent's proposal; R-M3). The editor
    /// warns before a person rewrites it (design 06 §6.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_by_agent: Option<bool>,
    /// The current project key (the key prefix).
    pub project_key: String,
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    pub status_id: Uuid,
    pub assigned_to_id: Option<Uuid>,
    pub priority_id: Option<Uuid>,
    /// Design 06 R-A11: the server keeps files (`OXLEDGER_FILES_DIR`); off,
    /// the page shows no upload controls.
    pub attachments_enabled: bool,
}

pub async fn spa_view(
    app: &App,
    caller: AuthContext,
    key: &str,
    include: Option<Vec<String>>,
    note_limit: Option<u32>,
    event_limit: Option<u32>,
) -> Result<SpaIssue> {
    let mut tx = read_tx(app).await?;
    // The full detail first: it does the visibility check (not_found alike).
    let (id, d) = service::get_in(&mut tx, caller, key).await?;
    let fields: Vec<String> = GET_FIELDS.iter().map(|f| f.to_string()).collect();
    let mut reader = Reader::new(&mut tx, caller).await?;
    let plan = plan(
        &mut tx,
        caller,
        Ask {
            fields: Some(&fields),
            citations: None,
            include: include.as_ref(),
            note_limit,
            event_limit,
        },
    )
    .await?;
    let mut v = get_one(&mut tx, &mut reader, caller, &d.key, &plan).await?;
    let project_id: Uuid = sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let (description_html, by_agent) = render_html(&mut tx, caller, id, project_id, &mut v).await?;
    v.author = Some(d.author);
    v.parent = Some(d.parent);
    v.children = Some(d.children);
    v.start_date = Some(d.start_date);
    v.due_date = Some(d.due_date);
    v.allowed_statuses = Some(d.allowed_statuses);
    v.custom_fields = Some(d.custom_fields);
    let ids: (String, Uuid, Uuid, Uuid, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT p.key, i.project_id, i.tracker_id, i.status_id, i.assigned_to_id, i.priority_id
         FROM issues i JOIN projects p ON p.id = i.project_id WHERE i.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    fill_can_delete(&mut tx, caller, id, &mut v).await?;
    tx.commit().await?;
    Ok(SpaIssue {
        view: v,
        description_html,
        description_by_agent: Some(by_agent),
        project_key: ids.0,
        project_id: ids.1,
        tracker_id: ids.2,
        status_id: ids.3,
        assigned_to_id: ids.4,
        priority_id: ids.5,
        attachments_enabled: crate::attachments::enabled(),
    })
}

/// Web app only: whether the caller may delete each attachment shown
/// (R-A7), for the delete control.
async fn fill_can_delete(
    conn: &mut PgConnection,
    caller: AuthContext,
    issue: Uuid,
    v: &mut IssueView,
) -> Result<()> {
    let rows = crate::attachments::read::of_issue(&mut *conn, issue).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let (project, tracker, author, obo): (Uuid, Uuid, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT project_id, tracker_id, author_id, created_on_behalf_of_id FROM issues WHERE id = $1",
    )
    .bind(issue)
    .fetch_one(&mut *conn)
    .await?;
    let actor = crate::perm::actor(&mut *conn, caller).await?;
    let Some(pp) = crate::perm::project_perms(&mut *conn, &actor, project).await? else {
        return Ok(());
    };
    let me = caller.user_id();
    let own = crate::perm::is_own(me, author, obo);
    let may = |id: Uuid| {
        rows.iter()
            .find(|r| r.id == id)
            .is_some_and(|r| crate::attachments::read::issue_may_delete(&pp, tracker, own, me, r))
    };
    for a in v.attachments.iter_mut().flatten() {
        a.can_delete = Some(may(a.id));
    }
    for n in v.recent_notes.iter_mut().flat_map(|n| n.items.iter_mut()) {
        for a in &mut n.attachments {
            a.can_delete = Some(may(a.id));
        }
    }
    Ok(())
}

/// Design 06 R-M10: the `*_html` fields of the web app's issue page (the
/// description and the notes read), one reference lookup for all of them.
/// Fills the notes' `body_html`; returns the description's HTML and
/// whether it is an agent's.
async fn render_html(
    conn: &mut PgConnection,
    caller: AuthContext,
    issue: Uuid,
    project: Uuid,
    v: &mut IssueView,
) -> Result<(Option<String>, bool)> {
    use crate::commands::TextFormat;
    use crate::markdown::{Source, Text};
    let by_agent = crate::markdown::description_by_agent(&mut *conn, issue).await?;
    let desc = v.description.clone().unwrap_or_default();
    let desc_format = TextFormat::from_db(v.description_format.as_deref().unwrap_or("plain"));
    let mut texts = vec![Text {
        text: &desc,
        src: Source {
            format: desc_format,
            agent: by_agent,
            container: issue,
        },
        images: crate::markdown::ImageScope::Description { issue },
    }];
    if let Some(notes) = &v.recent_notes {
        for n in &notes.items {
            texts.push(Text {
                text: &n.body,
                src: Source {
                    format: TextFormat::from_db(&n.body_format),
                    agent: n.by_agent,
                    container: n.note_id,
                },
                images: crate::markdown::ImageScope::Note {
                    issue,
                    note: n.note_id,
                },
            });
        }
    }
    let mut html = crate::markdown::render_for(&mut *conn, caller, project, &texts)
        .await?
        .into_iter();
    let description_html = html.next();
    if let Some(notes) = &mut v.recent_notes {
        for n in &mut notes.items {
            n.body_html = html.next();
        }
    }
    Ok((description_html, by_agent))
}

/// One `get_many` result: the issue, or the error for that key alone.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GetManyItem {
    /// The key as requested.
    pub key: String,
    /// ok | error
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<KeyError>,
}

/// The error for one key (`not_found` for missing and invisible alike).
#[derive(Debug, Serialize, JsonSchema)]
pub struct KeyError {
    pub code: ErrorCode,
    pub next_actions: &'static [NextAction],
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GetManyResult {
    /// In the order of `keys`.
    pub items: Vec<GetManyItem>,
}

/// `get_many` (contract 02 §8.8): up to 10 keys in one read transaction, the
/// same parameters as `get` applied to each.
pub async fn get_many(app: &App, caller: AuthContext, p: GetManyParams) -> Result<GetManyResult> {
    if p.keys.is_empty() || p.keys.len() > MAX_GET_MANY {
        return Err(arg_err(
            "keys",
            format!("keys must hold 1–{MAX_GET_MANY} issue keys"),
            None,
        ));
    }
    let mut tx = read_tx(app).await?;
    let mut reader = Reader::new(&mut tx, caller).await?;
    let plan = plan(
        &mut tx,
        caller,
        Ask {
            fields: p.fields.as_ref(),
            citations: p.citations.as_ref(),
            include: p.include.as_ref(),
            note_limit: p.note_limit,
            event_limit: p.event_limit,
        },
    )
    .await?;
    let mut items = Vec::with_capacity(p.keys.len());
    let mut size = 0usize;
    for key in &p.keys {
        match get_one(&mut tx, &mut reader, caller, key, &plan).await {
            Ok(v) => {
                let n = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(0);
                let issue = if size + n > GET_MANY_BUDGET {
                    IssueView {
                        key: v.key,
                        truncated: true,
                        ..Default::default()
                    }
                } else {
                    size += n;
                    v
                };
                items.push(GetManyItem {
                    key: key.clone(),
                    outcome: "ok".into(),
                    issue: Some(issue),
                    error: None,
                });
            }
            Err(e) => match crate::error::as_api_error(&e) {
                Some(a) if a.code == ErrorCode::NotFound => items.push(GetManyItem {
                    key: key.clone(),
                    outcome: "error".into(),
                    issue: None,
                    error: Some(KeyError {
                        code: a.code,
                        next_actions: a.code.next_actions(),
                        message: a.message.clone(),
                    }),
                }),
                _ => return Err(e),
            },
        }
    }
    tx.commit().await?;
    Ok(GetManyResult { items })
}

async fn get_one(
    conn: &mut PgConnection,
    reader: &mut Reader,
    caller: AuthContext,
    key: &str,
    plan: &Plan,
) -> Result<IssueView> {
    let Some(sel) = &plan.fields else {
        // The full current detail, as `get` returned before design 03.
        let (id, d) = service::get_in(&mut *conn, caller, key).await?;
        let mut v = IssueView {
            key: d.key,
            requested_key: d.requested_key,
            subject: Some(d.subject),
            description: Some(d.description),
            description_format: Some(d.description_format),
            project: Some(d.project),
            tracker: Some(d.tracker),
            status: Some(d.status),
            status_category: Some(d.status_category),
            author: Some(d.author),
            assignee: Some(d.assignee),
            is_private: Some(d.is_private),
            parent: Some(d.parent),
            children: Some(d.children),
            start_date: Some(d.start_date),
            due_date: Some(d.due_date),
            estimated_hours: Some(d.estimated_hours),
            spent_hours: d.spent_hours.map(Some),
            created_at: Some(d.created_at),
            updated_at: Some(d.updated_at),
            lock_version: Some(d.lock_version),
            allowed_statuses: Some(d.allowed_statuses),
            custom_fields: Some(d.custom_fields),
            form_rule_marks: (!d.form_rule_marks.is_empty()).then_some(d.form_rule_marks),
            ..Default::default()
        };
        let own = issue_attachments(&mut *conn, id).await?;
        v.attachments = (!own.is_empty()).then_some(own);
        attach_blocks(conn, reader, caller, &mut v, id, plan).await?;
        return Ok(v);
    };
    let key = key.trim().to_uppercase();
    let not_found = || api(ErrorCode::NotFound, format!("no issue {key}"));
    if !describe::well_formed_key(&key) {
        return Err(not_found());
    }
    let mut sql = String::from(
        "SELECT i.id, ck.key AS current_key, i.project_id, i.tracker_id, i.status_id,
                i.is_private AS priv, i.author_id, i.assigned_to_id, i.created_at AS created",
    );
    for f in sel {
        if let Some(c) = column(f) {
            sql.push_str(", ");
            sql.push_str(c);
        }
    }
    sql.push_str(
        " FROM issue_keys rk
         JOIN issues i ON i.id = rk.issue_id
         JOIN issue_keys ck ON ck.issue_id = i.id AND ck.is_current
         JOIN projects p ON p.id = i.project_id
         JOIN trackers t ON t.id = i.tracker_id
         JOIN issue_statuses s ON s.id = i.status_id
         LEFT JOIN principals a ON a.id = i.assigned_to_id
         LEFT JOIN issue_priorities pr ON pr.id = i.priority_id
         LEFT JOIN versions fv ON fv.id = i.fixed_version_id
         WHERE rk.key = $1 AND i.archived_at IS NULL",
    );
    let row: Option<PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&key)
        .fetch_optional(&mut *conn)
        .await?;
    let row = row.ok_or_else(not_found)?;
    let id: Uuid = row.try_get("id")?;
    let project_id: Uuid = row.try_get("project_id")?;
    let tracker_id: Uuid = row.try_get("tracker_id")?;
    let facts = IssueFacts {
        project_id,
        tracker_id,
        is_private: row.try_get("priv")?,
        author_id: row.try_get("author_id")?,
        assigned_to_id: row.try_get("assigned_to_id")?,
    };
    // Invisible and missing look the same.
    if !reader.scope().can_see(&facts) {
        return Err(not_found());
    }
    let current_key: String = row.try_get("current_key")?;
    let created: DateTime<Utc> = row.try_get("created")?;
    let mut v = IssueView {
        requested_key: (current_key != key).then(|| key.clone()),
        key: current_key,
        ..Default::default()
    };
    for f in sel {
        match *f {
            "subject" => v.subject = Some(row.try_get("subject")?),
            "description" => {
                v.description = Some(row.try_get("description")?);
                v.description_format = Some(row.try_get("description_format")?);
            }
            "project" => v.project = Some(row.try_get("project")?),
            "tracker" => v.tracker = Some(row.try_get("tracker")?),
            "status" => v.status = Some(row.try_get("status")?),
            "status_category" => v.status_category = Some(row.try_get("status_category")?),
            "assignee" => {
                v.assignee = Some(row.try_get("assignee")?);
                v.assignee_kind = Some(row.try_get("assignee_kind")?);
            }
            "priority" => v.priority = Some(row.try_get("priority")?),
            "fixed_version" => {
                let id: Option<Uuid> = row.try_get("fixed_version_id")?;
                let name: Option<String> = row.try_get("fixed_version_name")?;
                let status: Option<String> = row.try_get("fixed_version_status")?;
                let date: Option<chrono::NaiveDate> = row.try_get("fixed_version_date")?;
                let deleted: Option<bool> = row.try_get("fixed_version_deleted")?;
                v.fixed_version = Some(id.zip(name).map(|(id, name)| VersionBrief {
                    id,
                    name,
                    status: status.unwrap_or_default(),
                    effective_date: date.map(|d| d.to_string()),
                    deleted: deleted.unwrap_or(false),
                }));
            }
            "is_private" => v.is_private = Some(row.try_get("is_private")?),
            "estimated_hours" => v.estimated_hours = Some(row.try_get("estimated_hours")?),
            "spent_hours" => {
                let me = caller.user_id();
                v.spent_hours = Some(match me {
                    Some(me) => {
                        crate::time_reports::issue_spent(&mut *conn, me, project_id, id).await?
                    }
                    None => None,
                });
            }
            "created_at" => v.created_at = Some(created),
            "updated_at" => v.updated_at = Some(row.try_get("updated_at")?),
            "lock_version" => v.lock_version = Some(row.try_get("lock_version")?),
            "triage" => v.triage = Some(row.try_get("triage")?),
            "attachments" => v.attachments = Some(issue_attachments(&mut *conn, id).await?),
            "status_since" => {
                let status: Uuid = row.try_get("status_id")?;
                let since = status_since(&mut *conn, id, status, created).await?;
                v.status_since = Some(since);
                v.status_since_known = Some(since.is_some());
            }
            "last_activity_at" => {
                let issue = IssueRef {
                    id,
                    project: project_id,
                    tracker: tracker_id,
                };
                let (latest, _) = visible_events(&mut *conn, reader, caller, issue, 1).await?;
                v.last_activity_at = Some(latest.first().map_or(created, |e| e.at));
            }
            _ => {}
        }
    }
    if !plan.cf.is_empty() {
        let roles = crate::db::role_ids_for(&mut *conn, caller, project_id).await?;
        v.custom_fields = Some(
            service::custom_field_values(
                &mut *conn,
                caller,
                &roles,
                CfTarget {
                    issue_id: id,
                    tracker_id,
                    project_id,
                },
                Some(plan.cf.as_slice()),
            )
            .await?,
        );
    }
    if !plan.citations.is_empty() {
        let mut map = BTreeMap::new();
        for f in &plan.citations {
            let text = match f {
                TextField::Subject => v.subject.as_deref(),
                TextField::Description => v.description.as_deref(),
            }
            .unwrap_or_default();
            let n = text.chars().count();
            if n > 0 {
                map.insert(
                    f.as_str().to_string(),
                    citations::cite(
                        Source::IssueField {
                            issue_id: id,
                            field: *f,
                        },
                        text,
                        0,
                        n,
                    ),
                );
            }
        }
        v.citations = Some(map);
    }
    attach_blocks(conn, reader, caller, &mut v, id, plan).await?;
    Ok(v)
}

/// The latest activity on an issue `reader` can see (its latest readable
/// event, else `created`): the Triage inbox's `last_activity_at`.
pub(crate) async fn last_activity_at(
    conn: &mut PgConnection,
    reader: &mut Reader,
    caller: AuthContext,
    (id, project, tracker): (Uuid, Uuid, Uuid),
    created: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let issue = IssueRef {
        id,
        project,
        tracker,
    };
    let (latest, _) = visible_events(&mut *conn, reader, caller, issue, 1).await?;
    Ok(latest.first().map_or(created, |e| e.at))
}

/// The issue an attached block (or a notification, design 05 §3.11) is about.
#[derive(Clone, Copy)]
pub(crate) struct IssueRef {
    pub(crate) id: Uuid,
    pub(crate) project: Uuid,
    pub(crate) tracker: Uuid,
}

/// `include`: actions, recent notes, recent events (same read scope).
async fn attach_blocks(
    conn: &mut PgConnection,
    reader: &mut Reader,
    caller: AuthContext,
    v: &mut IssueView,
    id: Uuid,
    plan: &Plan,
) -> Result<()> {
    if !plan.actions && plan.notes.is_none() && plan.events.is_none() && !plan.dev {
        return Ok(());
    }
    let (project, tracker): (Uuid, Uuid) =
        sqlx::query_as("SELECT project_id, tracker_id FROM issues WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    let issue = IssueRef {
        id,
        project,
        tracker,
    };
    if plan.actions {
        v.actions = Some(describe::issue_actions_in(&mut *conn, caller, &v.key).await?);
    }
    if let Some(limit) = plan.notes {
        v.recent_notes = Some(recent_notes(&mut *conn, reader, issue, limit).await?);
    }
    if let Some(limit) = plan.events {
        let (items, truncated) = visible_events(&mut *conn, reader, caller, issue, limit).await?;
        v.recent_events = Some(RecentEvents { items, truncated });
    }
    if plan.dev {
        let closed: bool = sqlx::query_scalar(
            "SELECT s.category = 'done' FROM issues i JOIN issue_statuses s ON s.id = i.status_id
             WHERE i.id = $1",
        )
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        // The issue page's card: a human who may clean the project's
        // development data sees hidden commits and the hide control
        // (design 06 R-C7); agents never do.
        let manager = caller.require_human_session().is_ok()
            && crate::github::links::access_of(&mut *conn, caller, project)
                .await
                .is_ok_and(|a| a.can_clean);
        v.dev =
            Some(crate::github::read::issue_block(&mut *conn, id, &v.key, closed, manager).await?);
    }
    Ok(())
}

/// The issue's own live attachments (R-A9), oldest first.
async fn issue_attachments(
    conn: &mut PgConnection,
    issue: Uuid,
) -> Result<Vec<crate::attachments::read::AttachmentMeta>> {
    Ok(crate::attachments::read::of_issue(&mut *conn, issue)
        .await?
        .iter()
        .filter(|r| r.note_id.is_none())
        .map(|r| r.meta())
        .collect())
}

// ---------------------------------------------------------------- status_since

/// Contract 02 用法 4, T-U8: when the issue last entered its current status.
/// A transition that changes nothing writes no event, so it never moves this.
/// The latest status event must agree with the current status; otherwise, or
/// when it is redacted or there is no history at all, the answer is `None`
/// (unknown), never a guess from `created_at`.
async fn status_since(
    conn: &mut PgConnection,
    issue: Uuid,
    current: Uuid,
    created: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let last: Option<(String, DateTime<Utc>, Json, bool)> = sqlx::query_as(
        "SELECT kind, at, payload, redaction_id IS NOT NULL FROM issue_events
         WHERE issue_id = $1 AND kind IN ('created', 'status_changed')
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(issue)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((kind, at, payload, redacted)) = last else {
        return Ok(None);
    };
    if redacted {
        return Ok(None);
    }
    let cur = current.to_string();
    Ok(match kind.as_str() {
        "status_changed" if payload["new"].as_str() == Some(cur.as_str()) => Some(at),
        "created" if payload["fields"]["status_id"].as_str() == Some(cur.as_str()) => Some(created),
        _ => None,
    })
}

// ---------------------------------------------------------------- notes

#[derive(sqlx::FromRow)]
struct NoteRow {
    id: Uuid,
    author: String,
    author_id: Uuid,
    created_on_behalf_of_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    is_private: bool,
    body: String,
    body_format: String,
    redacted: bool,
    retracted_by: Option<String>,
    retracted: bool,
    by_agent: bool,
}

/// Readable notes, newest first: the private-note rule is applied before the
/// limit, so a hidden note neither takes a slot nor shows as "omitted".
async fn recent_notes(
    conn: &mut PgConnection,
    reader: &mut Reader,
    issue: IssueRef,
    limit: u32,
) -> Result<RecentNotes> {
    let mut items = Vec::new();
    let mut before: Option<(DateTime<Utc>, Uuid)> = None;
    let files = crate::attachments::read::of_issue(&mut *conn, issue.id).await?;
    loop {
        let rows: Vec<NoteRow> = sqlx::query_as(
            "SELECT n.id, p.name AS author, n.author_id, n.created_on_behalf_of_id, n.created_at,
                    n.is_private, n.body, n.body_format, n.redacted_at IS NOT NULL AS redacted,
                    rp.name AS retracted_by, n.retracted_at IS NOT NULL AS retracted,
                    (p.kind = 'agent' OR EXISTS (
                        SELECT 1 FROM issue_events e JOIN principals pp ON pp.id = e.proposer_id
                        WHERE e.issue_id = n.issue_id AND e.note_id = n.id
                          AND e.kind = 'note_added' AND pp.kind = 'agent')) AS by_agent
             FROM issue_notes n JOIN principals p ON p.id = n.author_id
             LEFT JOIN command_executions rx ON rx.id = n.retracted_by_execution_id
             LEFT JOIN principals rp ON rp.id = rx.actor_id
             WHERE n.issue_id = $1
               AND ($2::timestamptz IS NULL OR (n.created_at, n.id) < ($2, $3))
             ORDER BY n.created_at DESC, n.id DESC LIMIT $4",
        )
        .bind(issue.id)
        .bind(before.map(|b| b.0))
        .bind(before.map(|b| b.1))
        .bind(PAGE)
        .fetch_all(&mut *conn)
        .await?;
        let Some(last) = rows.last() else { break };
        before = Some((last.created_at, last.id));
        let n = rows.len() as i64;
        for r in rows {
            if !reader
                .note_visible(
                    &mut *conn,
                    issue.project,
                    issue.tracker,
                    r.is_private,
                    r.author_id,
                    r.created_on_behalf_of_id,
                )
                .await?
            {
                continue;
            }
            if items.len() as u32 >= limit {
                return Ok(RecentNotes {
                    items,
                    truncated: true,
                });
            }
            let len = CpText::new(&r.body).len();
            items.push(NoteView {
                note_id: r.id,
                author: r.author,
                created_at: r.created_at,
                is_private: r.is_private,
                citation_id: (!r.redacted && !r.retracted && len > 0).then(|| {
                    citations::cite(
                        Source::IssueNote {
                            issue_id: issue.id,
                            note_id: r.id,
                        },
                        &r.body,
                        0,
                        len,
                    )
                }),
                body: if r.retracted { String::new() } else { r.body },
                body_format: r.body_format,
                body_html: None,
                by_agent: r.by_agent,
                redacted: r.redacted,
                retracted: r.retracted,
                retracted_by: r.retracted_by,
                attachments: if r.retracted {
                    Vec::new()
                } else {
                    files
                        .iter()
                        .filter(|f| f.note_id == Some(r.id))
                        .map(|f| f.meta())
                        .collect()
                },
            });
        }
        if n < PAGE {
            break;
        }
    }
    Ok(RecentNotes {
        items,
        truncated: false,
    })
}

// ---------------------------------------------------------------- events

#[derive(sqlx::FromRow, Default)]
pub(crate) struct EventRow {
    pub(crate) id: Uuid,
    pub(crate) seq: i64,
    pub(crate) kind: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) actor: String,
    pub(crate) payload: Json,
    pub(crate) note_id: Option<Uuid>,
    pub(crate) redacted: bool,
    /// The attribution columns ([`EVENT_BY_COLS`]); readers that only ask
    /// whether an event is readable leave them out.
    #[sqlx(default)]
    pub(crate) actor_kind: String,
    #[sqlx(default)]
    pub(crate) obo_id: Option<Uuid>,
    #[sqlx(default)]
    pub(crate) obo_name: Option<String>,
    #[sqlx(default)]
    pub(crate) approver_id: Option<Uuid>,
    #[sqlx(default)]
    pub(crate) approver_name: Option<String>,
    #[sqlx(default)]
    pub(crate) chain_writes: Option<i32>,
}

/// The attribution columns of [`EventRow`] for `issue_events e` (P3 gaps
/// G40, G41): who the agent acted for, who approved an applied proposal,
/// and the agent chain of this event's issue after the direct write (its
/// own chain, migration 0033: never another issue's of the same write).
pub(crate) const EVENT_BY_COLS: &str = "e.actor_kind, e.on_behalf_of_id AS obo_id,
    (SELECT name FROM principals WHERE id = e.on_behalf_of_id) AS obo_name,
    e.approver_id, (SELECT name FROM principals WHERE id = e.approver_id) AS approver_name,
    (SELECT c.writes FROM command_execution_chains c
      WHERE c.execution_id = e.command_execution_id AND c.issue_id = e.issue_id) AS chain_writes";

/// What [`event_view`] looks up once per reader and issue.
#[derive(Default)]
pub(crate) struct EventCache {
    /// Readable custom field ids in the issue's project (loaded on first use).
    pub(crate) readable_cf: Option<Vec<i32>>,
    notes: HashMap<Uuid, bool>,
    /// The reader is a site administrator (a person; loaded on first use).
    admin: Option<bool>,
}

/// Names for ids in event payloads (contract 02 §7.3: shown as names).
async fn display(conn: &mut PgConnection, field: &str, v: &Json) -> Result<Json> {
    let Some(id) = v.as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
        return Ok(v.clone());
    };
    if field == "fixed_version_id" {
        // §17.14: the current name; a deleted one says so.
        let row: Option<(String, bool)> =
            sqlx::query_as("SELECT name, deleted_at IS NOT NULL FROM versions WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        return Ok(match row {
            Some((name, true)) => {
                json!({"id": id, "name": format!("{name}（已刪除）"), "deleted": true})
            }
            Some((name, false)) => json!({"id": id, "name": name, "deleted": false}),
            None => json!({"id": id, "name": null}),
        });
    }
    let sql = match field {
        "assigned_to_id" => "SELECT name FROM principals WHERE id = $1",
        "priority_id" => "SELECT name FROM issue_priorities WHERE id = $1",
        "status_id" => "SELECT name FROM issue_statuses WHERE id = $1",
        _ => return Ok(v.clone()),
    };
    let name: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(json!({"id": id, "name": name}))
}

/// Custom field ids the caller may read in this issue's project (agents:
/// `visible = true` only).
pub(crate) async fn readable_cf_ids(
    conn: &mut PgConnection,
    reader: &Reader,
    caller: AuthContext,
    project: Uuid,
) -> Result<Vec<i32>> {
    let (admin, roles) = if reader.is_agent() {
        (false, Vec::new())
    } else {
        (
            crate::db::is_admin(&mut *conn, caller).await?,
            crate::db::role_ids_for(&mut *conn, caller, project).await?,
        )
    };
    Ok(sqlx::query_scalar(
        "SELECT d.id FROM field_defs d WHERE d.index_state <> 'dropping'
           AND (d.visible OR $1 OR EXISTS (SELECT 1 FROM field_roles fr
                                           WHERE fr.field_id = d.id AND fr.role_id = ANY($2)))",
    )
    .bind(admin)
    .bind(&roles)
    .fetch_all(&mut *conn)
    .await?)
}

/// Readable fields of a `created` / `fields_changed` map, ids as names.
async fn filter_fields(
    conn: &mut PgConnection,
    readable_cf: &[i32],
    map: &Map<String, Json>,
    changes: bool,
) -> Result<Map<String, Json>> {
    let mut out = Map::new();
    for (k, val) in map {
        if let Some(id) = k.strip_prefix("cf:")
            && !id.parse::<i32>().is_ok_and(|i| readable_cf.contains(&i))
        {
            continue;
        }
        let shown = if changes {
            json!({
                "old": display(&mut *conn, k, &val["old"]).await?,
                "new": display(&mut *conn, k, &val["new"]).await?,
            })
        } else {
            display(&mut *conn, k, val).await?
        };
        out.insert(k.clone(), shown);
    }
    Ok(out)
}

/// Whether the caller may see note `note` of `issue` (cached per request).
async fn note_ok(
    conn: &mut PgConnection,
    reader: &mut Reader,
    cache: &mut HashMap<Uuid, bool>,
    issue: IssueRef,
    note: Uuid,
) -> Result<bool> {
    if let Some(ok) = cache.get(&note) {
        return Ok(*ok);
    }
    let row: Option<(bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT is_private, author_id, created_on_behalf_of_id FROM issue_notes
         WHERE id = $1 AND issue_id = $2",
    )
    .bind(note)
    .bind(issue.id)
    .fetch_optional(&mut *conn)
    .await?;
    let ok = match row {
        None => false,
        Some((p, a, b)) => {
            reader
                .note_visible(&mut *conn, issue.project, issue.tracker, p, a, b)
                .await?
        }
    };
    cache.insert(note, ok);
    Ok(ok)
}

/// The other end of a duplicate, if the caller can see it.
async fn visible_key(
    conn: &mut PgConnection,
    reader: &Reader,
    other: Uuid,
) -> Result<Option<String>> {
    let row: Option<(Uuid, Uuid, bool, Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id, k.key
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = $1 AND i.archived_at IS NULL",
    )
    .bind(other)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|(p, t, pr, a, asg, k)| {
        reader
            .scope()
            .can_see(&IssueFacts {
                project_id: p,
                tracker_id: t,
                is_private: pr,
                author_id: a,
                assigned_to_id: asg,
            })
            .then_some(k)
    }))
}

/// Readable events, newest first, at most `limit`, and whether more exist
/// (contract 02 §7.4: filter first, then limit; hidden events leave no trace).
async fn visible_events(
    conn: &mut PgConnection,
    reader: &mut Reader,
    caller: AuthContext,
    issue: IssueRef,
    limit: u32,
) -> Result<(Vec<EventView>, bool)> {
    let mut cache = EventCache::default();
    let mut out = Vec::new();
    let mut before: Option<i64> = None;
    loop {
        let rows: Vec<EventRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT e.id, e.seq, e.kind, e.at, p.name AS actor, e.payload, e.note_id,
                    e.redaction_id IS NOT NULL AS redacted, {EVENT_BY_COLS}
             FROM issue_events e JOIN principals p ON p.id = e.actor_id
             WHERE e.issue_id = $1 AND ($2::bigint IS NULL OR e.seq < $2)
             ORDER BY e.seq DESC LIMIT $3"
        )))
        .bind(issue.id)
        .bind(before)
        .bind(PAGE)
        .fetch_all(&mut *conn)
        .await?;
        let Some(last) = rows.last() else { break };
        before = Some(last.seq);
        let n = rows.len() as i64;
        for r in rows {
            let Some(v) = event_view(&mut *conn, reader, caller, issue, &mut cache, r).await?
            else {
                continue;
            };
            if out.len() as u32 >= limit {
                return Ok((out, true));
            }
            out.push(v);
        }
        if n < PAGE {
            break;
        }
    }
    Ok((out, false))
}

/// One event as `caller` may see it now, or `None` when nothing of it is
/// readable (a hidden note, only unreadable fields, the other end of a
/// duplicate out of sight). The issue itself must already be visible to
/// `caller`. Issue history (`recent_events`) and notifications (design 05
/// §3.11.2 step 3, §3.11.3) both use this one function, so they agree.
pub(crate) async fn event_view(
    conn: &mut PgConnection,
    reader: &mut Reader,
    caller: AuthContext,
    issue: IssueRef,
    cache: &mut EventCache,
    r: EventRow,
) -> Result<Option<EventView>> {
    // Notes first: a hidden note's event is left out even when redacted.
    if let Some(note) = r.note_id
        && !note_ok(&mut *conn, reader, &mut cache.notes, issue, note).await?
    {
        return Ok(None);
    }
    // R-A5: a retracted note's files are out of sight, names included,
    // redacted or not (the note's own event stays, as an empty note).
    if matches!(r.kind.as_str(), "attachment_added" | "attachment_removed")
        && let Some(note) = r.note_id
    {
        let retracted: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM issue_notes
                            WHERE id = $1 AND retracted_at IS NOT NULL)",
        )
        .bind(note)
        .fetch_one(&mut *conn)
        .await?;
        if retracted {
            return Ok(None);
        }
    }
    let detail = if r.redacted {
        None
    } else {
        match r.kind.as_str() {
            "created" | "fields_changed" => {
                if cache.readable_cf.is_none() {
                    cache.readable_cf =
                        Some(readable_cf_ids(&mut *conn, reader, caller, issue.project).await?);
                }
                let rc = cache.readable_cf.as_deref().unwrap_or_default();
                let changes = r.kind == "fields_changed";
                let key = if changes { "changes" } else { "fields" };
                let empty = Map::new();
                let map = r.payload[key].as_object().unwrap_or(&empty);
                let shown = filter_fields(&mut *conn, rc, map, changes).await?;
                // Design 06 R-M9: the description's format (it changes only
                // with the description, which every reader of the issue sees).
                let format = r.payload.get("description_format");
                if changes && shown.is_empty() && format.is_none() {
                    return Ok(None);
                }
                let mut d = json!({ key: shown });
                if let Some(f) = format {
                    d["description_format"] = f.clone();
                }
                if let Some(t) = r.payload.get("triage") {
                    d["triage"] = t.clone();
                }
                Some(d)
            }
            // Contract 05 §3.4: the decision and its A5 flags
            // ("自建自分流" in the history).
            "triage_changed" => Some(json!({
                "old": r.payload["old"],
                "new": r.payload["new"],
                "self_triage": r.payload["self_triage"],
                "same_sponsor": r.payload["same_sponsor"],
                "snoozed_until": r.payload["snoozed_until"],
            })),
            "status_changed" => Some(json!({
                "old": display(&mut *conn, "status_id", &r.payload["old"]).await?,
                "new": display(&mut *conn, "status_id", &r.payload["new"]).await?,
            })),
            "note_added" | "note_retracted" => Some(json!({"note_id": r.note_id})),
            // Design 06 R-A7: which file (the note's, when on a note).
            "attachment_added" | "attachment_removed" => Some(json!({
            "attachment_id": r.payload["attachment_id"],
            "filename": r.payload["filename"],
            "note_id": r.note_id,
            })),
            "duplicate_added" | "duplicate_removed" => {
                let other = r.payload["other_issue_id"]
                    .as_str()
                    .and_then(|s| s.parse::<Uuid>().ok());
                let Some(other) = other else {
                    return Ok(None);
                };
                let Some(k) = visible_key(&mut *conn, reader, other).await? else {
                    return Ok(None);
                };
                Some(json!({"role": r.payload["role"], "other_key": k}))
            }
            _ => None,
        }
    };
    let named = |id: Option<Uuid>, name: Option<String>| match (id, name) {
        (Some(id), Some(name)) => Some(crate::time_reports::Named { id, name }),
        _ => None,
    };
    // G41: the approver only where the viewer may know it (see EventView).
    let approved_by = match r.approver_id {
        None => None,
        Some(a) if r.obo_id == Some(a) || caller.user_id() == Some(a) => {
            named(Some(a), r.approver_name.clone())
        }
        Some(a) => {
            if cache.admin.is_none() {
                cache.admin = Some(
                    caller.user_id().is_some() && crate::db::is_admin(&mut *conn, caller).await?,
                );
            }
            if cache.admin == Some(true) {
                named(Some(a), r.approver_name.clone())
            } else {
                None
            }
        }
    };
    let agent = r.actor_kind == "agent";
    Ok(Some(EventView {
        event_id: r.id,
        kind: r.kind,
        at: r.at,
        actor: r.actor,
        detail,
        redacted: r.redacted,
        on_behalf_of: if agent {
            named(r.obo_id, r.obo_name)
        } else {
            None
        },
        approved_by,
        // G40: the chain the direct write extended (agent writes only).
        agent_chain: agent && r.chain_writes.is_some_and(|w| w >= 2),
        actor_kind: r.actor_kind,
    }))
}
