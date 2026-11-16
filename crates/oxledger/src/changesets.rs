// SPDX-License-Identifier: GPL-2.0-or-later
//! ChangeSets (contract 02 §8): an agent proposes a command document, its
//! delegator reviews and approves it in the browser, and the approval runs
//! it through the same engine as `human_direct` (§6.7 `Mode::Apply`).
//!
//! Approve-and-apply is three short transactions (§8.6):
//!
//! ```text
//! A  changeset FOR UPDATE → operation_requests → checks → decision(approve)
//!    + execution(queued), state approved
//! B  changeset → execution FOR UPDATE → context → principals, session,
//!    delegation, client FOR SHARE → projects (R and destination, UUID order)
//!    → §6.7 steps 7–17 → application + execution succeeded + changeset
//!    succeeded + operation request completed
//! C  (only after a domain failure in B) changeset → execution again;
//!    an application found → success; else execution failed and, only if
//!    still approved at that version, changeset failed
//! ```
//!
//! An execution is persisted in A before any effect, so a request that is
//! cut off anywhere can be resent with the same Idempotency-Key and ends as
//! zero effects or exactly one application (C-1…C-6). GET never writes.
//!
//! S5 leaves to S6: evidence verification (§6.7 step 7), the audience checks
//! (steps 8, 14), quotas (§8.9), groups and the MCP tools; to S6b the queue
//! review mode and revise-and-apply.

use crate::auth::{AuthContext, PrincipalKind};
use crate::commands::{
    self, Attribution, CommandDoc, CommandResult, Effect, EvalCtx, ItemResult, Mode, Parsed,
    SearchReceipt,
};
use crate::db;
use crate::describe::MAX_OPS;
use crate::error::{ApiError, ErrorCode, NextAction, api, classify};
use crate::perm::DelegationScope;
use crate::policy;
use crate::quota;
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const ACTION_PROPOSE: &str = "changeset_propose";
/// `command_submit` (§16.4): direct, or a ChangeSet when the policy says review.
pub const ACTION_SUBMIT: &str = "command_submit";
pub const ACTION_REVISE: &str = "changeset_revise";
pub const ACTION_OWNER_REVISE: &str = "changeset_owner_revise";
pub const ACTION_CANCEL: &str = "changeset_cancel";
pub const ACTION_REJECT: &str = "changeset_reject";
pub const ACTION_APPROVE: &str = "approve_and_apply";
pub const ACTION_REVISE_AND_APPLY: &str = "revise_and_apply";
/// `changeset_versions.audience_contract_ver` (§8.1).
pub const AUDIENCE_CONTRACT_VER: i32 = crate::audience::AUDIENCE_ALGORITHM_VERSION;
const COMMAND_SCHEMA_VERSION: i32 = 1;
/// Default lifetime of a proposal, capped by the delegation (§8.2).
const DEFAULT_DAYS: i32 = 7;
const RETRIES: usize = 3;
const LIST_LIMIT: i64 = 200;

// ---------------------------------------------------------------- fault injection (C-1…C-6)

/// Test-only fault points: a test arms a point to fail once (the request is
/// cut off there) or to block until released (a barrier).
#[cfg(feature = "test-support")]
pub mod faults {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// A barrier: `reached` fires when the request gets there, and it waits for `release`.
    #[derive(Default)]
    pub struct Gate {
        pub reached: Reached,
        pub release: Notify,
    }

    impl Gate {
        /// A gate whose `reached.notified()` gives up after `timeout`
        /// instead of [`crate::test_support::WAIT_TIMEOUT`].
        pub fn with_timeout(timeout: Duration) -> Gate {
            let g = Gate::default();
            if let Ok(mut t) = g.reached.timeout.lock() {
                *t = Some(timeout);
            }
            g
        }
    }

    /// `reached` of a [`Gate`]: a `Notify` whose wait is bounded (T1b). A
    /// gate that is never reached (the work skipped the point, G21) panics
    /// with the point's name, the outbox backlog and the xid holders
    /// instead of hanging the test.
    #[derive(Default)]
    pub struct Reached {
        notify: Notify,
        /// The point the gate was armed at (set by [`arm`]).
        point: Mutex<Option<String>>,
        timeout: Mutex<Option<Duration>>,
    }

    impl Reached {
        pub fn notify_one(&self) {
            self.notify.notify_one();
        }

        /// How long [`Self::notified`] waits:
        /// [`crate::test_support::WAIT_TIMEOUT`] unless the gate was made
        /// with [`Gate::with_timeout`].
        pub fn timeout(&self) -> Duration {
            self.timeout
                .lock()
                .ok()
                .and_then(|t| *t)
                .unwrap_or(crate::test_support::WAIT_TIMEOUT)
        }

        /// The point [`arm`] armed this gate at.
        pub fn point(&self) -> Option<String> {
            self.point.lock().ok().and_then(|p| p.clone())
        }

        /// Waits until the armed point is hit; panics after
        /// [`Self::timeout`] with the point, the caller's line and the state.
        #[track_caller]
        pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
            let at = std::panic::Location::caller();
            async move {
                let point = self.point().unwrap_or_else(|| "(not armed)".into());
                crate::test_support::wait_at(
                    &format!("fault gate {point} to be reached"),
                    self.timeout(),
                    self.notify.notified(),
                    at,
                )
                .await;
            }
        }
    }

    pub enum Action {
        Fail,
        Block(Arc<Gate>),
        /// Fails like a deadlock (retryable, T-I6).
        Deadlock,
    }

    /// The error [`Action::Deadlock`] raises; `is_retryable_db` treats it as 40P01.
    #[derive(Debug)]
    pub struct InjectedDeadlock;

    impl std::fmt::Display for InjectedDeadlock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("injected deadlock")
        }
    }

    impl std::error::Error for InjectedDeadlock {}

    static ARMED: Mutex<Vec<(String, Action)>> = Mutex::new(Vec::new());

    /// Arms `point` for its next hit only.
    pub fn arm(point: &str, a: Action) {
        if let Action::Block(g) = &a
            && let Ok(mut p) = g.reached.point.lock()
        {
            *p = Some(point.to_string());
        }
        ARMED.lock().unwrap().push((point.to_string(), a));
    }

    pub fn clear() {
        ARMED.lock().unwrap().clear();
    }

    pub(crate) async fn hit(point: &str) -> anyhow::Result<()> {
        let a = {
            let mut v = ARMED.lock().unwrap();
            v.iter()
                .position(|(p, _)| p == point)
                .map(|i| v.remove(i).1)
        };
        match a {
            None => Ok(()),
            // A cut-off connection: the request's outcome is unknown.
            Some(Action::Fail) => Err(anyhow::Error::from(sqlx::Error::PoolClosed)
                .context(format!("fault injected at {point}"))),
            Some(Action::Deadlock) => Err(InjectedDeadlock.into()),
            Some(Action::Block(g)) => {
                g.reached.notify_one();
                g.release.notified().await;
                Ok(())
            }
        }
    }
}

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- views

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct TargetView {
    pub id: Uuid,
    pub key: String,
}

/// One blocking problem: which op, which code, which field (§8.4).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Blocking {
    pub op_id: Option<Uuid>,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CompactValidation {
    /// `passed`, `blocked` or `conflict`.
    pub status: String,
    /// At most 10.
    pub blocking: Vec<Blocking>,
    pub warning_codes: Vec<String>,
}

/// The compact propose / revise response (§8.4).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProposeResult {
    pub changeset_id: Uuid,
    pub version: i32,
    pub proposal_hash: String,
    pub state: String,
    pub group_id: Option<Uuid>,
    pub targets: Vec<TargetView>,
    /// Absent on an agent's replay when a target is no longer readable (§8.8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<CompactValidation>,
    /// `not_readable` (or why the content is gone) when `validation` is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_unavailable: Option<String>,
    pub review_url: String,
    pub expires_at: DateTime<Utc>,
    pub next_actions: Vec<NextAction>,
    /// `command_submit` with `reason = "agent_chain"` (design 05 §3.12.4):
    /// `{exceeded: "depth"|"writes", max_depth, max_writes}`, never the
    /// chain's counts or agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
}

/// An error as the owner sees it on a failed execution or a validation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorView {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Json::is_null")]
    pub details: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ExecItem {
    pub op_id: Uuid,
    /// `applied`, `no_effect`, `failed`, `rolled_back` or `not_run`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<Uuid>,
    /// The page an approved wiki proposal wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki_page_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecutionView {
    pub execution_id: Uuid,
    pub changeset_id: Uuid,
    pub version: i32,
    pub attempt_no: i32,
    /// `queued`, `running`, `succeeded` or `failed`.
    pub status: String,
    pub error: Option<ErrorView>,
    pub items: Vec<ExecItem>,
    pub effects: Vec<Effect>,
    /// Only when succeeded: the token from the domain transaction (§6.9).
    pub search: Option<SearchReceipt>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// approve-and-apply: `pending` → HTTP 202 (the execution is persisted but
/// its result is not known yet; resend the same request).
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub pending: bool,
    pub execution_id: Uuid,
    pub execution: Option<ExecutionView>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChangesetView {
    pub changeset_id: Uuid,
    pub state: String,
    pub current_version: i32,
    pub version: i32,
    pub proposal_hash: String,
    pub edit_method: String,
    pub editor_id: Uuid,
    pub based_on_version: Option<i32>,
    pub proposer_id: Uuid,
    pub on_behalf_of_id: Uuid,
    pub context_project_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub version_created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The normalized command document; `null` with `content_unavailable`.
    pub proposal: Option<Json>,
    /// `redacted`, `purged` or `not_readable`.
    pub content_unavailable: Option<String>,
    /// The check at creation: codes and statuses only.
    pub validation: Option<Json>,
    pub targets: Vec<TargetView>,
    pub requires_human_approval: bool,
    pub review_url: String,
    pub last_execution: Option<ExecutionView>,
    /// Contract 05 §3.6.3: the form rules of the targets changed since this
    /// version was stored (review it in full; it cannot be queue-ticked).
    pub rules_changed: bool,
    /// G12: every changed field of this version with display values, as the
    /// queue shows it (`before` = the issue now, `after` = proposed,
    /// `original` = when the agent proposed it, G13). Empty with
    /// `content_unavailable`.
    pub diff: Vec<crate::review_queue::DiffRow>,
    /// Design 06 §3.12: a wiki proposal against the page as it is now
    /// (R-W7's diff). Absent for issue proposals and with
    /// `content_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki: Option<crate::wiki::WikiProposalDiff>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChangesetSummary {
    pub changeset_id: Uuid,
    pub state: String,
    pub current_version: i32,
    /// The hash of `current_version` (a queue tick binds version + hash).
    pub proposal_hash: Option<String>,
    pub title: Option<String>,
    pub targets: Vec<TargetView>,
    pub group_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_execution_status: Option<String>,
    /// Proposed rows only: every changed field with before and after (§15.3).
    pub diff: Vec<crate::review_queue::DiffRow>,
    /// `evidence_changed` when a citation no longer verifies for you.
    pub warnings: Vec<String>,
    pub validation: Option<SummaryValidation>,
    /// `["detail"]`, or `["detail","queue"]` when it may be ticked.
    pub review_modes: Vec<String>,
    /// Contract 05 §3.6.3: the form rules changed since this version.
    pub rules_changed: bool,
    /// Design 05 §3.12.4: `depth` (換手過多) or `writes` (寫入次數過多) when
    /// the agent chain limit sent it for review; no counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_chain: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SummaryValidation {
    pub status: String,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChangesetList {
    pub items: Vec<ChangesetSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StateView {
    pub changeset_id: Uuid,
    pub state: String,
    pub current_version: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OpEffect {
    pub op_id: Uuid,
    /// `would_change` or `no_effect`.
    pub status: String,
}

/// `POST /changesets/{id}/validations` (§8.5).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Validation {
    pub version: i32,
    pub proposal_hash: String,
    /// `passed`, `blocked` or `conflict`.
    pub status: String,
    pub checked_at: DateTime<Utc>,
    /// structure, visibility, permissions, preconditions, workflow:
    /// `passed`, `failed` or `not_checked`.
    pub checks: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub effects: Vec<OpEffect>,
    pub error: Option<ErrorView>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OperationView {
    pub operation_id: Uuid,
    pub action: String,
    /// `in_progress`, `completed` or `failed`.
    pub status: String,
    pub changeset_id: Option<Uuid>,
    pub version: Option<i32>,
    pub execution: Option<ExecutionView>,
}

// ---------------------------------------------------------------- inputs

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveInput {
    pub version: i32,
    /// `sha256:<hex>` as returned with the version.
    pub proposal_hash: String,
    /// Only `detail` in S5 (`queue` and `revise_and_apply` come with S6b).
    #[serde(default)]
    pub review_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    pub version: i32,
    pub proposal_hash: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateInput {
    pub version: i32,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevisionInput {
    pub expected_current_version: i32,
    /// The complete new document (no deep merge, §8.8).
    pub proposal: CommandDoc,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct CancelInput {}

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone, sqlx::FromRow)]
struct CsRow {
    id: Uuid,
    context_project_id: Uuid,
    proposer_id: Uuid,
    on_behalf_of_id: Uuid,
    delegation_id: Option<Uuid>,
    context_id: Option<Uuid>,
    current_version: i32,
    state: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    group_id: Option<Uuid>,
    /// Design 05 §3.12.4: `depth` or `writes` when the chain limit sent it.
    chain_review: Option<String>,
    /// `external_agent` or `builtin_ai` (design 05 §2.12, B-42).
    origin: String,
}

const CS_COLS: &str =
    "id, context_project_id, proposer_id, on_behalf_of_id, delegation_id, context_id,
                       current_version, state, created_at, expires_at, group_id, chain_review, origin";

#[derive(Debug, Clone, sqlx::FromRow)]
struct VerRow {
    proposal_hash: Vec<u8>,
    authority_ceiling: Json,
    editor_id: Uuid,
    edit_method: String,
    based_on_version: Option<i32>,
    validation: Option<Json>,
    created_at: DateTime<Utc>,
    body: Option<Json>,
    content_state: String,
    /// Contract 05 §3.6.3 (0024): the form rules version at storing.
    rules_version: Option<String>,
    /// G13 (0031): the targets' values at proposal time; `None` for an
    /// owner's version, an older row, or redacted content.
    baseline: Option<Json>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ExecRow {
    id: Uuid,
    changeset_id: Uuid,
    version: i32,
    proposal_hash: Vec<u8>,
    attempt_no: i32,
    operation_request_id: Uuid,
    executor_id: Uuid,
    status: String,
    error_code: Option<String>,
    item_results: Option<Json>,
    consistent_after: Option<String>,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

const EXEC_COLS: &str = "id, changeset_id, version, proposal_hash, attempt_no, operation_request_id,
                         executor_id, status, error_code, item_results, consistent_after, created_at,
                         finished_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct DelegRow {
    id: Uuid,
    project_id: Uuid,
    proposal_ops: Vec<String>,
    writable_fields: Vec<String>,
    max_ops: i32,
    expires_at: DateTime<Utc>,
    policy_version: i64,
}

async fn load_cs(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<Option<CsRow>> {
    let sql = format!(
        "SELECT {CS_COLS} FROM changesets WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

async fn load_version(conn: &mut PgConnection, cs: Uuid, version: i32) -> Result<Option<VerRow>> {
    Ok(sqlx::query_as(
        "SELECT v.proposal_hash, v.authority_ceiling, v.editor_id, v.edit_method,
                v.based_on_version, v.validation, v.created_at, c.body, c.state AS content_state,
                v.rules_version, c.baseline
         FROM changeset_versions v JOIN proposal_contents c ON c.id = v.content_id
         WHERE v.changeset_id = $1 AND v.version = $2",
    )
    .bind(cs)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?)
}

async fn load_exec(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<Option<ExecRow>> {
    let sql = format!(
        "SELECT {EXEC_COLS} FROM changeset_executions WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such changeset")
}

fn hash_str(h: &[u8]) -> String {
    format!("sha256:{}", hex::encode(h))
}

fn parse_hash(s: &str) -> Result<Vec<u8>> {
    s.strip_prefix("sha256:")
        .and_then(|h| hex::decode(h).ok())
        .filter(|b| b.len() == 32)
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::InvalidRequest,
                "proposal_hash is \"sha256:<64 hex digits>\"",
            )
            .with_details(json!({"path": "proposal_hash"}))
            .into()
        })
}

fn review_path(id: Uuid, version: i32) -> String {
    format!("/changesets/{id}?version={version}")
}

fn ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

async fn clock(conn: &mut PgConnection) -> Result<DateTime<Utc>> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?)
}

fn state_error(cs: &CsRow, applied: Option<Uuid>) -> Option<anyhow::Error> {
    match cs.state.as_str() {
        "succeeded" => Some(
            ApiError::new(
                ErrorCode::AlreadyApplied,
                "this proposal was applied already",
            )
            .with_details(json!({"execution_id": applied}))
            .into(),
        ),
        "rejected" | "canceled" => Some(api(
            ErrorCode::ChangesetNotExecutable,
            format!("the proposal was {}", cs.state),
        )),
        "expired" => Some(api(ErrorCode::ChangesetExpired, "the proposal expired")),
        _ => None,
    }
}

// ---------------------------------------------------------------- proposal hash (§8.1)

/// Everything `proposal_hash` covers.
pub struct HashInput<'a> {
    /// The canonical command document (title, ops, op ids, binds,
    /// preconditions, evidence, exec_mode).
    pub document: &'a Json,
    pub expires_at: DateTime<Utc>,
    pub ceiling: &'a Json,
    pub origin: &'a str,
    pub proposer: Uuid,
    pub delegation: Uuid,
    pub context: Uuid,
    /// (project_id, security_epoch), any order (sorted here).
    pub sources: &'a [(Uuid, i64)],
    pub audience_contract_ver: i32,
}

/// `SHA-256("oxledger/proposal/v1" ‖ command_schema_version ‖
/// canonical(document and expires_at) ‖ canonical(ceiling) ‖ origin ‖ proposer
/// ‖ delegation ‖ context ‖ canonical(sources) ‖ audience_contract_ver)`, parts
/// joined by a newline. Canonical JSON sorts keys and keeps arrays in order; uuids
/// are lowercase hyphenated, times UTC with microseconds, epochs decimal
/// strings; no Unicode normalization.
pub fn proposal_hash(h: &HashInput) -> [u8; 32] {
    let mut sources = h.sources.to_vec();
    sources.sort();
    let sources = Json::Array(
        sources
            .iter()
            .map(|(p, e)| json!({"project_id": p, "security_epoch": e.to_string()}))
            .collect(),
    );
    let doc = json!({"document": h.document, "expires_at": ts(h.expires_at)});
    let parts = [
        "oxledger/proposal/v1".to_string(),
        COMMAND_SCHEMA_VERSION.to_string(),
        commands::canonical_json(&doc),
        commands::canonical_json(h.ceiling),
        h.origin.to_string(),
        h.proposer.to_string(),
        h.delegation.to_string(),
        h.context.to_string(),
        commands::canonical_json(&sources),
        h.audience_contract_ver.to_string(),
    ];
    Sha256::digest(parts.join("\n")).into()
}

/// The stored form of a document: through [`CommandDoc`] (so omitted
/// defaults and explicit defaults are the same), `exec_mode` explicit, and
/// `is_private` of each new issue fixed to the tracker default when omitted
/// (§6.1, T-W7).
fn canonical_doc(norm: &Json, private_defaults: &BTreeMap<usize, bool>) -> Result<Json> {
    let mut d: CommandDoc = serde_json::from_value(norm.clone()).map_err(|e| {
        api(
            ErrorCode::InvalidRequest,
            format!("not a command document: {e}"),
        )
    })?;
    d.exec_mode = Some("atomic".into());
    for (i, op) in d.ops.iter_mut().enumerate() {
        if let commands::OpInput::CreateIssue(c) = op
            && !c.fields.contains_key("is_private")
            && let Some(p) = private_defaults.get(&i)
        {
            c.fields.insert("is_private".into(), json!(p));
        }
    }
    Ok(serde_json::to_value(d)?)
}

fn ceiling_json(d: &DelegRow) -> Json {
    let mut ops = d.proposal_ops.clone();
    ops.sort();
    let mut fields = d.writable_fields.clone();
    fields.sort();
    json!({"projects": [{"project_id": d.project_id, "proposal_ops": ops,
                         "writable_fields": fields, "max_ops": d.max_ops}]})
}

fn ceiling_map(c: &Json) -> BTreeMap<Uuid, DelegationScope> {
    let strings = |v: &Json| -> BTreeSet<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    c["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let id = p["project_id"].as_str()?.parse().ok()?;
            Some((
                id,
                DelegationScope {
                    proposal_ops: strings(&p["proposal_ops"]),
                    writable_fields: strings(&p["writable_fields"]),
                    max_ops: p["max_ops"].as_i64().unwrap_or(10) as i32,
                },
            ))
        })
        .collect()
}

// ---------------------------------------------------------------- validation results

fn error_field(d: &Json) -> Option<String> {
    if let Some(f) = d.get("field").and_then(Json::as_str) {
        return Some(f.to_string());
    }
    if let Some(f) = d
        .get("fields")
        .and_then(Json::as_array)
        .and_then(|a| a.first())
        .and_then(Json::as_str)
    {
        return Some(f.to_string());
    }
    d.get("path")
        .and_then(Json::as_str)
        .and_then(|p| p.rsplit('.').next())
        .map(String::from)
}

fn is_conflict(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::Conflict | ErrorCode::PreconditionFailed)
}

/// The stored check of a version (§6.7 step 9): codes and statuses only.
fn stored_validation(r: &Result<commands::Sim>, at: DateTime<Utc>) -> Json {
    match r {
        Ok(sim) => json!({
            "status": "passed", "checked_at": at, "blocking": [],
            "items": sim.items.iter().map(|i| json!({"op_id": i.op_id,
                "status": if i.status == "applied" { "would_change" } else { "no_effect" }})).collect::<Vec<_>>(),
        }),
        Err(e) => failed_validation(e, at),
    }
}

/// The stored validation of a check that failed: `conflict` or `blocked`.
fn failed_validation(e: &anyhow::Error, at: DateTime<Utc>) -> Json {
    let a = classify(e);
    let code = a
        .as_ref()
        .map_or(ErrorCode::TemporarilyUnavailable, |a| a.code);
    let d = a.and_then(|a| a.details).unwrap_or(Json::Null);
    let blocking = Blocking {
        op_id: d
            .get("op_id")
            .and_then(Json::as_str)
            .and_then(|s| s.parse().ok()),
        code: code.as_str().to_string(),
        field: error_field(&d),
    };
    json!({
        "status": if is_conflict(code) { "conflict" } else { "blocked" },
        "checked_at": at, "blocking": [blocking], "items": [],
    })
}

/// The stored validation of a wiki proposal's trial write.
fn wiki_validation(r: &Result<CommandResult>, at: DateTime<Utc>) -> Json {
    match r {
        Ok(c) => json!({
            "status": "passed", "checked_at": at, "blocking": [],
            "items": c.items.iter().map(|i| json!({"op_id": i.op_id,
                "status": if i.status == "applied" { "would_change" } else { "no_effect" }})).collect::<Vec<_>>(),
        }),
        Err(e) => failed_validation(e, at),
    }
}

fn compact(v: &Json) -> CompactValidation {
    CompactValidation {
        status: v["status"].as_str().unwrap_or("blocked").to_string(),
        blocking: serde_json::from_value::<Vec<Blocking>>(v["blocking"].clone())
            .unwrap_or_default()
            .into_iter()
            .take(10)
            .collect(),
        warning_codes: Vec::new(),
    }
}

fn next_for(status: &str) -> Vec<NextAction> {
    match status {
        "passed" => vec![NextAction::AskHuman],
        "conflict" => vec![NextAction::RefreshAndRevise],
        _ => vec![NextAction::FixInput],
    }
}

fn error_view(e: &anyhow::Error) -> ErrorView {
    match classify(e) {
        Some(a) => ErrorView {
            code: a.code.as_str().to_string(),
            message: a.message.clone(),
            details: a.details.clone().unwrap_or(Json::Null),
        },
        None => ErrorView {
            code: ErrorCode::TemporarilyUnavailable.as_str().to_string(),
            message: "the check could not finish".into(),
            details: Json::Null,
        },
    }
}

// ---------------------------------------------------------------- visibility helpers

/// Current keys of the issues in `ids` that `caller` can see.
async fn visible_keys(
    conn: &mut PgConnection,
    caller: AuthContext,
    ids: &[Uuid],
) -> Result<BTreeMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let scope = db::load_scope(&mut *conn, caller).await?;
    type Row = (Uuid, String, Uuid, Uuid, bool, Uuid, Option<Uuid>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = ANY($1) AND i.archived_at IS NULL",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
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
        .collect())
}

/// Design 06 §3.12: a wiki proposal is approved or rejected as it is; a
/// change is a new proposal of the agent (02's resubmit flow).
fn wiki_not_revisable() -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidRequest,
        "a wiki proposal is not revised: approve it as it is, or reject it and let the agent propose again",
    )
    .with_details(json!({"reason": "wiki_proposal"}))
    .into()
}

/// The project of a stored wiki proposal (its body names it by id);
/// `None` for an issue document.
fn wiki_project(body: &Json) -> Option<Uuid> {
    if !crate::wiki::is_wiki_doc(body) {
        return None;
    }
    body["ops"][0]["target"]["project"]
        .as_str()
        .and_then(|s| s.parse().ok())
}

/// Design 06 §3.12: who may read a wiki proposal. A person while they read
/// that wiki (R-W1); an agent while R-AG1 holds for it. `None` for an issue
/// document (its issues decide, [`readable_targets`]).
async fn wiki_readable(
    conn: &mut PgConnection,
    caller: AuthContext,
    body: &Json,
) -> Result<Option<bool>> {
    if !crate::wiki::is_wiki_doc(body) {
        return Ok(None);
    }
    let Some(project) = wiki_project(body) else {
        return Ok(Some(false));
    };
    if caller.context_id().is_some() {
        return match crate::wiki::agent::agent_access(
            &mut *conn,
            caller,
            &project.to_string(),
            false,
        )
        .await
        {
            Ok(_) => Ok(Some(true)),
            Err(e) if classify(&e).is_some() => Ok(Some(false)),
            Err(e) => Err(e),
        };
    }
    Ok(Some(
        crate::wiki::access_of(&mut *conn, caller, project)
            .await?
            .is_some(),
    ))
}

/// Existing target issues of a stored document.
fn body_targets(body: &Json) -> Vec<Uuid> {
    body["preconditions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["target"]["type"] == "issue")
        .filter_map(|p| p["target"]["id"].as_str()?.parse().ok())
        .collect()
}

/// `caller` can see every target of a proposal body (forms impact).
pub(crate) async fn targets_readable(
    conn: &mut PgConnection,
    caller: AuthContext,
    body: &Json,
) -> Result<bool> {
    Ok(readable_targets(conn, caller, body).await?.is_some())
}

/// Design 05 §3.12.5 stats: `agent`'s ChangeSets the caller can read in
/// full (the destination project and every target of the current version):
/// the pending ones, or (with `chain_since`) the chain reviews since then.
pub(crate) async fn count_readable(
    conn: &mut PgConnection,
    caller: AuthContext,
    agent: Uuid,
    chain_since: Option<DateTime<Utc>>,
) -> Result<i64> {
    let rows: Vec<(Uuid, i32, Uuid)> = sqlx::query_as(
        "SELECT id, current_version, context_project_id FROM changesets
         WHERE proposer_id = $1
           AND CASE WHEN $2::timestamptz IS NULL THEN state = 'proposed'
                    ELSE chain_review IS NOT NULL AND created_at >= $2 END",
    )
    .bind(agent)
    .bind(chain_since)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let scope = db::load_scope(&mut *conn, caller).await?;
    let projects: std::collections::BTreeSet<Uuid> = scope.project_ids().collect();
    let mut n = 0;
    for (id, version, project) in rows {
        if !projects.contains(&project) {
            continue;
        }
        let Some(v) = load_version(&mut *conn, id, version).await? else {
            continue;
        };
        let Some(body) = v.body else { continue };
        if readable_targets(&mut *conn, caller, &body).await?.is_some() {
            n += 1;
        }
    }
    Ok(n)
}

/// Targets as (id, key) when `caller` can see every one; `None` otherwise.
async fn readable_targets(
    conn: &mut PgConnection,
    caller: AuthContext,
    body: &Json,
) -> Result<Option<Vec<TargetView>>> {
    // A wiki proposal names no issue: it is readable while its wiki is.
    if let Some(ok) = wiki_readable(&mut *conn, caller, body).await? {
        return Ok(ok.then(Vec::new));
    }
    let ids = body_targets(body);
    let keys = visible_keys(&mut *conn, caller, &ids).await?;
    if ids.iter().any(|i| !keys.contains_key(i)) {
        return Ok(None);
    }
    Ok(Some(
        ids.into_iter()
            .map(|id| TargetView {
                key: keys[&id].clone(),
                id,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------- agent side

struct AgentWho {
    agent: Uuid,
    context: Uuid,
    delegator: Uuid,
    credential: Option<Uuid>,
    /// The built-in AI's run (design 05 §3.13.1); `None` for a token.
    run: Option<Uuid>,
}

impl AgentWho {
    /// The changeset origin of this agent's proposals (B-42).
    fn origin(&self) -> &'static str {
        if self.run.is_some() {
            "builtin_ai"
        } else {
            "external_agent"
        }
    }
}

/// The agent behind `caller`, re-checked now (token, context, client,
/// delegator; §3.7).
async fn agent_who(conn: &mut PgConnection, caller: AuthContext) -> Result<AgentWho> {
    if caller.kind() != PrincipalKind::Agent {
        return Err(
            ApiError::new(ErrorCode::ForbiddenOp, "only an agent proposes")
                .with_details(json!({"reason": "agent_only"}))
                .into(),
        );
    }
    let me = db::effective(&mut *conn, caller).await?;
    let a = me
        .agent
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "not an agent token"))?;
    Ok(AgentWho {
        agent: a.agent_principal_id,
        context: a.context_id,
        delegator: a.delegator_id,
        credential: caller.credential_id(),
        run: caller.run_id(),
    })
}

/// The one project the document writes to (§1: one issue, a new issue, or
/// a duplicate within one project).
/// §6.7 step 7: every `{citation_id, quote}` must verify as `valid`;
/// otherwise `source_unverifiable` listing the others (T-M14).
async fn check_evidence(
    conn: &mut PgConnection,
    caller: AuthContext,
    parsed: &Parsed,
) -> Result<()> {
    let Some(items) = parsed.norm.get("evidence").and_then(Json::as_array) else {
        return Ok(());
    };
    if items.is_empty() {
        return Ok(());
    }
    let mut reader = crate::citations::Reader::new(&mut *conn, caller).await?;
    let mut bad = Vec::new();
    for (i, ev) in items.iter().enumerate() {
        let st = reader
            .verify_exact(
                &mut *conn,
                ev["citation_id"].as_str().unwrap_or_default(),
                ev["quote"].as_str().unwrap_or_default(),
            )
            .await?;
        if st != crate::citations::CitationStatus::Valid {
            bad.push(json!({"index": i, "status": st.as_str()}));
        }
    }
    if bad.is_empty() {
        return Ok(());
    }
    Err(ApiError::new(
        ErrorCode::SourceUnverifiable,
        "some evidence does not verify; use citation ids and quotes exactly as the tools returned them",
    )
    .with_details(json!({"items": bad}))
    .into())
}

/// `seen` (an agent's own write): what the agent reads. An issue or entry
/// outside it is `not_found` exactly like a missing one, before any
/// project is compared (M1.11 gate); `None` for a person's revision of a
/// proposal they own.
async fn destination(
    conn: &mut PgConnection,
    parsed: &Parsed,
    seen: Option<&ox_core::visibility::Scope>,
) -> Result<Uuid> {
    // Contract 05 §3.5.4: deleting time is for people only (T-M1-38), refused
    // before any policy.
    if parsed.deletes_time() {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "delete_time_entry is not among an agent's commands",
        )
        .with_details(json!({"path": "ops[0].kind"}))
        .into());
    }
    if let Some(cmd) = parsed.version_cmd() {
        return crate::versions::cmd_project(&mut *conn, cmd)
            .await?
            .ok_or_else(|| api(ErrorCode::NotFound, "no version"));
    }
    let mut projects: BTreeSet<Uuid> = parsed.creates().iter().map(|c| c.1).collect();
    let targets = parsed.targets();
    if let Some(scope) = seen {
        let found = crate::time_entries::seen_issue_projects(&mut *conn, &targets, scope).await?;
        if let Some(unseen) = targets.iter().find(|t| !found.contains_key(t)) {
            return Err(parsed.issue_not_found(*unseen));
        }
        projects.extend(found.values().copied());
        if parsed.has_time() {
            projects.extend(
                crate::time_entries::cmd_projects_seen(&mut *conn, &parsed.time_cmds(), scope)
                    .await?,
            );
        }
        return one_project(projects);
    }
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, project_id FROM issues WHERE id = ANY($1) AND archived_at IS NULL",
    )
    .bind(&targets)
    .fetch_all(&mut *conn)
    .await?;
    if rows.len() != targets.len() {
        return Err(api(ErrorCode::NotFound, "no such issue"));
    }
    projects.extend(rows.iter().map(|r| r.1));
    if parsed.has_time() {
        projects.extend(crate::time_entries::cmd_projects(&mut *conn, &parsed.time_cmds()).await?);
    }
    one_project(projects)
}

fn one_project(projects: BTreeSet<Uuid>) -> Result<Uuid> {
    let mut it = projects.into_iter();
    match (it.next(), it.next()) {
        (Some(p), None) => Ok(p),
        (Some(_), Some(_)) => Err(ApiError::new(
            ErrorCode::RelationNotAllowed,
            "a proposal writes to one project",
        )
        .with_details(json!({"reason": "cross_project"}))
        .into()),
        _ => Err(api(ErrorCode::InvalidRequest, "nothing to do")),
    }
}

/// The new source snapshot contains the old one (R only grows, §3.8 M5).
async fn sources_kept(conn: &mut PgConnection, cs: &CsRow, sources: &[(Uuid, i64)]) -> Result<()> {
    let lost: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM changeset_version_source_projects s
                        WHERE s.changeset_id = $1 AND s.version = $2
                          AND s.project_id <> ALL($3))",
    )
    .bind(cs.id)
    .bind(cs.current_version)
    .bind(sources.iter().map(|s| s.0).collect::<Vec<_>>())
    .fetch_one(&mut *conn)
    .await?;
    if lost {
        return Err(quarantined());
    }
    Ok(())
}

fn quarantined() -> anyhow::Error {
    crate::agents::quarantined()
}

fn context_closed() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ContextUnavailable,
        "this work context is closed; the delegator can start a new one",
    )
    .with_details(json!({"kind": "closed"}))
    .into()
}

fn delegation_gone() -> anyhow::Error {
    api(
        ErrorCode::DelegationUnavailable,
        "the delegation for this project is no longer usable",
    )
}

/// The runtime row, then the work context, FOR SHARE (design 05 §3.12.2
/// lock order); the context must be active.
async fn lock_context_row(conn: &mut PgConnection, context: Uuid) -> Result<()> {
    sqlx::query(
        "SELECT 1 FROM agent_runtimes WHERE id = (SELECT runtime_id FROM agent_work_contexts WHERE id = $1)
         FOR SHARE",
    )
    .bind(context)
    .execute(&mut *conn)
    .await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM agent_work_contexts WHERE id = $1 FOR SHARE")
            .bind(context)
            .fetch_optional(&mut *conn)
            .await?;
    if state.as_deref() != Some("active") {
        return Err(context_closed());
    }
    Ok(())
}

/// R and the destination projects FOR SHARE (FOR UPDATE where an issue is
/// created) in UUID order, every epoch of R as recorded (§3.8 M7). Returns
/// the current source snapshot.
async fn lock_sources(
    conn: &mut PgConnection,
    context: Uuid,
    dest: &BTreeMap<Uuid, bool>,
) -> Result<Vec<(Uuid, i64)>> {
    let r: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT project_id, security_epoch FROM agent_context_source_projects
         WHERE context_id = $1 ORDER BY project_id",
    )
    .bind(context)
    .fetch_all(&mut *conn)
    .await?;
    let mut all: BTreeSet<Uuid> = r.iter().map(|x| x.0).collect();
    all.extend(dest.keys().copied());
    for p in all {
        let sql = if dest.get(&p) == Some(&true) {
            "SELECT security_epoch FROM projects WHERE id = $1 FOR UPDATE"
        } else {
            "SELECT security_epoch FROM projects WHERE id = $1 FOR SHARE"
        };
        let epoch: Option<i64> = sqlx::query_scalar(sql)
            .bind(p)
            .fetch_optional(&mut *conn)
            .await?;
        let Some(epoch) = epoch else {
            return Err(api(ErrorCode::NotFound, "no project"));
        };
        if r.iter().any(|(rp, re)| *rp == p && *re != epoch) {
            return Err(quarantined());
        }
    }
    fault("propose.after_sources").await?;
    Ok(r)
}

/// Step 1 for propose / revise: context, delegation for `dest`, R.
async fn agent_step1(
    conn: &mut PgConnection,
    who: &AgentWho,
    dest: Uuid,
    creates: bool,
) -> Result<(DelegRow, Vec<(Uuid, i64)>)> {
    lock_context_row(&mut *conn, who.context).await?;
    // §16.5: principals (id order) and the credential FOR SHARE, still valid.
    let mut ids = vec![who.agent, who.delegator];
    ids.sort();
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT id, status FROM principals WHERE id = ANY($1) ORDER BY id FOR SHARE) x
         WHERE status = 'active'",
    )
    .bind(&ids)
    .fetch_one(&mut *conn)
    .await?;
    if active != 2 {
        return Err(delegation_gone());
    }
    if let Some(c) = who.credential {
        let ok: Option<bool> = sqlx::query_scalar(
            "SELECT revoked_at IS NULL AND expires_at > clock_timestamp() FROM agent_credentials
             WHERE id = $1 AND context_id = $2 AND agent_principal_id = $3 FOR SHARE",
        )
        .bind(c)
        .bind(who.context)
        .bind(who.agent)
        .fetch_optional(&mut *conn)
        .await?;
        if ok != Some(true) {
            return Err(api(
                ErrorCode::Unauthenticated,
                "the credentials are no longer valid",
            ));
        }
    }
    let d: Option<DelegRow> = sqlx::query_as(
        "SELECT id, project_id, proposal_ops, writable_fields, max_ops, expires_at, policy_version
         FROM delegations
         WHERE context_id = $1 AND project_id = $2 AND agent_principal_id = $3 AND revoked_at IS NULL
           AND expires_at > clock_timestamp() FOR SHARE",
    )
    .bind(who.context)
    .bind(dest)
    .bind(who.agent)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(d) = d else {
        let ever: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM delegations WHERE context_id = $1 AND project_id = $2
                              AND agent_principal_id = $3)",
        )
        .bind(who.context)
        .bind(dest)
        .bind(who.agent)
        .fetch_one(&mut *conn)
        .await?;
        return Err(if ever {
            delegation_gone()
        } else {
            api(ErrorCode::NotFound, "no project")
        });
    };
    let sources = lock_sources(&mut *conn, who.context, &BTreeMap::from([(dest, creates)])).await?;
    if !crate::agents::audience_consistent(&mut *conn, dest).await? {
        return Err(delegation_gone());
    }
    Ok((d, sources))
}

/// `changeset_propose` (§8.4, §6.7 propose steps 1–10) for the agent
/// `caller`. `spa_origin` is the canonical SPA origin for `review_url`.
pub async fn propose(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    proposal: &Json,
    spa_origin: &str,
) -> Result<ProposeResult> {
    propose_in_group(app, caller, operation_id, proposal, None, spa_origin).await
}

/// Which group a new proposal joins (§14.3): a label (a group of this
/// delegation and project with that label from the last 24 hours, or a new
/// one) or the id of such a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRef {
    Label(String),
    Id(Uuid),
}

/// `changeset_propose` with `group_label` or `group_id`.
pub async fn propose_in_group(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    proposal: &Json,
    group: Option<&GroupRef>,
    spa_origin: &str,
) -> Result<ProposeResult> {
    agent_write(
        app,
        caller,
        operation_id,
        proposal,
        None,
        None,
        group,
        spa_origin,
    )
    .await
    .map(SubmitResult::into_proposal)
}

/// `command_submit` (§16.4): the whole document is written directly when
/// every command and field in it is `direct` and `require_review` is false;
/// a ChangeSet when anything is `review` (or review was asked for); refused
/// when anything is `off`.
pub async fn command_submit(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    command: &Json,
    require_review: bool,
    spa_origin: &str,
) -> Result<SubmitResult> {
    agent_write(
        app,
        caller,
        operation_id,
        command,
        None,
        Some(require_review),
        None,
        spa_origin,
    )
    .await
}

/// The answer of `command_submit` (§16.4, a discriminated union).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SubmitResult {
    Applied {
        execution_id: Uuid,
        /// Issues the agent can read now; others are left out (§8.8 filter).
        effects: Vec<commands::Effect>,
        /// A version document's result (§17.9a).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        versions: Vec<commands::VersionEffect>,
        /// Time entries written (ids and lock versions only, contract 05
        /// §3.5.4: the fields are read with `time_list`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        time_entries: Vec<crate::time_entries::TimeEntryEffect>,
        /// A wiki document's page (design 06 §3.11).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wiki: Vec<crate::wiki::WikiEffect>,
        search: commands::SearchReceipt,
        next_actions: Vec<NextAction>,
    },
    ReviewRequired {
        /// `policy` or `requested`.
        reason: String,
        #[serde(flatten)]
        proposal: ProposeResult,
    },
}

impl SubmitResult {
    fn into_proposal(self) -> ProposeResult {
        match self {
            SubmitResult::ReviewRequired { proposal, .. } => proposal,
            SubmitResult::Applied { .. } => unreachable!("propose never writes directly"),
        }
    }
}

/// `changeset_revise`: a complete new document for `changeset_id`, based on
/// `expected_current_version`.
pub async fn revise(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    changeset_id: Uuid,
    expected_current_version: i32,
    proposal: &Json,
    spa_origin: &str,
) -> Result<ProposeResult> {
    agent_write(
        app,
        caller,
        operation_id,
        proposal,
        Some((changeset_id, expected_current_version)),
        None,
        None,
        spa_origin,
    )
    .await
    .map(SubmitResult::into_proposal)
}

macro_rules! retrying {
    ($app:expr, $tx:ident, $body:expr) => {{
        let mut attempt = 0;
        loop {
            let mut $tx = $app.pool.begin().await?;
            sqlx::query("SET LOCAL lock_timeout = '10s'")
                .execute(&mut *$tx)
                .await?;
            match $body {
                Ok(v) => {
                    $tx.commit().await?;
                    break Ok(v);
                }
                Err(e) => {
                    drop($tx);
                    if commands::is_retryable_db(&e) && attempt < RETRIES {
                        attempt += 1;
                        continue;
                    }
                    if commands::is_lock_timeout(&e) {
                        break Err(api(
                            ErrorCode::TemporarilyUnavailable,
                            "the proposal is busy; send the same request again",
                        ));
                    }
                    break Err(e);
                }
            }
        }
    }};
}

#[allow(clippy::too_many_arguments)]
async fn agent_write(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    proposal: &Json,
    revise: Option<(Uuid, i32)>,
    submit: Option<bool>,
    group: Option<&GroupRef>,
    spa_origin: &str,
) -> Result<SubmitResult> {
    if let Some(GroupRef::Label(l)) = group {
        check_group_label(l)?;
    }
    quota::check_raw_payload(proposal)?;
    let r = retrying!(
        app,
        tx,
        agent_write_tx(
            &mut tx,
            caller,
            operation_id,
            proposal,
            revise,
            submit,
            group,
            spa_origin
        )
        .await
    );
    // §16.6: the breaker's state commits although the request rolled back.
    let Err(e) = r else { return r };
    if let Some(t) = e.downcast_ref::<quota::TripBreaker>() {
        quota::trip(&app.pool, t.context, t.reason).await?;
        return Err(quota::suspended(t.reason));
    }
    if classify(&e).is_some_and(|a| a.code == ErrorCode::AudienceExpansionForbidden)
        && let (Some(agent), Some(context)) = (caller.principal_id(), caller.context_id())
        && quota::audience_blocked(&app.pool, agent, context).await?
    {
        return Err(quota::suspended("audience_blocked"));
    }
    Err(e)
}

#[allow(clippy::too_many_arguments)]
async fn agent_write_tx(
    conn: &mut PgConnection,
    caller: AuthContext,
    operation_id: Uuid,
    proposal: &Json,
    revise: Option<(Uuid, i32)>,
    submit: Option<bool>,
    group: Option<&GroupRef>,
    spa_origin: &str,
) -> Result<SubmitResult> {
    let who = agent_who(&mut *conn, caller).await?;
    // Design 06 §3.11: a wiki document has its own path (one op, one page).
    if crate::wiki::is_wiki_doc(proposal) {
        return agent_wiki_tx(
            &mut *conn,
            caller,
            &who,
            operation_id,
            proposal,
            revise,
            submit,
            group,
            spa_origin,
        )
        .await;
    }
    // Steps 2–4.
    let action = match (revise, submit) {
        (Some(_), _) => ACTION_REVISE,
        (None, Some(_)) => ACTION_SUBMIT,
        (None, None) => ACTION_PROPOSE,
    };
    // §16.5: the fingerprint covers require_review.
    let review_flag = [submit.map(|r| 1 + r as u8).unwrap_or(0)];
    let fp_of = |parsed: &commands::Parsed| -> [u8; 32] {
        let payload: [u8; 32] = Sha256::digest(
            [
                &parsed.hash[..],
                who.context.as_bytes(),
                revise.map(|r| r.0).unwrap_or_default().as_bytes(),
                &revise.map(|r| r.1).unwrap_or(0).to_be_bytes(),
                &review_flag[..],
                // The group is part of the request (not of the proposal hash).
                match group {
                    None => b"".as_slice(),
                    Some(GroupRef::Label(l)) => l.as_bytes(),
                    Some(GroupRef::Id(_)) => b"\0id".as_slice(),
                },
                match group {
                    Some(GroupRef::Id(g)) => g.as_bytes().as_slice(),
                    _ => b"".as_slice(),
                },
            ]
            .concat(),
        )
        .into();
        commands::fingerprint(action, &payload)
    };
    // A resend of a request the agent already made replays (the replay
    // re-checks visibility), so the visibility cut is for new requests only.
    let (parsed, resent) =
        match commands::parse_for(&mut *conn, proposal, MAX_OPS as usize, caller).await {
            Ok(p) => (p, false),
            Err(e) => {
                let again = commands::resent(
                    &mut *conn,
                    proposal,
                    MAX_OPS as usize,
                    who.agent,
                    operation_id,
                    fp_of,
                )
                .await?;
                match again {
                    Some(p) => (p, true),
                    None => return Err(e),
                }
            }
        };
    quota::check_normalized(commands::canonical_json(&parsed.norm).len())?;
    let fp = fp_of(&parsed);

    // A revision locks the ChangeSet first (the order approve-and-apply uses).
    let base = match revise {
        None => None,
        Some((id, expected)) => {
            let cs = load_cs(&mut *conn, id, true).await?.ok_or_else(not_found)?;
            if cs.proposer_id != who.agent || cs.context_id != Some(who.context) {
                return Err(not_found());
            }
            Some((cs, expected))
        }
    };

    // Step 1 (also before a replay: a replay re-authenticates).
    let seen = db::write_scope(&mut *conn, caller).await?;
    let dest = destination(&mut *conn, &parsed, (!resent).then_some(&seen)).await?;
    let (deleg, sources) =
        agent_step1(&mut *conn, &who, dest, !parsed.creates().is_empty()).await?;

    let Some(request_id) =
        commands::open_request(&mut *conn, who.agent, operation_id, action, &fp).await?
    else {
        let r = commands::stored_result(&mut *conn, who.agent, operation_id, action, &fp).await?;
        if r.get("execution_id").is_some() {
            // A direct write: the stored result, visibility re-checked.
            let c =
                commands::replay(&mut *conn, caller, who.agent, operation_id, action, &fp).await?;
            return Ok(SubmitResult::Applied {
                execution_id: c.execution_id,
                effects: c.effects,
                versions: serde_json::from_value(r["versions"].clone()).unwrap_or_default(),
                time_entries: c.time_entries,
                wiki: Vec::new(),
                search: c.search,
                next_actions: Vec::new(),
            });
        }
        let id: Uuid = r["changeset_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(not_found)?;
        let v = r["version"].as_i64().unwrap_or(1) as i32;
        // §16.5: a key that answered review_required keeps that answer.
        let reason = r["reason"].as_str().unwrap_or("requested").to_string();
        let details = r.get("details").filter(|d| !d.is_null()).cloned();
        return agent_view(&mut *conn, caller, id, v, spa_origin)
            .await
            .map(|mut proposal| {
                proposal.details = details;
                SubmitResult::ReviewRequired { reason, proposal }
            });
    };

    // §16.6: a suspended context writes nothing (reads go on).
    quota::check_not_suspended(&mut *conn, who.context).await?;
    // Design 05 §3.12.5: a paused role (or its delegation here) writes
    // nothing, proposals included; reads go on.
    crate::ai_team::check_not_paused(&mut *conn, who.agent, deleg.id).await?;
    // §8.9: two writes per delegation at a time (replays returned above).
    quota::take_proposal_slot(&mut *conn, deleg.id).await?;
    fault("agent_write.after_slot").await?;

    let mut old_ceiling = None;
    if let Some((cs, expected)) = &base {
        if let Some(e) = state_error(cs, None) {
            return Err(e);
        }
        if cs.state == "approved" || cs.state == "executing" {
            return Err(api(
                ErrorCode::ExecutionInProgress,
                "an approved execution is pending; it cannot be revised now",
            ));
        }
        if clock(&mut *conn).await? >= cs.expires_at {
            return Err(api(ErrorCode::ChangesetExpired, "the proposal expired"));
        }
        if *expected != cs.current_version {
            return Err(ApiError::new(
                ErrorCode::VersionSuperseded,
                "the proposal has a newer version",
            )
            .with_details(json!({"current_version": cs.current_version}))
            .into());
        }
        if cs.delegation_id != Some(deleg.id) {
            return Err(delegation_gone());
        }
        let v = load_version(&mut *conn, cs.id, cs.current_version)
            .await?
            .ok_or_else(not_found)?;
        if v.body.as_ref().is_some_and(crate::wiki::is_wiki_doc) {
            return Err(wiki_not_revisable());
        }
        old_ceiling = Some(v.authority_ceiling);
    }

    // Step 6: the delegation's max_ops (ops and fields are checked in the simulation).
    if parsed.op_list().len() > deleg.max_ops as usize {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            format!("the delegation allows at most {} ops", deleg.max_ops),
        )
        .with_details(json!({"reason": "max_ops", "max_ops": deleg.max_ops}))
        .into());
    }

    // Step 7: evidence, exact, in the agent's read scope (design 03 §3.6).
    check_evidence(&mut *conn, caller, &parsed).await?;

    // §16.3: the delegation's policies (its row is held FOR SHARE, rule 4).
    // `off` refuses every path; `review` turns a submit into a ChangeSet.
    let decision = policy::load(&mut *conn, deleg.id).await?.decide(&parsed);
    if let Some(e) = decision.refusal() {
        return Err(e);
    }

    // Step 8: the audience check against the current R (§3.8 M8, M9); not stored.
    let source_ids: Vec<Uuid> = sources.iter().map(|s| s.0).collect();
    crate::audience::check(&mut *conn, &parsed, &source_ids, who.delegator).await?;

    // Design 05 §3.12.4: after the policy (an `off` op was refused above),
    // the chain limit can only turn `direct` into `review`.
    let chain = match (submit, &decision) {
        (Some(false), policy::Decision::Direct) => {
            chain_exceeded(&mut *conn, who.agent, &parsed.chain_issues()).await?
        }
        _ => None,
    };
    // Design 05 §3.13.3 (Q3): after the chain limit, a built-in AI run that
    // read the user's time turns `direct` into `review` (never `off`).
    let time_taint = match (submit, &decision, chain, who.run) {
        (Some(false), policy::Decision::Direct, None, Some(run)) => {
            crate::ai::runner::run_time_tainted(&mut *conn, run).await?
        }
        _ => false,
    };
    let reason = match (submit, &decision, chain, time_taint) {
        (Some(false), policy::Decision::Direct, None, false) => {
            return agent_direct(
                &mut *conn, caller, &who, &deleg, &parsed, &sources, request_id,
            )
            .await;
        }
        (Some(false), _, Some(_), _) => "agent_chain",
        (Some(false), policy::Decision::Direct, None, true) => "time_taint",
        (Some(false), _, None, _) => "policy",
        _ => "requested",
    };
    if chain.is_some() {
        quota::log_chain_review(&mut *conn, who.agent, who.context, deleg.id).await?;
    }
    if time_taint {
        quota::log_time_taint_review(&mut *conn, who.agent, who.context, deleg.id).await?;
    }

    // Steps 5, 6 and 9: the simulation as the agent. Rejections are not stored.
    let ectx = EvalCtx {
        mode: Mode::Propose,
        caller,
        ceiling: None,
        deadlines: Vec::new(),
        author: Some((who.agent, Some(who.delegator))),
    };
    // Contract 05 §3.6.3: the rules version is read before the check, so
    // rules published while it runs leave the version looking changed.
    let pre_pairs = crate::forms::doc_pairs(&mut *conn, &parsed.norm).await?;
    let pre_rules = crate::forms::pairs_version(&mut *conn, &pre_pairs).await?;
    let sim = commands::simulate(&mut *conn, &ectx, &parsed).await;
    if let Err(e) = &sim
        && commands::is_rejection(e)
    {
        return Err(sim.err().expect("an error"));
    }
    let checked_at = clock(&mut *conn).await?;
    let validation = stored_validation(&sim, checked_at);

    let ceiling = old_ceiling.unwrap_or_else(|| ceiling_json(&deleg));
    let (cs_id, version, expires_at, edit_method, based_on) = match &base {
        Some((cs, _)) => (
            cs.id,
            cs.current_version + 1,
            cs.expires_at,
            "agent_revise",
            Some(cs.current_version),
        ),
        None => {
            let expires_at: DateTime<Utc> = sqlx::query_scalar(
                "SELECT LEAST(clock_timestamp() + make_interval(days => $1), $2)",
            )
            .bind(DEFAULT_DAYS)
            .bind(deleg.expires_at)
            .fetch_one(&mut *conn)
            .await?;
            (Uuid::now_v7(), 1, expires_at, "agent_propose", None)
        }
    };
    if let Some((cs, _)) = &base {
        sources_kept(&mut *conn, cs, &sources).await?;
    }
    quota::reserve_proposal(&mut *conn, who.agent, deleg.id, base.is_none()).await?;
    quota::record_proposal(&mut *conn, who.agent, who.context, deleg.id).await?;
    let group_id = match (group, &base) {
        (Some(g), None) => Some(resolve_group(&mut *conn, deleg.id, dest, g).await?),
        (_, Some((cs, _))) => cs.group_id,
        (None, None) => None,
    };
    // G13: what the changed fields held now, as the agent reads them.
    let baseline = crate::review_queue::baseline(&mut *conn, caller, &parsed.norm).await?;
    let stored = StoreVersion {
        origin: who.origin(),
        group_id,
        cs_id,
        version,
        new_changeset: base.is_none(),
        context_project_id: dest,
        proposer: who.agent,
        on_behalf_of: who.delegator,
        delegation: deleg.id,
        context: who.context,
        expires_at,
        ceiling: &ceiling,
        sources: &sources,
        editor: who.agent,
        edit_method,
        based_on,
        validation: &validation,
        rules_version: pre_rules,
        baseline: Some(&baseline),
    };
    let hash = store_version(&mut *conn, &parsed, &stored).await?;
    let details = chain.map(chain_details);
    if let Some(exceeded) = chain {
        // The queue shows why (without numbers) and keeps it out of bulk
        // approval (§3.12.4).
        sqlx::query("UPDATE changesets SET chain_review = $2 WHERE id = $1")
            .bind(cs_id)
            .bind(exceeded)
            .execute(&mut *conn)
            .await?;
    }
    commands::complete_request(
        &mut *conn,
        request_id,
        &json!({"changeset_id": cs_id, "version": version, "reason": reason, "details": details}),
    )
    .await?;

    let targets = visible_keys(&mut *conn, caller, &parsed.targets()).await?;
    let status = validation["status"]
        .as_str()
        .unwrap_or("blocked")
        .to_string();
    let proposal = ProposeResult {
        changeset_id: cs_id,
        version,
        proposal_hash: hash_str(&hash),
        state: "proposed".into(),
        group_id,
        targets: targets
            .into_iter()
            .map(|(id, key)| TargetView { id, key })
            .collect(),
        validation: Some(compact(&validation)),
        content_unavailable: None,
        review_url: format!("{spa_origin}{}", review_path(cs_id, version)),
        expires_at,
        next_actions: next_for(&status),
        details,
    };
    Ok(SubmitResult::ReviewRequired {
        reason: reason.to_string(),
        proposal,
    })
}

/// The direct branch of `command_submit` (§16.5): the same simulation with
/// the row locks, then the write, attributed to the agent on behalf of the
/// delegator (`agent_direct`, §16.2). Step 1 already holds the context,
/// principals, credential, delegation and projects.
async fn agent_direct(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &AgentWho,
    deleg: &DelegRow,
    parsed: &Parsed,
    sources: &[(Uuid, i64)],
    request_id: Uuid,
) -> Result<SubmitResult> {
    let client: Uuid = sqlx::query_scalar(
        "SELECT c.id FROM agent_clients c JOIN agent_principals ap ON ap.agent_client_id = c.id
         WHERE ap.agent_principal_id = $1 AND c.enabled FOR SHARE OF c",
    )
    .bind(who.agent)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(delegation_gone)?;
    let mut deadlines = vec![(deleg.expires_at, ErrorCode::DelegationUnavailable)];
    if let Some(c) = who.credential {
        let exp: DateTime<Utc> =
            sqlx::query_scalar("SELECT expires_at FROM agent_credentials WHERE id = $1")
                .bind(c)
                .fetch_one(&mut *conn)
                .await?;
        deadlines.push((exp, ErrorCode::Unauthenticated));
    }
    // §16.6 counters (the context's control row NOWAIT, the agent row).
    let existing: Vec<Uuid> = parsed.targets();
    let vw = match parsed.version_cmd() {
        Some(commands::VersionCmd::Create { .. }) => quota::VersionWrite {
            create: true,
            update: None,
        },
        Some(commands::VersionCmd::Update { version_id, .. }) => quota::VersionWrite {
            create: false,
            update: Some(version_id),
        },
        None => quota::VersionWrite::default(),
    };
    quota::reserve_direct(
        &mut *conn,
        who.agent,
        who.context,
        &existing,
        parsed.creates().len() as i64,
        vw,
    )
    .await?;
    let logs = parsed
        .time_cmds()
        .iter()
        .filter(|c| matches!(c, crate::time_entries::TimeCmd::Log { .. }))
        .count() as i64;
    quota::reserve_time(&mut *conn, who.agent, logs).await?;
    let ectx = EvalCtx {
        mode: Mode::AgentDirect,
        caller,
        ceiling: None,
        deadlines,
        author: Some((who.agent, Some(who.delegator))),
    };
    let sim = commands::simulate(&mut *conn, &ectx, parsed).await?;
    let consistent_after = sim.token.clone().unwrap_or_default();
    let execution_id = Uuid::now_v7();
    sqlx::query("INSERT INTO execution_refs (id, kind) VALUES ($1, 'command')")
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
    // Design 05 §3.12.4: extend the chains of the existing issues written
    // (rows already locked by `chain_exceeded`); the largest goes to the
    // append-only audit row.
    let mut chain =
        chain_record(&mut *conn, who.agent, execution_id, &parsed.chain_issues()).await?;
    if !parsed.creates().is_empty() {
        chain = Some(chain.map_or((1, 0), |(w, d)| (w.max(1), d)));
    }
    sqlx::query(
        "INSERT INTO command_executions (id, operation_request_id, actor_id, on_behalf_of_id, origin,
                                         delegation_id, context_id, policy_version, command_hash, consistent_after,
                                         agent_chain_writes, agent_chain_depth)
         VALUES ($1, $2, $3, $4, 'agent_direct', $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(execution_id)
    .bind(request_id)
    .bind(who.agent)
    .bind(who.delegator)
    .bind(deleg.id)
    .bind(who.context)
    .bind(deleg.policy_version)
    .bind(&parsed.hash[..])
    .bind(&consistent_after)
    .bind(chain.map(|c| c.0))
    .bind(chain.map(|c| c.1))
    .execute(&mut *conn)
    .await?;
    for (p, e) in sources {
        sqlx::query(
            "INSERT INTO command_execution_source_projects (execution_id, project_id, security_epoch)
             VALUES ($1, $2, $3)",
        )
        .bind(execution_id)
        .bind(p)
        .bind(e)
        .execute(&mut *conn)
        .await?;
    }
    let at = Attribution {
        origin: "agent_direct",
        actor_id: who.agent,
        actor_kind: "agent",
        author_id: who.agent,
        author_on_behalf: Some(who.delegator),
        request_id,
        command_execution_id: Some(execution_id),
        execution_ref: execution_id,
        changeset: None,
        changeset_execution_id: None,
        proposer_id: None,
        approver_id: None,
        on_behalf_of_id: Some(who.delegator),
        delegation_id: Some(deleg.id),
        agent_client_id: Some(client),
        record_changes: true,
    };
    let effects = commands::write_effects(&mut *conn, parsed, &sim, &at).await?;
    let written: Vec<Uuid> = effects.iter().map(|e| e.issue_id).collect();
    let created: Vec<Uuid> = effects
        .iter()
        .filter(|e| e.created)
        .map(|e| e.issue_id)
        .collect();
    // A new issue starts its chain with this write (counted as (1, 0) above).
    chain_record(&mut *conn, who.agent, execution_id, &created).await?;
    quota::record_direct(
        &mut *conn,
        who.agent,
        who.context,
        deleg.id,
        &written,
        &created,
        sim.version.as_ref().map(|p| (p.id, p.created)),
    )
    .await?;
    quota::record_time(&mut *conn, who.agent, who.context, deleg.id, logs).await?;
    let versions: Vec<commands::VersionEffect> = sim.version.iter().map(|p| p.effect()).collect();
    let time_entries = sim.time.as_ref().map(|t| t.effects()).unwrap_or_default();
    commands::complete_request(
        &mut *conn,
        request_id,
        &json!({
            "execution_id": execution_id,
            "consistent_after": consistent_after,
            "versions": versions,
            "time_entries": time_entries,
            "effects": effects.iter().map(commands::stored_effect).collect::<Vec<_>>(),
            "items": sim.items,
        }),
    )
    .await?;
    // The response filter (§8.8): only issues the agent can read now (a new
    // private issue is not among them, T-D10).
    let ids: Vec<Uuid> = effects.iter().map(|e| e.issue_id).collect();
    let visible: BTreeMap<Uuid, String> = visible_keys(&mut *conn, caller, &ids)
        .await?
        .into_iter()
        .collect();
    Ok(SubmitResult::Applied {
        execution_id,
        effects: effects
            .into_iter()
            .filter(|e| visible.contains_key(&e.issue_id))
            .collect(),
        versions,
        time_entries,
        wiki: Vec::new(),
        search: commands::SearchReceipt { consistent_after },
        next_actions: Vec::new(),
    })
}

/// The wiki ops an agent may send (design 06 §3.11).
pub const AGENT_WIKI_OPS: [&str; 2] = ["wiki_put", "wiki_move"];

/// Design 06 §3.11: what an agent may not put in a wiki document, before
/// it is parsed: the other wiki ops are not among its commands
/// (`invalid_request`, as `delete_time_entry`); attachments are for people
/// (`forbidden_op`, A2).
fn agent_wiki_refusal(raw: &Json) -> Option<anyhow::Error> {
    let ops = raw.get("ops").and_then(Json::as_array)?;
    for (i, op) in ops.iter().enumerate() {
        let kind = op.get("kind").and_then(Json::as_str).unwrap_or_default();
        if !AGENT_WIKI_OPS.contains(&kind) {
            return Some(
                ApiError::new(
                    ErrorCode::InvalidRequest,
                    format!("{kind} is not among an agent's commands"),
                )
                .with_details(json!({"path": format!("ops[{i}].kind")}))
                .into(),
            );
        }
        if op.get("attachments").is_some() {
            return Some(
                ApiError::new(ErrorCode::ForbiddenOp, "only people attach files")
                    .with_details(json!({"reason": "attachments_people_only",
                                         "path": format!("ops[{i}].attachments")}))
                    .into(),
            );
        }
    }
    None
}

/// Design 06 R-AG11 (A6 (b)): an external agent whose work context may read
/// the delegator's time (05 A1(c)) sends every wiki write for review; the
/// server keeps no record of what the agent read, and a wiki has no
/// notifications. The whole context counts: the time it read is not tied
/// to the delegation of the project it writes.
async fn reads_own_time(conn: &mut PgConnection, who: &AgentWho) -> Result<bool> {
    if who.run.is_some() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM delegations WHERE context_id = $1 AND agent_principal_id = $2
                          AND read_own_time AND revoked_at IS NULL AND expires_at > clock_timestamp())",
    )
    .bind(who.context)
    .bind(who.agent)
    .fetch_one(&mut *conn)
    .await?)
}

/// `command_submit` (and, from M2.3b's review half, propose and revise)
/// with a wiki document (design 06 §3.11, §3.12): R-AG1 decides the
/// project, then the steps of [`agent_write_tx`] in the same order; a
/// direct write runs the wiki engine as `agent_direct`.
#[allow(clippy::too_many_arguments)]
async fn agent_wiki_tx(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &AgentWho,
    operation_id: Uuid,
    raw: &Json,
    revise: Option<(Uuid, i32)>,
    submit: Option<bool>,
    group: Option<&GroupRef>,
    spa_origin: &str,
) -> Result<SubmitResult> {
    if let Some(e) = agent_wiki_refusal(raw) {
        return Err(e);
    }
    let doc = crate::wiki::parse(raw)?;
    let (kind, project_text) = match &doc.ops[0] {
        crate::wiki::WikiOp::WikiPut(o) => ("wiki_put", o.target.project.clone()),
        crate::wiki::WikiOp::WikiMove(o) => ("wiki_move", o.target.project.clone()),
        _ => unreachable!("refused above"),
    };
    let hash: [u8; 32] = Sha256::digest(commands::canonical_json(raw).as_bytes()).into();
    let (Some(require_review), None, None) = (submit, revise, group) else {
        // M2.3b's review half: wiki proposals through propose / revise.
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "wiki documents go through command_submit",
        )
        .with_details(json!({"path": "ops[0].kind"}))
        .into());
    };
    let fp: [u8; 32] = {
        let payload: [u8; 32] = Sha256::digest(
            [
                &hash[..],
                who.context.as_bytes(),
                Uuid::nil().as_bytes(),
                &0i32.to_be_bytes(),
                &[1 + require_review as u8][..],
            ]
            .concat(),
        )
        .into();
        commands::fingerprint(ACTION_SUBMIT, &payload)
    };

    // R-AG1 (and §6.7 step 1's `not_found`): the project is delegated, its
    // wiki is the delegator's to read, and W(P) ⊇ A(P).
    let (access, delegable) =
        crate::wiki::agent::agent_access_d(&mut *conn, caller, &project_text, false).await?;
    let dest = access.project;
    let (deleg, sources) = agent_step1(&mut *conn, who, dest, false).await?;

    let Some(request_id) =
        commands::open_request(&mut *conn, who.agent, operation_id, ACTION_SUBMIT, &fp).await?
    else {
        let r = commands::stored_result(&mut *conn, who.agent, operation_id, ACTION_SUBMIT, &fp)
            .await?;
        // §16.5: a key that answered review_required keeps that answer.
        if let Some(id) = r["changeset_id"].as_str().and_then(|s| s.parse().ok()) {
            let reason = r["reason"].as_str().unwrap_or("requested").to_string();
            let v = r["version"].as_i64().unwrap_or(1) as i32;
            let proposal = agent_view(&mut *conn, caller, id, v, spa_origin).await?;
            return Ok(SubmitResult::ReviewRequired { reason, proposal });
        }
        return wiki_replay(&mut *conn, caller, &r).await;
    };

    quota::check_not_suspended(&mut *conn, who.context).await?;
    crate::ai_team::check_not_paused(&mut *conn, who.agent, deleg.id).await?;
    quota::take_proposal_slot(&mut *conn, deleg.id).await?;

    // §16.3: the delegation's policy for the op (an existing delegation has
    // no wiki op: `off`, design 06 T-M2-48).
    let decision = wiki_decision(&mut *conn, deleg.id, &deleg.proposal_ops, kind).await?;
    if let Some(e) = decision.refusal() {
        return Err(e);
    }

    // Step 8, R-AG3: W(P) ⊆ ∩ A(R).
    let source_ids: Vec<Uuid> = sources.iter().map(|s| s.0).collect();
    crate::audience::check_wiki(&mut *conn, dest, &source_ids).await?;

    let reason = if require_review {
        Some("requested")
    } else if decision == policy::Decision::Review {
        Some("policy")
    } else if reads_own_time(&mut *conn, who).await? {
        Some("time_read_delegated")
    } else {
        match who.run {
            Some(run) if crate::ai::runner::run_time_tainted(&mut *conn, run).await? => {
                quota::log_time_taint_review(&mut *conn, who.agent, who.context, deleg.id).await?;
                Some("time_taint")
            }
            _ => None,
        }
    };

    let client: Uuid = sqlx::query_scalar(
        "SELECT c.id FROM agent_clients c JOIN agent_principals ap ON ap.agent_client_id = c.id
         WHERE ap.agent_principal_id = $1 AND c.enabled FOR SHARE OF c",
    )
    .bind(who.agent)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(delegation_gone)?;
    let mut deadlines = vec![(deleg.expires_at, ErrorCode::DelegationUnavailable)];
    if let Some(c) = who.credential {
        let exp: DateTime<Utc> =
            sqlx::query_scalar("SELECT expires_at FROM agent_credentials WHERE id = $1")
                .bind(c)
                .fetch_one(&mut *conn)
                .await?;
        deadlines.push((exp, ErrorCode::Unauthenticated));
    }
    let writer = crate::wiki::agent::AgentWriter {
        agent: who.agent,
        delegator: who.delegator,
        delegation: deleg.id,
        client,
        context: who.context,
        policy_version: deleg.policy_version,
        hash,
        sources: sources.clone(),
        deadlines,
        history: delegable.history,
        access,
        review: None,
    };

    if let Some(reason) = reason {
        return wiki_propose(
            &mut *conn, caller, who, &deleg, &sources, &writer, request_id, &doc, kind, reason,
            spa_origin,
        )
        .await;
    }

    // The direct write (§16.5): the counters, then the engine as the agent.
    quota::reserve_direct(
        &mut *conn,
        who.agent,
        who.context,
        &[],
        0,
        quota::VersionWrite::default(),
    )
    .await?;
    let r = crate::wiki::execute(
        &mut *conn,
        caller,
        crate::wiki::Writer::Agent(&writer),
        request_id,
        hash,
        &doc,
    )
    .await?;
    let page = r.wiki[0].page_id;
    // R-AG6: more than 10 writes to one page in 10 minutes trip the breaker.
    quota::wiki_page_burst(&mut *conn, who.agent, who.context, page).await?;
    quota::record_direct(&mut *conn, who.agent, who.context, deleg.id, &[], &[], None).await?;
    quota::record_wiki_write(&mut *conn, who.agent, who.context, deleg.id, page).await?;
    commands::complete_request(
        &mut *conn,
        request_id,
        &json!({
            "execution_id": r.execution_id,
            "consistent_after": r.search.consistent_after,
            "wiki": r.wiki.iter().map(|w| json!({
                "page_id": w.page_id, "project_id": w.project_id, "version": w.version,
                "lock_version": w.lock_version, "changed": w.changed, "created": w.created,
            })).collect::<Vec<_>>(),
        }),
    )
    .await?;
    Ok(SubmitResult::Applied {
        execution_id: r.execution_id,
        effects: Vec::new(),
        versions: Vec::new(),
        time_entries: Vec::new(),
        wiki: r.wiki,
        search: r.search,
        next_actions: Vec::new(),
    })
}

/// §16.3 for a wiki op: `off` unless the delegation holds it (an older
/// delegation has no wiki op, design 06 T-M2-48), else its policy.
async fn wiki_decision(
    conn: &mut PgConnection,
    delegation: Uuid,
    proposal_ops: &[String],
    kind: &str,
) -> Result<policy::Decision> {
    Ok(if proposal_ops.iter().any(|o| o == kind) {
        policy::load(&mut *conn, delegation)
            .await?
            .decide_items(&[(0, kind, Vec::new())])
    } else {
        policy::Decision::Off {
            op_index: 0,
            command: kind.to_string(),
            field: None,
        }
    })
}

/// The trial write of a wiki document (in a savepoint, rolled back): the
/// engine's own checks, nothing kept. Rejections (§6.7: forbidden, not
/// found, invalid) come back as `Err`; a conflict or a block as the inner
/// `Err`.
async fn wiki_trial(
    conn: &mut PgConnection,
    caller: AuthContext,
    writer: &crate::wiki::agent::AgentWriter,
    request: Option<Uuid>,
    doc: &crate::wiki::WikiCommandDoc,
) -> Result<Result<CommandResult>> {
    sqlx::query("SAVEPOINT wiki_trial")
        .execute(&mut *conn)
        .await?;
    // A validation has no request of its own: a throwaway one, rolled back
    // with the rest.
    let request = match request {
        Some(r) => r,
        None => commands::open_request(
            &mut *conn,
            writer.agent,
            Uuid::now_v7(),
            "validate",
            &[0; 32],
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("a fresh operation id is free"))?,
    };
    let r = crate::wiki::execute(
        &mut *conn,
        caller,
        crate::wiki::Writer::Agent(writer),
        request,
        writer.hash,
        doc,
    )
    .await;
    sqlx::query("ROLLBACK TO SAVEPOINT wiki_trial")
        .execute(&mut *conn)
        .await?;
    sqlx::query("RELEASE SAVEPOINT wiki_trial")
        .execute(&mut *conn)
        .await?;
    match r {
        Err(e) if commands::is_rejection(&e) => Err(e),
        r => Ok(r),
    }
}

/// Design 06 §3.12 審閱路徑: a wiki op for review becomes a ChangeSet with
/// the whole document (its project by id), checked by a trial write.
#[allow(clippy::too_many_arguments)]
async fn wiki_propose(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &AgentWho,
    deleg: &DelegRow,
    sources: &[(Uuid, i64)],
    writer: &crate::wiki::agent::AgentWriter,
    request_id: Uuid,
    doc: &crate::wiki::WikiCommandDoc,
    kind: &str,
    reason: &str,
    spa_origin: &str,
) -> Result<SubmitResult> {
    let dest = writer.access.project;
    let trial = wiki_trial(&mut *conn, caller, writer, Some(request_id), doc).await?;
    let checked_at = clock(&mut *conn).await?;
    let validation = wiki_validation(&trial, checked_at);
    let mut body = serde_json::to_value(doc)?;
    body["ops"][0]["target"]["project"] = json!(dest.to_string());
    let op_id = body["ops"][0]["op_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(|| anyhow::anyhow!("a parsed wiki op has its op_id"))?;
    let expires_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT LEAST(clock_timestamp() + make_interval(days => $1), $2)")
            .bind(DEFAULT_DAYS)
            .bind(deleg.expires_at)
            .fetch_one(&mut *conn)
            .await?;
    quota::reserve_proposal(&mut *conn, who.agent, deleg.id, true).await?;
    quota::record_proposal(&mut *conn, who.agent, who.context, deleg.id).await?;
    let cs_id = Uuid::now_v7();
    let ceiling = ceiling_json(deleg);
    let stored = StoreVersion {
        origin: who.origin(),
        group_id: None,
        cs_id,
        version: 1,
        new_changeset: true,
        context_project_id: dest,
        proposer: who.agent,
        on_behalf_of: who.delegator,
        delegation: deleg.id,
        context: who.context,
        expires_at,
        ceiling: &ceiling,
        sources,
        editor: who.agent,
        edit_method: "agent_propose",
        based_on: None,
        validation: &validation,
        rules_version: None,
        baseline: None,
    };
    let hash = store_body(&mut *conn, &body, &[(op_id, kind)], &stored).await?;
    commands::complete_request(
        &mut *conn,
        request_id,
        &json!({"changeset_id": cs_id, "version": 1, "reason": reason, "details": null}),
    )
    .await?;
    let status = validation["status"]
        .as_str()
        .unwrap_or("blocked")
        .to_string();
    Ok(SubmitResult::ReviewRequired {
        reason: reason.to_string(),
        proposal: ProposeResult {
            changeset_id: cs_id,
            version: 1,
            proposal_hash: hash_str(&hash),
            state: "proposed".into(),
            group_id: None,
            targets: Vec::new(),
            validation: Some(compact(&validation)),
            content_unavailable: None,
            review_url: format!("{spa_origin}{}", review_path(cs_id, 1)),
            expires_at,
            next_actions: next_for(&status),
            details: None,
        },
    })
}

/// A resent wiki `command_submit`: the stored result; the page only while
/// the agent may still read that wiki (R-AG1), with its title now.
async fn wiki_replay(
    conn: &mut PgConnection,
    caller: AuthContext,
    r: &Json,
) -> Result<SubmitResult> {
    let mut wiki = Vec::new();
    for e in r["wiki"].as_array().cloned().unwrap_or_default() {
        let (Some(page), Some(project)) = (
            e["page_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()),
            e["project_id"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok()),
        ) else {
            continue;
        };
        if crate::wiki::agent::agent_access(&mut *conn, caller, &project.to_string(), false)
            .await
            .is_err()
        {
            continue;
        }
        let title: String = sqlx::query_scalar("SELECT title FROM wiki_pages WHERE id = $1")
            .bind(page)
            .fetch_one(&mut *conn)
            .await?;
        wiki.push(crate::wiki::WikiEffect {
            page_id: page,
            project_id: project,
            title,
            version: e["version"].as_i64().unwrap_or_default() as i32,
            lock_version: e["lock_version"].as_i64().unwrap_or_default(),
            changed: e["changed"].as_bool().unwrap_or_default(),
            created: e["created"].as_bool().unwrap_or_default(),
            reparented: Vec::new(),
        });
    }
    Ok(SubmitResult::Applied {
        execution_id: r["execution_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default(),
        effects: Vec::new(),
        versions: Vec::new(),
        time_entries: Vec::new(),
        wiki,
        search: commands::SearchReceipt {
            consistent_after: r["consistent_after"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
        next_actions: Vec::new(),
    })
}

/// Design 05 §3.12.4: whether a direct write of `agent` to `issues` would
/// make a chain exceed the limits: `Some("depth")` or `Some("writes")`. The
/// issue rows are locked FOR UPDATE in id order first (the engine's issue
/// lock, taken again by the simulation), so concurrent agents on one issue
/// count in turn (T-M1-97).
async fn chain_exceeded(
    conn: &mut PgConnection,
    agent: Uuid,
    issues: &[Uuid],
) -> Result<Option<&'static str>> {
    if issues.is_empty() {
        return Ok(None);
    }
    // A second line of defence, not the one that keeps the count right
    // (verification M14). Every issue the chain counts is an existing
    // target of the document, and every existing target needs a
    // `lock_version` precondition (`commands::parse`); every effect bumps
    // `lock_version` (02 K8) and the executor checks it on the row it holds
    // FOR UPDATE. So of two concurrent direct writes touching one issue at
    // most one commits, and the chain cannot be counted past its limit
    // without this lock either. The lock only makes the loser wait here and
    // see the new count (an `agent_chain` review) instead of a `conflict`,
    // and keeps the rule true if preconditions ever become optional.
    sqlx::query("SELECT id FROM issues WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(issues)
        .execute(&mut *conn)
        .await?;
    let rows: Vec<(i32, i32, Uuid)> = sqlx::query_as(
        "SELECT writes, depth, last_agent_principal_id FROM issue_agent_chains
         WHERE issue_id = ANY($1) ORDER BY issue_id",
    )
    .bind(issues)
    .fetch_all(&mut *conn)
    .await?;
    let mut writes = false;
    for (w, d, last) in rows {
        if d + i32::from(last != agent) > crate::agents::CHAIN_MAX_DEPTH {
            return Ok(Some("depth"));
        }
        writes |= w + 1 > crate::agents::CHAIN_MAX_WRITES;
    }
    Ok(writes.then_some("writes"))
}

/// The `review_required` details of a chain review (no counts, no agent
/// names: appendix D V21).
fn chain_details(exceeded: &str) -> Json {
    json!({
        "exceeded": exceeded,
        "max_depth": crate::agents::CHAIN_MAX_DEPTH,
        "max_writes": crate::agents::CHAIN_MAX_WRITES,
    })
}

/// After a direct write: extends the chains of the written issues (a new
/// issue starts one). Returns the largest (writes, depth) for the audit
/// columns.
async fn chain_record(
    conn: &mut PgConnection,
    agent: Uuid,
    execution: Uuid,
    issues: &[Uuid],
) -> Result<Option<(i32, i32)>> {
    let mut max: Option<(i32, i32)> = None;
    for issue in issues {
        let (w, d): (i32, i32) = sqlx::query_as(
            "INSERT INTO issue_agent_chains AS c (issue_id, started_at, started_by_execution_id, writes, depth,
                                                  last_agent_principal_id, agents, updated_at)
             VALUES ($1, clock_timestamp(), $2, 1, 0, $3, ARRAY[$3], clock_timestamp())
             ON CONFLICT (issue_id) DO UPDATE SET
                 writes = c.writes + 1,
                 depth = c.depth + (c.last_agent_principal_id <> EXCLUDED.last_agent_principal_id)::int,
                 last_agent_principal_id = EXCLUDED.last_agent_principal_id,
                 agents = CASE WHEN EXCLUDED.last_agent_principal_id = ANY(c.agents) OR cardinality(c.agents) >= 20
                               THEN c.agents ELSE c.agents || EXCLUDED.last_agent_principal_id END,
                 updated_at = clock_timestamp()
             RETURNING writes, depth",
        )
        .bind(issue)
        .bind(execution)
        .bind(agent)
        .fetch_one(&mut *conn)
        .await?;
        // P3 gap G40: each issue's own chain, for its history (migration 0033).
        sqlx::query(
            "INSERT INTO command_execution_chains (execution_id, issue_id, writes) VALUES ($1, $2, $3)",
        )
        .bind(execution)
        .bind(issue)
        .bind(w)
        .execute(&mut *conn)
        .await?;
        max = Some(match max {
            None => (w, d),
            Some((mw, md)) => (mw.max(w), md.max(d)),
        });
    }
    Ok(max)
}

struct StoreVersion<'a> {
    /// `external_agent`, or `builtin_ai` for the built-in AI (B-42).
    origin: &'a str,
    group_id: Option<Uuid>,
    cs_id: Uuid,
    version: i32,
    new_changeset: bool,
    context_project_id: Uuid,
    proposer: Uuid,
    on_behalf_of: Uuid,
    delegation: Uuid,
    context: Uuid,
    expires_at: DateTime<Utc>,
    ceiling: &'a Json,
    sources: &'a [(Uuid, i64)],
    editor: Uuid,
    edit_method: &'static str,
    based_on: Option<i32>,
    validation: &'a Json,
    /// The effective rules version of the targets, read before the
    /// validation (§3.6.3).
    rules_version: Option<String>,
    /// G13: [`crate::review_queue::baseline`], stored with the body.
    baseline: Option<&'a Json>,
}

/// Step 10: normalizes, hashes and stores a version (and the ChangeSet for
/// version 1). Returns the proposal hash.
async fn store_version(
    conn: &mut PgConnection,
    parsed: &Parsed,
    s: &StoreVersion<'_>,
) -> Result<[u8; 32]> {
    let mut private_defaults = BTreeMap::new();
    for (i, _, tracker) in parsed.creates() {
        let p: bool = sqlx::query_scalar("SELECT private_by_default FROM trackers WHERE id = $1")
            .bind(tracker)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(false);
        private_defaults.insert(i, p);
    }
    let body = canonical_doc(&parsed.norm, &private_defaults)?;
    store_body(conn, &body, &parsed.op_list(), s).await
}

/// [`store_version`] of a body already normalized, with its ops in order
/// (a wiki proposal: one `wiki_put` or `wiki_move`, design 06 §3.12).
async fn store_body(
    conn: &mut PgConnection,
    body: &Json,
    ops: &[(Uuid, &str)],
    s: &StoreVersion<'_>,
) -> Result<[u8; 32]> {
    let body_sha: [u8; 32] = Sha256::digest(commands::canonical_json(body)).into();
    let hash = proposal_hash(&HashInput {
        document: body,
        expires_at: s.expires_at,
        ceiling: s.ceiling,
        origin: s.origin,
        proposer: s.proposer,
        delegation: s.delegation,
        context: s.context,
        sources: s.sources,
        audience_contract_ver: AUDIENCE_CONTRACT_VER,
    });
    let dest_epoch: i64 = s
        .sources
        .iter()
        .find(|x| x.0 == s.context_project_id)
        .map(|x| x.1)
        .ok_or_else(|| anyhow::anyhow!("the destination project is not in the source snapshot"))?;
    if s.new_changeset {
        sqlx::query(
            "INSERT INTO changesets (id, origin, context_project_id, proposer_id, on_behalf_of_id,
                                     delegation_id, context_id, current_version, state, expires_at, group_id)
             VALUES ($1, $9, $2, $3, $4, $5, $6, 1, 'proposed', $7, $8)",
        )
        .bind(s.cs_id)
        .bind(s.context_project_id)
        .bind(s.proposer)
        .bind(s.on_behalf_of)
        .bind(s.delegation)
        .bind(s.context)
        .bind(s.expires_at)
        .bind(s.group_id)
        .bind(s.origin)
        .execute(&mut *conn)
        .await?;
    }
    let content: Uuid = sqlx::query_scalar(
        "INSERT INTO proposal_contents (body, body_sha256, baseline) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(body)
    .bind(&body_sha[..])
    .bind(s.baseline)
    .fetch_one(&mut *conn)
    .await?;
    // Contract 05 §3.6.3: the form rules the version was checked under
    // (changeset_versions is append-only, so it goes in with the row).
    let rules_version = &s.rules_version;
    sqlx::query(
        "INSERT INTO changeset_versions (changeset_id, version, content_id, proposal_hash,
                                         command_schema_version, exec_mode, expires_at,
                                         authority_ceiling, security_epoch, editor_id, edit_method,
                                         based_on_version, validation, audience_contract_ver,
                                         rules_version)
         VALUES ($1, $2, $3, $4, $5, 'atomic', $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(s.cs_id)
    .bind(s.version)
    .bind(content)
    .bind(&hash[..])
    .bind(COMMAND_SCHEMA_VERSION)
    .bind(s.expires_at)
    .bind(s.ceiling)
    .bind(dest_epoch)
    .bind(s.editor)
    .bind(s.edit_method)
    .bind(s.based_on)
    .bind(s.validation)
    .bind(AUDIENCE_CONTRACT_VER)
    .bind(rules_version)
    .execute(&mut *conn)
    .await?;
    for (p, e) in s.sources {
        sqlx::query(
            "INSERT INTO changeset_version_source_projects (changeset_id, version, project_id, security_epoch)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(s.cs_id)
        .bind(s.version)
        .bind(p)
        .bind(e)
        .execute(&mut *conn)
        .await?;
    }
    for (pos, (op_id, kind)) in ops.iter().enumerate() {
        sqlx::query(
            "INSERT INTO changeset_version_ops (changeset_id, version, op_id, position, kind)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(s.cs_id)
        .bind(s.version)
        .bind(*op_id)
        .bind(pos as i32)
        .bind(*kind)
        .execute(&mut *conn)
        .await?;
    }
    if !s.new_changeset {
        sqlx::query("UPDATE changesets SET current_version = $2, state = 'proposed' WHERE id = $1")
            .bind(s.cs_id)
            .bind(s.version)
            .execute(&mut *conn)
            .await?;
    }
    Ok(hash)
}

/// The compact view of an agent's own version (idempotent replays).
async fn agent_view(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: Uuid,
    version: i32,
    spa_origin: &str,
) -> Result<ProposeResult> {
    let cs = load_cs(&mut *conn, id, false)
        .await?
        .ok_or_else(not_found)?;
    let v = load_version(&mut *conn, id, version)
        .await?
        .ok_or_else(not_found)?;
    let (targets, all_readable) = match &v.body {
        Some(b) => {
            let ids = body_targets(b);
            let k = visible_keys(&mut *conn, caller, &ids).await?;
            let all = ids.iter().all(|i| k.contains_key(i))
                && wiki_readable(&mut *conn, caller, b).await?.unwrap_or(true);
            (k, all)
        }
        None => (BTreeMap::new(), false),
    };
    // The §8.8 filter: validation only while every target is readable.
    let validation = all_readable
        .then(|| v.validation.as_ref().map(compact))
        .flatten();
    let next_actions = match &validation {
        Some(c) => next_for(&c.status),
        None => vec![NextAction::AskHuman],
    };
    let content_unavailable = if v.body.is_none() {
        Some(v.content_state.clone())
    } else if !all_readable {
        Some("not_readable".to_string())
    } else {
        None
    };
    Ok(ProposeResult {
        changeset_id: id,
        version,
        proposal_hash: hash_str(&v.proposal_hash),
        state: cs.state.clone(),
        group_id: cs.group_id,
        targets: targets
            .into_iter()
            .map(|(id, key)| TargetView { id, key })
            .collect(),
        validation,
        content_unavailable,
        review_url: format!("{spa_origin}{}", review_path(id, version)),
        expires_at: cs.expires_at,
        next_actions,
        details: None,
    })
}

// ---------------------------------------------------------------- groups (§14.3)

/// §14.3: at most 80 characters and 320 bytes, no line breaks, control or
/// bidirectional formatting characters (A15).
fn check_group_label(l: &str) -> Result<()> {
    let bad_char = |c: char| {
        c.is_control()
            || matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{061C}' | '\u{200E}' | '\u{200F}'
            )
            || ('\u{202A}'..='\u{202E}').contains(&c)
            || ('\u{2066}'..='\u{2069}').contains(&c)
    };
    let n = l.chars().count();
    if l.trim().is_empty() || n > 80 || l.len() > 320 || l.chars().any(bad_char) {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            "group_label is 1–80 characters (at most 320 bytes) on one line, without control characters",
        )
        .with_details(json!({"path": "group_label"}))
        .into());
    }
    Ok(())
}

/// The group a new proposal joins. A label reuses a group of the same
/// delegation and project from the last 24 hours (concurrent first uses of
/// one label wait on an advisory lock, so they make one group); an id must
/// name such a group.
async fn resolve_group(
    conn: &mut PgConnection,
    delegation: Uuid,
    project: Uuid,
    g: &GroupRef,
) -> Result<Uuid> {
    match g {
        GroupRef::Id(id) => {
            let ok: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM changeset_groups WHERE id = $1 AND delegation_id = $2 AND project_id = $3
                   AND created_at > clock_timestamp() - interval '24 hours'",
            )
            .bind(id)
            .bind(delegation)
            .bind(project)
            .fetch_optional(&mut *conn)
            .await?;
            ok.ok_or_else(|| {
                ApiError::new(
                    ErrorCode::InvalidArgument,
                    "group_id must be a group of this delegation and project from the last 24 hours",
                )
                .with_details(json!({"path": "group_id"}))
                .into()
            })
        }
        GroupRef::Label(label) => {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("oxledger/group/{delegation}/{project}/{label}"))
                .execute(&mut *conn)
                .await?;
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM changeset_groups WHERE delegation_id = $1 AND project_id = $2 AND label = $3
                   AND label_state = 'available' AND created_at > clock_timestamp() - interval '24 hours'
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(delegation)
            .bind(project)
            .bind(label)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(id) = found {
                return Ok(id);
            }
            Ok(sqlx::query_scalar(
                "INSERT INTO changeset_groups (delegation_id, project_id, security_epoch, label)
                 SELECT $1, $2, security_epoch, $3 FROM projects WHERE id = $2 RETURNING id",
            )
            .bind(delegation)
            .bind(project)
            .bind(label)
            .fetch_one(&mut *conn)
            .await?)
        }
    }
}

// ---------------------------------------------------------------- agent reads (§8.8)

/// A proposal as its agent sees it now: the one response filter of §8.8
/// (changeset_get, changeset_list, operation_get, changeset_validate).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentChangeset {
    pub changeset_id: Uuid,
    pub state: String,
    pub current_version: i32,
    pub version: i32,
    pub edit_method: String,
    /// Only for the agent's own versions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_hash: Option<String>,
    /// The document: only the agent's own version, every target readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Json>,
    /// `owner_revised`, `not_readable`, or why the content is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_unavailable: Option<String>,
    /// Target issues the agent can read now.
    pub targets: Vec<TargetView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<CompactValidation>,
    pub group_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_label: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub review_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_execution: Option<AgentExecution>,
    /// Always true: a person approves in the web app; agents cannot apply.
    pub requires_human_approval: bool,
    pub next_actions: Vec<NextAction>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentExecution {
    pub version: i32,
    /// `queued`, `running`, `succeeded` or `failed` (no error code, §8.8).
    pub status: String,
    /// Per op, only for the agent's own version with readable targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<AgentItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consistent_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentItem {
    pub op_id: Uuid,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentChangesetRow {
    pub changeset_id: Uuid,
    pub state: String,
    pub current_version: i32,
    pub targets: Vec<TargetView>,
    pub group_id: Option<Uuid>,
    pub last_execution_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentChangesetList {
    pub items: Vec<AgentChangesetRow>,
}

/// `changeset_validate` for the agent: its own version is run again;
/// an owner's revision shows only that it exists.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentValidation {
    pub version: i32,
    pub state: String,
    pub edit_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_unavailable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<Validation>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AgentOperation {
    pub operation_id: Uuid,
    pub action: String,
    /// `in_progress`, `completed` or `failed`.
    pub status: String,
    /// For a proposal (or a submit that needed review).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeset: Option<AgentChangeset>,
    /// For a direct write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<commands::Effect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<commands::VersionEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consistent_after: Option<String>,
}

fn is_agent_version(edit_method: &str) -> bool {
    matches!(edit_method, "agent_propose" | "agent_revise")
}

async fn delegation_live(conn: &mut PgConnection, d: Option<Uuid>) -> Result<bool> {
    let Some(d) = d else { return Ok(false) };
    Ok(sqlx::query_scalar(
        "SELECT revoked_at IS NULL AND expires_at > clock_timestamp() FROM delegations WHERE id = $1",
    )
    .bind(d)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(false))
}

/// The agent's proposal in its current context, its delegation still usable
/// (§8.8: not even its own proposal or receipt is let through otherwise).
async fn agent_cs(conn: &mut PgConnection, caller: AuthContext, id: Uuid) -> Result<CsRow> {
    let who = agent_who(&mut *conn, caller).await?;
    let cs = load_cs(&mut *conn, id, false)
        .await?
        .filter(|cs| cs.proposer_id == who.agent && cs.context_id == Some(who.context))
        .ok_or_else(not_found)?;
    if !delegation_live(&mut *conn, cs.delegation_id).await? {
        return Err(delegation_gone());
    }
    Ok(cs)
}

/// The §8.8 filter.
async fn filter_for_agent(
    conn: &mut PgConnection,
    caller: AuthContext,
    cs: &CsRow,
    version: i32,
    spa_origin: &str,
) -> Result<AgentChangeset> {
    let ver = load_version(&mut *conn, cs.id, version)
        .await?
        .ok_or_else(not_found)?;
    let own = is_agent_version(&ver.edit_method);
    let ids = ver.body.as_ref().map(body_targets).unwrap_or_default();
    let visible = visible_keys(&mut *conn, caller, &ids).await?;
    // §8.8 (T-M12): issues the version created are targets too.
    let created = created_issues(&mut *conn, cs.id, version).await?;
    let created_visible = visible_keys(&mut *conn, caller, &created).await?;
    let wiki_ok = match &ver.body {
        Some(b) => wiki_readable(&mut *conn, caller, b).await?.unwrap_or(true),
        None => true,
    };
    let all_readable = ids.iter().all(|i| visible.contains_key(i))
        && created.iter().all(|i| created_visible.contains_key(i))
        && wiki_ok;
    let shows_body = own && all_readable && ver.body.is_some();
    let content_unavailable = if !own {
        Some("owner_revised".to_string())
    } else if ver.body.is_none() {
        Some(ver.content_state.clone())
    } else if !all_readable {
        Some("not_readable".to_string())
    } else {
        None
    };
    let validation = shows_body
        .then(|| ver.validation.as_ref().map(compact))
        .flatten();
    let label: Option<String> = match cs.group_id {
        Some(g) => sqlx::query_scalar(
            "SELECT label FROM changeset_groups WHERE id = $1 AND label_state = 'available'",
        )
        .bind(g)
        .fetch_optional(&mut *conn)
        .await?
        .flatten(),
        None => None,
    };
    let last_execution = match latest_exec(&mut *conn, cs.id).await? {
        None => None,
        Some(e) => {
            // §8.8: items of the agent's own version; a key only where
            // the issue is readable (unreadable targets: {op_id, status}).
            let detail = if e.version == version {
                own && ver.body.is_some()
            } else {
                match load_version(&mut *conn, cs.id, e.version).await? {
                    Some(v) => is_agent_version(&v.edit_method) && v.body.is_some(),
                    None => false,
                }
            };
            let items = if detail {
                let stored: Vec<ExecItem> = e
                    .item_results
                    .as_ref()
                    .and_then(|r| serde_json::from_value(r["items"].clone()).ok())
                    .unwrap_or_default();
                let ids: Vec<Uuid> = stored.iter().filter_map(|i| i.issue_id).collect();
                let keys = visible_keys(&mut *conn, caller, &ids).await?;
                Some(
                    stored
                        .into_iter()
                        .map(|i| AgentItem {
                            op_id: i.op_id,
                            status: i.status,
                            key: i.issue_id.and_then(|id| keys.get(&id).cloned()),
                        })
                        .collect(),
                )
            } else {
                None
            };
            Some(AgentExecution {
                version: e.version,
                status: e.status.clone(),
                items,
                consistent_after: e.consistent_after.clone(),
            })
        }
    };
    let next_actions = match (cs.state.as_str(), &validation) {
        ("proposed", Some(v)) => next_for(&v.status),
        ("proposed", None) => vec![NextAction::AskHuman],
        _ => Vec::new(),
    };
    Ok(AgentChangeset {
        changeset_id: cs.id,
        state: cs.state.clone(),
        current_version: cs.current_version,
        version,
        edit_method: ver.edit_method.clone(),
        proposal_hash: own.then(|| hash_str(&ver.proposal_hash)),
        proposal: if shows_body { ver.body.clone() } else { None },
        content_unavailable,
        // The targets come from the version's body: an owner's version
        // hides them too (§8.8).
        targets: if own {
            ids.iter()
                .filter_map(|id| {
                    visible.get(id).map(|k| TargetView {
                        id: *id,
                        key: k.clone(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        },
        validation,
        group_id: cs.group_id,
        group_label: label,
        expires_at: cs.expires_at,
        review_url: format!("{spa_origin}{}", review_path(cs.id, version)),
        last_execution,
        requires_human_approval: true,
        next_actions,
    })
}

/// MCP `changeset_get` (§8.8).
pub async fn agent_get(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    version: Option<i32>,
    spa_origin: &str,
) -> Result<AgentChangeset> {
    let mut tx = read_tx(app).await?;
    let cs = agent_cs(&mut tx, caller, id).await?;
    let v = version.unwrap_or(cs.current_version);
    filter_for_agent(&mut tx, caller, &cs, v, spa_origin).await
}

/// MCP `changeset_list` (§8.8): this context's proposals whose delegation
/// is still usable (others are left out, not an error).
pub async fn agent_list(
    app: &App,
    caller: AuthContext,
    state: Option<&str>,
    group_id: Option<Uuid>,
) -> Result<AgentChangesetList> {
    let mut tx = read_tx(app).await?;
    let who = agent_who(&mut tx, caller).await?;
    let sql = format!(
        "SELECT {CS_COLS} FROM changesets c
         WHERE proposer_id = $1 AND context_id = $2
           AND ($3::text IS NULL OR state = $3) AND ($4::uuid IS NULL OR group_id = $4)
           AND EXISTS (SELECT 1 FROM delegations d WHERE d.id = c.delegation_id
                         AND d.revoked_at IS NULL AND d.expires_at > clock_timestamp())
         ORDER BY created_at DESC, id DESC LIMIT $5"
    );
    let rows: Vec<CsRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(who.agent)
        .bind(who.context)
        .bind(state)
        .bind(group_id)
        .bind(LIST_LIMIT)
        .fetch_all(&mut *tx)
        .await?;
    let mut items = Vec::new();
    for cs in rows {
        let ver = load_version(&mut tx, cs.id, cs.current_version).await?;
        // Only the agent's own versions name their targets (§8.8).
        let ids = ver
            .as_ref()
            .filter(|v| is_agent_version(&v.edit_method))
            .and_then(|v| v.body.as_ref())
            .map(body_targets)
            .unwrap_or_default();
        let keys = visible_keys(&mut tx, caller, &ids).await?;
        let last = latest_exec(&mut tx, cs.id).await?.map(|e| e.status);
        items.push(AgentChangesetRow {
            changeset_id: cs.id,
            state: cs.state,
            current_version: cs.current_version,
            targets: ids
                .iter()
                .filter_map(|id| {
                    keys.get(id).map(|k| TargetView {
                        id: *id,
                        key: k.clone(),
                    })
                })
                .collect(),
            group_id: cs.group_id,
            last_execution_status: last,
        });
    }
    Ok(AgentChangesetList { items })
}

/// MCP `changeset_validate` (§8.5, §8.8).
pub async fn agent_validate(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    version: i32,
) -> Result<AgentValidation> {
    // §8.9: the validate quota, reserved before the trial run.
    {
        let mut c = app.pool.acquire().await?;
        let cs = agent_cs(&mut c, caller, id).await?;
        if let (Some(agent), Some(context), Some(d)) =
            (caller.principal_id(), caller.context_id(), cs.delegation_id)
        {
            quota::reserve_validate(&app.pool, agent, context, d).await?;
        }
    }
    let mut tx = trial_tx(app).await?;
    let cs = agent_cs(&mut tx, caller, id).await?;
    let ver = load_version(&mut tx, id, version)
        .await?
        .ok_or_else(not_found)?;
    if !is_agent_version(&ver.edit_method) {
        return Ok(AgentValidation {
            version,
            state: cs.state,
            edit_method: ver.edit_method,
            content_unavailable: Some("owner_revised".into()),
            validation: None,
        });
    }
    let v = validate_in(&mut tx, caller, &cs, version, &ver).await?;
    Ok(AgentValidation {
        version,
        state: cs.state,
        edit_method: ver.edit_method,
        content_unavailable: None,
        validation: Some(v),
    })
}

/// MCP `validate` (contract 05 §3.6.4): `command_submit`'s checks without
/// writing anything; counts against the validate quota (§8.9, 120/hour).
pub async fn validate_command(
    app: &App,
    caller: AuthContext,
    input: &crate::forms::ValidateCommandInput,
) -> Result<crate::forms::ValidateResult> {
    let raw = crate::forms::command_of(input);
    quota::check_raw_payload(&raw)?;
    if crate::wiki::is_wiki_doc(&raw) {
        // Design 06 §3.11: wiki ops go through command_submit only; a
        // proposal for review carries its own check.
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "validate takes issue documents; command_submit checks a wiki document",
        )
        .with_details(json!({"path": "ops[0].kind"}))
        .into());
    }
    // Step 1 first (who, where, the delegation), then the quota, then the
    // trial run in a transaction that is rolled back.
    let deleg_id = {
        let mut tx = app.pool.begin().await?;
        let who = agent_who(&mut tx, caller).await?;
        let parsed = commands::parse_for(&mut tx, &raw, MAX_OPS as usize, caller).await?;
        let seen = db::write_scope(&mut tx, caller).await?;
        let dest = destination(&mut tx, &parsed, Some(&seen)).await?;
        let (deleg, _) = agent_step1(&mut tx, &who, dest, !parsed.creates().is_empty()).await?;
        tx.rollback().await?;
        quota::reserve_validate(&app.pool, who.agent, who.context, deleg.id).await?;
        deleg.id
    };
    let mut tx = app.pool.begin().await?;
    let who = agent_who(&mut tx, caller).await?;
    let parsed = commands::parse_for(&mut tx, &raw, MAX_OPS as usize, caller).await?;
    let seen = db::write_scope(&mut tx, caller).await?;
    let dest = destination(&mut tx, &parsed, Some(&seen)).await?;
    let (deleg, sources) = agent_step1(&mut tx, &who, dest, !parsed.creates().is_empty()).await?;
    if deleg.id != deleg_id {
        return Err(delegation_gone());
    }
    if parsed.op_list().len() > deleg.max_ops as usize {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            format!("the delegation allows at most {} ops", deleg.max_ops),
        )
        .with_details(json!({"reason": "max_ops", "max_ops": deleg.max_ops}))
        .into());
    }
    let decision = policy::load(&mut tx, deleg.id).await?.decide(&parsed);
    if let Some(e) = decision.refusal() {
        return Err(e);
    }
    let source_ids: Vec<Uuid> = sources.iter().map(|s| s.0).collect();
    crate::audience::check(&mut tx, &parsed, &source_ids, who.delegator).await?;
    let ectx = EvalCtx {
        mode: Mode::Validate,
        caller,
        ceiling: None,
        deadlines: Vec::new(),
        author: Some((who.agent, Some(who.delegator))),
    };
    // Design 05 §3.12.4: the chain limit counts in `would_require_review`.
    let chain = decision == policy::Decision::Direct
        && chain_exceeded(&mut tx, who.agent, &parsed.chain_issues())
            .await?
            .is_some();
    // Design 05 §3.13.3: so does a built-in AI run's time taint.
    let taint = match who.run {
        Some(run) if decision == policy::Decision::Direct && !chain => {
            crate::ai::runner::run_time_tainted(&mut tx, run).await?
        }
        _ => false,
    };
    let sim = commands::simulate(&mut tx, &ectx, &parsed).await;
    let review = decision == policy::Decision::Review || chain || taint;
    let r = crate::forms::outcome(&mut tx, &parsed, sim, review).await;
    tx.rollback().await?;
    r
}

/// MCP `operation_get` (§8.8): what one of the agent's keys did.
pub async fn agent_operation(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    spa_origin: &str,
) -> Result<AgentOperation> {
    let mut tx = read_tx(app).await?;
    let who = agent_who(&mut tx, caller).await?;
    let row: Option<(String, String, Option<Json>, bool)> = sqlx::query_as(
        "SELECT action, status, result_ref, COALESCE(result_expires_at < now(), false)
         FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
    )
    .bind(who.agent)
    .bind(operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((action, status, r, expired)) = row else {
        return Err(api(ErrorCode::NotFound, "no such operation"));
    };
    if expired || (status != "in_progress" && r.is_none()) {
        return Err(api(
            ErrorCode::OperationResultExpired,
            "the result of this operation is no longer kept; it is not run again",
        ));
    }
    let r = r.unwrap_or(Json::Null);
    let uuid = |k: &str| r[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let mut out = AgentOperation {
        operation_id,
        action,
        status,
        changeset: None,
        execution_id: None,
        effects: Vec::new(),
        versions: Vec::new(),
        consistent_after: None,
    };
    if let Some(c) = uuid("changeset_id") {
        let cs = agent_cs(&mut tx, caller, c).await?;
        let v = r["version"]
            .as_i64()
            .map(|v| v as i32)
            .unwrap_or(cs.current_version);
        out.changeset = Some(filter_for_agent(&mut tx, caller, &cs, v, spa_origin).await?);
    } else if let Some(e) = uuid("execution_id") {
        let (deleg, context): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
            "SELECT delegation_id, context_id FROM command_executions WHERE id = $1",
        )
        .bind(e)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((None, None));
        if context != Some(who.context) {
            return Err(api(ErrorCode::NotFound, "no such operation"));
        }
        if !delegation_live(&mut tx, deleg).await? {
            return Err(delegation_gone());
        }
        let stored: Vec<Json> = r["effects"].as_array().cloned().unwrap_or_default();
        let ids: Vec<Uuid> = stored
            .iter()
            .filter_map(|x| x["issue_id"].as_str()?.parse().ok())
            .collect();
        let keys = visible_keys(&mut tx, caller, &ids).await?;
        out.effects = stored
            .iter()
            .filter_map(|x| {
                let id: Uuid = x["issue_id"].as_str()?.parse().ok()?;
                commands::effect_from_stored(x, id, keys.get(&id)?.clone())
            })
            .collect();
        out.versions = serde_json::from_value(r["versions"].clone()).unwrap_or_default();
        out.execution_id = Some(e);
        out.consistent_after = r["consistent_after"].as_str().map(String::from);
    }
    Ok(out)
}

// ---------------------------------------------------------------- owner side

/// The person behind `caller` (any user credential for reads).
fn owner_of(caller: &AuthContext) -> Result<Uuid> {
    caller.user_id().ok_or_else(|| {
        api(
            ErrorCode::HumanSessionRequired,
            "only the delegator, signed in to the web app, reviews proposals",
        )
    })
}

/// The ChangeSet when `user` is its owner; `not_found` otherwise.
async fn load_owned(conn: &mut PgConnection, user: Uuid, id: Uuid, lock: bool) -> Result<CsRow> {
    match load_cs(&mut *conn, id, lock).await? {
        Some(cs) if cs.on_behalf_of_id == user => Ok(cs),
        _ => Err(not_found()),
    }
}

/// An execution as its owner sees it now (issues re-checked for visibility).
async fn exec_view(
    conn: &mut PgConnection,
    caller: AuthContext,
    ex: &ExecRow,
) -> Result<ExecutionView> {
    let stored = ex.item_results.clone().unwrap_or(Json::Null);
    let items: Vec<ExecItem> = serde_json::from_value(stored["items"].clone()).unwrap_or_default();
    let effects: Vec<Json> = stored["effects"].as_array().cloned().unwrap_or_default();
    let mut ids: Vec<Uuid> = effects
        .iter()
        .filter_map(|e| e["issue_id"].as_str()?.parse().ok())
        .collect();
    ids.extend(items.iter().filter_map(|i| i.issue_id));
    ids.sort();
    ids.dedup();
    let visible = visible_keys(&mut *conn, caller, &ids).await?;
    let effects = effects
        .iter()
        .filter_map(|e| {
            let id: Uuid = e["issue_id"].as_str()?.parse().ok()?;
            commands::effect_from_stored(e, id, visible.get(&id)?.clone())
        })
        .collect();
    // A wiki page only while its wiki is the viewer's to read (R-W1).
    let mut wiki_ok: BTreeSet<Uuid> = BTreeSet::new();
    for page in items.iter().filter_map(|i| i.wiki_page_id) {
        let project: Option<Uuid> =
            sqlx::query_scalar("SELECT project_id FROM wiki_pages WHERE id = $1")
                .bind(page)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(p) = project
            && crate::wiki::access_of(&mut *conn, caller, p)
                .await?
                .is_some()
        {
            wiki_ok.insert(page);
        }
    }
    let items = items
        .into_iter()
        .filter(|i| i.issue_id.is_none_or(|id| visible.contains_key(&id)))
        .filter(|i| i.wiki_page_id.is_none_or(|p| wiki_ok.contains(&p)))
        .collect();
    Ok(ExecutionView {
        execution_id: ex.id,
        changeset_id: ex.changeset_id,
        version: ex.version,
        attempt_no: ex.attempt_no,
        status: ex.status.clone(),
        error: ex.error_code.as_ref().map(|c| ErrorView {
            code: c.clone(),
            message: "the execution failed; nothing was changed".into(),
            details: Json::Null,
        }),
        items,
        effects,
        search: ex.consistent_after.clone().map(|c| SearchReceipt {
            consistent_after: c,
        }),
        created_at: ex.created_at,
        finished_at: ex.finished_at,
    })
}

/// Issues created by executions of `version` (from their stored items).
async fn created_issues(conn: &mut PgConnection, cs: Uuid, version: i32) -> Result<Vec<Uuid>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT (it->>'issue_id')::uuid
         FROM changeset_executions e, jsonb_array_elements(e.item_results->'items') it
         WHERE e.changeset_id = $1 AND e.version = $2 AND it ? 'issue_id'",
    )
    .bind(cs)
    .bind(version)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ids)
}

async fn latest_exec(conn: &mut PgConnection, cs: Uuid) -> Result<Option<ExecRow>> {
    let sql = format!(
        "SELECT {EXEC_COLS} FROM changeset_executions WHERE changeset_id = $1
         ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(cs)
        .fetch_optional(&mut *conn)
        .await?)
}

/// `GET /changesets` (§4.2): the caller's proposals, newest first.
/// `bucket = awaiting_me` (default: proposed or failed) or `all`; `state`
/// narrows further.
/// Rows per page of the owner's list (each proposed row is simulated).
const OWNER_PAGE: i64 = 50;

pub async fn list(
    app: &App,
    caller: AuthContext,
    bucket: Option<&str>,
    state: Option<&str>,
    group: Option<Uuid>,
    cursor_text: Option<&str>,
) -> Result<ChangesetList> {
    let user = owner_of(&caller)?;
    let states: Vec<String> = match (bucket.unwrap_or("awaiting_me"), state) {
        (_, Some(s)) => vec![s.to_string()],
        ("awaiting_me", None) => vec!["proposed".into(), "failed".into()],
        ("all", None) => Vec::new(),
        (b, None) => {
            return Err(
                ApiError::new(ErrorCode::InvalidArgument, format!("unknown bucket {b:?}"))
                    .with_details(json!({"path": "bucket", "allowed": ["awaiting_me", "all"]}))
                    .into(),
            );
        }
    };
    let qhash = crate::cursor::query_hash(&json!({"changesets": states, "group": group}));
    let after = match cursor_text {
        None => None,
        Some(c) => {
            match crate::cursor::decode(&app.pool, c, caller.principal_id(), &qhash).await? {
                crate::cursor::Position::Keyset { t, i } => Some((t, i)),
                _ => return Err(api(ErrorCode::InvalidCursor, "invalid cursor")),
            }
        }
    };
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let sql = format!(
        "SELECT {CS_COLS} FROM changesets WHERE on_behalf_of_id = $1
           AND (cardinality($2::text[]) = 0 OR state = ANY($2))
           AND ($4::uuid IS NULL OR group_id = $4)
           AND ($5::timestamptz IS NULL OR (created_at, id) < ($5, $6))
         ORDER BY created_at DESC, id DESC LIMIT $3"
    );
    let mut rows: Vec<CsRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .bind(&states)
        .bind(OWNER_PAGE + 1)
        .bind(group)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .fetch_all(&mut *tx)
        .await?;
    let more = rows.len() as i64 > OWNER_PAGE;
    rows.truncate(OWNER_PAGE as usize);
    let last = rows.last().map(|c| (c.created_at, c.id));
    let mut items = Vec::new();
    for cs in rows {
        let v = load_version(&mut tx, cs.id, cs.current_version).await?;
        let mut diff = Vec::new();
        let mut warnings = Vec::new();
        let mut review_modes = vec!["detail".to_string()];
        let mut rules_changed = false;
        let (title, targets) = match v.as_ref().and_then(|v| v.body.clone()) {
            Some(b) => match readable_targets(&mut tx, caller, &b).await? {
                Some(t) => {
                    if cs.state == "proposed" {
                        let keys: BTreeMap<Uuid, String> =
                            t.iter().map(|x| (x.id, x.key.clone())).collect();
                        diff = crate::review_queue::diff_with_baseline(
                            &mut tx,
                            caller,
                            &b,
                            &keys,
                            v.as_ref().and_then(|v| v.baseline.as_ref()),
                        )
                        .await?;
                        let status = v
                            .as_ref()
                            .and_then(|v| v.validation.as_ref())
                            .and_then(|j| j["status"].as_str())
                            .unwrap_or("blocked")
                            .to_string();
                        let stored = v.as_ref().and_then(|v| v.rules_version.as_deref());
                        let e = crate::review_queue::eligibility_for(
                            &mut tx,
                            caller,
                            &b,
                            &status,
                            stored,
                            cs.chain_review.as_deref(),
                        )
                        .await?;
                        rules_changed = e.reasons.iter().any(|r| r == "rules_changed");
                        warnings = e.warnings.clone();
                        review_modes = e.review_modes();
                    }
                    (b["title"].as_str().map(String::from), t)
                }
                None => (None, Vec::new()),
            },
            None => (None, Vec::new()),
        };
        let validation =
            v.as_ref()
                .and_then(|v| v.validation.as_ref())
                .map(|j| SummaryValidation {
                    status: j["status"].as_str().unwrap_or("blocked").to_string(),
                    checked_at: serde_json::from_value(j["checked_at"].clone()).ok(),
                });
        let last_status = latest_exec(&mut tx, cs.id).await?.map(|e| e.status);
        items.push(ChangesetSummary {
            changeset_id: cs.id,
            state: cs.state,
            current_version: cs.current_version,
            proposal_hash: v.as_ref().map(|v| hash_str(&v.proposal_hash)),
            title,
            targets,
            group_id: cs.group_id,
            created_at: cs.created_at,
            expires_at: cs.expires_at,
            last_execution_status: last_status,
            diff,
            warnings,
            validation,
            review_modes,
            rules_changed,
            agent_chain: cs.chain_review,
        });
    }
    tx.commit().await?;
    let next_cursor = match (more, last) {
        (true, Some((t, i))) => Some(
            crate::cursor::encode(
                &app.pool,
                caller.principal_id(),
                &qhash,
                crate::cursor::Position::Keyset { t, i },
            )
            .await?,
        ),
        _ => None,
    };
    Ok(ChangesetList { items, next_cursor })
}

/// `GET /changesets/{id}` and `/changesets/{id}/versions/{v}`.
pub async fn get(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    version: Option<i32>,
) -> Result<ChangesetView> {
    let user = owner_of(&caller)?;
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let cs = load_owned(&mut tx, user, id, false).await?;
    let v = version.unwrap_or(cs.current_version);
    let ver = load_version(&mut tx, id, v).await?.ok_or_else(not_found)?;
    let (proposal, unavailable, targets) = match &ver.body {
        None => (None, Some(ver.content_state.clone()), Vec::new()),
        Some(b) => match readable_targets(&mut tx, caller, b).await? {
            Some(t) => (Some(b.clone()), None, t),
            None => (None, Some("not_readable".into()), Vec::new()),
        },
    };
    let last = match latest_exec(&mut tx, id).await? {
        Some(e) => Some(exec_view(&mut tx, caller, &e).await?),
        None => None,
    };
    let rules_changed = match &proposal {
        Some(b) => crate::forms::rules_changed(&mut tx, b, ver.rules_version.as_deref()).await?,
        None => false,
    };
    // G12: the queue's diff (same function), with this version's baseline.
    let diff = match &proposal {
        Some(b) => {
            let keys: BTreeMap<Uuid, String> =
                targets.iter().map(|x| (x.id, x.key.clone())).collect();
            crate::review_queue::diff_with_baseline(
                &mut tx,
                caller,
                b,
                &keys,
                ver.baseline.as_ref(),
            )
            .await?
        }
        None => Vec::new(),
    };
    let wiki = match &proposal {
        Some(b) => crate::wiki::proposal_diff(&mut tx, caller, b).await?,
        None => None,
    };
    Ok(ChangesetView {
        diff,
        wiki,
        changeset_id: id,
        state: cs.state,
        current_version: cs.current_version,
        version: v,
        proposal_hash: hash_str(&ver.proposal_hash),
        edit_method: ver.edit_method,
        editor_id: ver.editor_id,
        based_on_version: ver.based_on_version,
        proposer_id: cs.proposer_id,
        on_behalf_of_id: cs.on_behalf_of_id,
        context_project_id: cs.context_project_id,
        created_at: cs.created_at,
        version_created_at: ver.created_at,
        expires_at: cs.expires_at,
        proposal,
        content_unavailable: unavailable,
        validation: ver.validation,
        targets,
        requires_human_approval: true,
        review_url: review_path(id, v),
        last_execution: last,
        rules_changed,
    })
}

/// `POST /changesets/{id}/validations` (§8.5): apply steps 9–14 without
/// locks or writes, as the owner within the proposal's authority ceiling.
pub async fn validate(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    input: &ValidateInput,
) -> Result<Validation> {
    let user = owner_of(&caller)?;
    let mut tx = trial_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let cs = load_owned(&mut tx, user, id, false).await?;
    // Only something still approvable is worth a trial run (§8.5).
    if let Some(e) = state_error(&cs, None) {
        return Err(e);
    }
    if clock(&mut tx).await? >= cs.expires_at {
        return Err(api(ErrorCode::ChangesetExpired, "the proposal expired"));
    }
    if input.version != cs.current_version {
        return Err(ApiError::new(
            ErrorCode::VersionSuperseded,
            "the proposal has a newer version",
        )
        .with_details(json!({"current_version": cs.current_version}))
        .into());
    }
    let ver = load_version(&mut tx, id, input.version)
        .await?
        .ok_or_else(not_found)?;
    validate_in(&mut tx, caller, &cs, input.version, &ver).await
}

/// §8.5 without the owner checks: the state, expiry and version checks,
/// then steps 9–14 as `caller` within the version's ceiling.
async fn validate_in(
    tx: &mut PgConnection,
    caller: AuthContext,
    cs: &CsRow,
    version: i32,
    ver: &VerRow,
) -> Result<Validation> {
    if let Some(e) = state_error(cs, None) {
        return Err(e);
    }
    if clock(&mut *tx).await? >= cs.expires_at {
        return Err(api(ErrorCode::ChangesetExpired, "the proposal expired"));
    }
    if version != cs.current_version {
        return Err(ApiError::new(
            ErrorCode::VersionSuperseded,
            "the proposal has a newer version",
        )
        .with_details(json!({"current_version": cs.current_version}))
        .into());
    }
    let Some(body) = ver.body.clone() else {
        return Err(api(
            ErrorCode::ProposalUnavailable,
            "the proposal content was removed",
        ));
    };
    if readable_targets(&mut *tx, caller, &body).await?.is_none() {
        return Err(api(
            ErrorCode::ProposalUnavailable,
            "you can no longer read every issue this proposal changes",
        ));
    }
    if crate::wiki::is_wiki_doc(&body) {
        return wiki_validate(&mut *tx, caller, cs, version, ver, &body).await;
    }
    let parsed = commands::parse(&mut *tx, &body, MAX_OPS as usize).await?;
    let ectx = EvalCtx {
        mode: Mode::Validate,
        caller,
        ceiling: Some(ceiling_map(&ver.authority_ceiling)),
        deadlines: Vec::new(),
        author: Some((cs.proposer_id, Some(cs.on_behalf_of_id))),
    };
    let sim = commands::simulate(&mut *tx, &ectx, &parsed).await;
    let checked_at = clock(&mut *tx).await?;
    let names = [
        "structure",
        "visibility",
        "permissions",
        "preconditions",
        "workflow",
    ];
    let mut checks: BTreeMap<String, String> = names
        .iter()
        .map(|n| (n.to_string(), "passed".to_string()))
        .collect();
    checks.insert("audience".into(), "not_checked".into());
    let (status, effects, error) = match &sim {
        Ok(s) => (
            "passed",
            s.items
                .iter()
                .map(|i| OpEffect {
                    op_id: i.op_id,
                    status: if i.status == "applied" {
                        "would_change"
                    } else {
                        "no_effect"
                    }
                    .into(),
                })
                .collect(),
            None,
        ),
        Err(e) => {
            let (status, error) = failed_checks(&mut checks, e);
            (status, Vec::new(), Some(error))
        }
    };
    Ok(Validation {
        version,
        proposal_hash: hash_str(&ver.proposal_hash),
        status: status.into(),
        checked_at,
        checks,
        warnings: Vec::new(),
        effects,
        error,
    })
}

const CHECK_NAMES: [&str; 5] = [
    "structure",
    "visibility",
    "permissions",
    "preconditions",
    "workflow",
];

/// §8.5's checks up to the failing one; the status and the error.
fn failed_checks(
    checks: &mut BTreeMap<String, String>,
    e: &anyhow::Error,
) -> (&'static str, ErrorView) {
    let code = classify(e).map(|a| a.code);
    let failed = match code {
        Some(ErrorCode::InvalidRequest) => 0,
        Some(ErrorCode::NotFound) => 1,
        Some(ErrorCode::ForbiddenOp) => 2,
        Some(c) if is_conflict(c) => 3,
        _ => 4,
    };
    for (i, n) in CHECK_NAMES.iter().enumerate() {
        let s = match i.cmp(&failed) {
            std::cmp::Ordering::Less => "passed",
            std::cmp::Ordering::Equal => "failed",
            std::cmp::Ordering::Greater => "not_checked",
        };
        checks.insert(n.to_string(), s.into());
    }
    (
        if code.is_some_and(is_conflict) {
            "conflict"
        } else {
            "blocked"
        },
        error_view(e),
    )
}

/// §8.5 for a wiki proposal: a trial write (rolled back) as the agent for
/// the delegator — through the owner's view of the wiki, or R-AG1's for
/// the agent itself — within the version's sources (R-AG3).
async fn wiki_validate(
    conn: &mut PgConnection,
    caller: AuthContext,
    cs: &CsRow,
    version: i32,
    ver: &VerRow,
    body: &Json,
) -> Result<Validation> {
    let r: Result<CommandResult> = async {
        let doc = crate::wiki::parse(body)?;
        let project = wiki_project(body).ok_or_else(not_found)?;
        let (access, delegable) = if caller.context_id().is_some() {
            crate::wiki::agent::agent_access_d(&mut *conn, caller, &project.to_string(), false)
                .await?
        } else {
            let a = crate::wiki::access_of(&mut *conn, caller, project)
                .await?
                .ok_or_else(not_found)?;
            let d = crate::audience::wiki_delegable(&mut *conn, project).await?;
            if !d.pages {
                return Err(crate::wiki::agent::not_delegable());
            }
            (a, d)
        };
        let sources: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT project_id, security_epoch FROM changeset_version_source_projects
             WHERE changeset_id = $1 AND version = $2 ORDER BY project_id",
        )
        .bind(cs.id)
        .bind(version)
        .fetch_all(&mut *conn)
        .await?;
        let ids: Vec<Uuid> = sources.iter().map(|x| x.0).collect();
        crate::audience::check_wiki(&mut *conn, project, &ids).await?;
        let delegation = cs.delegation_id.ok_or_else(delegation_gone)?;
        let policy_version: i64 =
            sqlx::query_scalar("SELECT policy_version FROM delegations WHERE id = $1")
                .bind(delegation)
                .fetch_one(&mut *conn)
                .await?;
        let client: Uuid = sqlx::query_scalar(
            "SELECT agent_client_id FROM agent_principals WHERE agent_principal_id = $1",
        )
        .bind(cs.proposer_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(delegation_gone)?;
        let writer = crate::wiki::agent::AgentWriter {
            agent: cs.proposer_id,
            delegator: cs.on_behalf_of_id,
            delegation,
            client,
            context: cs.context_id.ok_or_else(not_found)?,
            policy_version,
            hash: ver
                .proposal_hash
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("a proposal hash is 32 bytes"))?,
            sources,
            deadlines: Vec::new(),
            history: delegable.history,
            access,
            review: None,
        };
        wiki_trial(&mut *conn, caller, &writer, None, &doc).await?
    }
    .await;
    let checked_at = clock(&mut *conn).await?;
    let mut checks: BTreeMap<String, String> = CHECK_NAMES
        .iter()
        .map(|n| (n.to_string(), "passed".to_string()))
        .collect();
    checks.insert("audience".into(), "passed".into());
    let (status, effects, error) = match &r {
        Ok(c) => (
            "passed",
            c.items
                .iter()
                .map(|i| OpEffect {
                    op_id: i.op_id,
                    status: if i.status == "applied" {
                        "would_change"
                    } else {
                        "no_effect"
                    }
                    .into(),
                })
                .collect(),
            None,
        ),
        Err(e) => {
            let (status, error) = failed_checks(&mut checks, e);
            (status, Vec::new(), Some(error))
        }
    };
    Ok(Validation {
        version,
        proposal_hash: hash_str(&ver.proposal_hash),
        status: status.into(),
        checked_at,
        checks,
        warnings: Vec::new(),
        effects,
        error,
    })
}

/// A transaction for a trial run: writable (a wiki proposal is checked by
/// a write that is rolled back), never committed.
async fn trial_tx(app: &App) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// The browser session of the owner (decisions record it).
fn session_executor(caller: &AuthContext) -> Result<commands::Executor> {
    let who = commands::executor(caller)?;
    if who.session_id.is_none() {
        return Err(api(
            ErrorCode::HumanSessionRequired,
            "only a person signed in to the web app can decide on proposals",
        ));
    }
    Ok(who)
}

/// A used key of `user`: same content → its request id and `result_ref`.
async fn used_key(
    conn: &mut PgConnection,
    user: Uuid,
    operation_id: Uuid,
    action: &str,
    fp: &[u8; 32],
) -> Result<(Uuid, String, Option<Json>)> {
    let (req, stored_action, stored_fp, status, result_ref, expired): (
        Uuid,
        String,
        Vec<u8>,
        String,
        Option<Json>,
        bool,
    ) = sqlx::query_as(
        "SELECT id, action, fingerprint, status, result_ref, COALESCE(result_expires_at < now(), false)
         FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
    )
    .bind(user)
    .bind(operation_id)
    .fetch_one(&mut *conn)
    .await?;
    if stored_action != action || stored_fp.as_slice() != fp.as_slice() {
        return Err(api(
            ErrorCode::IdempotencyKeyReused,
            "this Idempotency-Key was already used for a different request",
        ));
    }
    if expired {
        return Err(api(
            ErrorCode::OperationResultExpired,
            "the result of this operation is no longer kept; it is not run again",
        ));
    }
    Ok((req, status, result_ref))
}

fn digest(v: &Json) -> [u8; 32] {
    Sha256::digest(commands::canonical_json(v)).into()
}

/// `POST /changesets/{id}/reject`: the current version, by hash. Terminal (U10).
pub async fn reject(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &DecisionInput,
) -> Result<StateView> {
    let who = session_executor(&caller)?;
    let hash = parse_hash(&input.proposal_hash)?;
    let fp = commands::fingerprint(
        ACTION_REJECT,
        &digest(
            &json!({"changeset_id": id, "version": input.version, "proposal_hash": hex::encode(&hash)}),
        ),
    );
    retrying!(app, tx, async {
        let cs = load_owned(&mut tx, who.user_id, id, true).await?;
        if commands::open_request(&mut tx, who.user_id, operation_id, ACTION_REJECT, &fp)
            .await?
            .is_none()
        {
            used_key(&mut tx, who.user_id, operation_id, ACTION_REJECT, &fp).await?;
            return Ok(state_view(&cs));
        }
        let req: Uuid = sqlx::query_scalar(
            "SELECT id FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
        )
        .bind(who.user_id)
        .bind(operation_id)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(e) = state_error(&cs, None) {
            return Err(e);
        }
        if cs.state != "proposed" {
            return Err(api(
                ErrorCode::ChangesetNotExecutable,
                format!("a {} proposal cannot be rejected", cs.state),
            ));
        }
        check_version(&mut tx, &cs, input.version, &hash).await?;
        let deadline = commands::lock_executor(&mut tx, &who).await?;
        commands::check_deadlines(
            &mut tx,
            &deadline
                .map(|d| (d, ErrorCode::Unauthenticated))
                .into_iter()
                .collect::<Vec<_>>(),
        )
        .await?;
        sqlx::query(
            "INSERT INTO changeset_decisions (changeset_id, version, proposal_hash, decider_id, decision, session_id)
             VALUES ($1, $2, $3, $4, 'reject', $5)",
        )
        .bind(id)
        .bind(input.version)
        .bind(&hash)
        .bind(who.user_id)
        .bind(who.session_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE changesets SET state = 'rejected' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        commands::complete_request(&mut tx, req, &json!({"changeset_id": id, "version": input.version}))
            .await?;
        Ok(StateView {
            changeset_id: id,
            state: "rejected".into(),
            current_version: cs.current_version,
        })
    }
    .await)
}

fn state_view(cs: &CsRow) -> StateView {
    StateView {
        changeset_id: cs.id,
        state: cs.state.clone(),
        current_version: cs.current_version,
    }
}

/// Before any decision: the version is current, the hash matches, the
/// content is there and not expired.
async fn check_version(
    conn: &mut PgConnection,
    cs: &CsRow,
    version: i32,
    hash: &[u8],
) -> Result<VerRow> {
    if clock(&mut *conn).await? >= cs.expires_at {
        return Err(api(ErrorCode::ChangesetExpired, "the proposal expired"));
    }
    if version != cs.current_version {
        return Err(ApiError::new(
            ErrorCode::VersionSuperseded,
            "the proposal has a newer version; review it again",
        )
        .with_details(json!({"current_version": cs.current_version}))
        .into());
    }
    let v = load_version(&mut *conn, cs.id, version)
        .await?
        .ok_or_else(not_found)?;
    if v.proposal_hash != hash || v.body.is_none() {
        return Err(api(
            ErrorCode::ApprovalInvalidated,
            "what you reviewed is not this version any more",
        ));
    }
    Ok(v)
}

/// `POST /changesets/{id}/cancel` for the owner, or `changeset_cancel` for
/// the proposing agent: proposed, approved-but-not-started or failed →
/// canceled; a queued execution becomes failed (`changeset_canceled`).
pub async fn cancel(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
) -> Result<StateView> {
    let fp = commands::fingerprint(ACTION_CANCEL, &digest(&json!({"changeset_id": id})));
    retrying!(
        app,
        tx,
        async {
            let (who, is_owner) = match caller.kind() {
                PrincipalKind::Agent => (agent_who(&mut tx, caller).await?.agent, false),
                _ => (owner_of(&caller)?, true),
            };
            let cs = load_cs(&mut tx, id, true).await?.ok_or_else(not_found)?;
            let mine = if is_owner {
                cs.on_behalf_of_id == who
            } else {
                cs.proposer_id == who && cs.context_id == caller.context_id()
            };
            if !mine {
                return Err(not_found());
            }
            if !is_owner {
                lock_context_row(&mut tx, cs.context_id.unwrap_or_default()).await?;
            }
            if commands::open_request(&mut tx, who, operation_id, ACTION_CANCEL, &fp)
                .await?
                .is_none()
            {
                used_key(&mut tx, who, operation_id, ACTION_CANCEL, &fp).await?;
                return Ok(state_view(&cs));
            }
            let req: Uuid = sqlx::query_scalar(
                "SELECT id FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
            )
            .bind(who)
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
            // §8.9: propose + revise + cancel share 60 per delegation per hour.
            if !is_owner && let (Some(d), Some(c)) = (cs.delegation_id, cs.context_id) {
                quota::reserve_proposal(&mut tx, who, d, false).await?;
                quota::record_proposal(&mut tx, who, c, d).await?;
            }
            if cs.state == "succeeded" {
                return Err(api(
                    ErrorCode::ChangesetNotExecutable,
                    "an applied proposal cannot be canceled",
                ));
            }
            if let Some(e) = state_error(&cs, None) {
                return Err(e);
            }
            end_queued(&mut tx, id, "changeset_canceled").await?;
            sqlx::query("UPDATE changesets SET state = 'canceled' WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            commands::complete_request(&mut tx, req, &json!({"changeset_id": id})).await?;
            Ok(StateView {
                changeset_id: id,
                state: "canceled".into(),
                current_version: cs.current_version,
            })
        }
        .await
    )
}

/// Fails the queued executions of a ChangeSet (cancel, expiry) so the
/// one-active-execution index keeps no placeholder (§8.6).
async fn end_queued(conn: &mut PgConnection, id: Uuid, code: &str) -> Result<()> {
    let reqs: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE changeset_executions SET status = 'failed', error_code = $2,
                item_results = '{\"items\": [], \"effects\": []}', finished_at = clock_timestamp()
         WHERE changeset_id = $1 AND status = 'queued' RETURNING operation_request_id",
    )
    .bind(id)
    .bind(code)
    .fetch_all(&mut *conn)
    .await?;
    for r in reqs {
        end_request(&mut *conn, r).await?;
    }
    Ok(())
}

/// The approve request of a failed execution: finished (its result_ref
/// already names the execution).
async fn end_request(conn: &mut PgConnection, request: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE operation_requests SET status = 'failed',
                result_expires_at = now() + make_interval(days => $2)
         WHERE id = $1 AND status = 'in_progress'",
    )
    .bind(request)
    .bind(commands::RESULT_DAYS)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The background tidy-up (§8.2): expired proposals → expired, their queued
/// executions failed. Applying checks `clock_timestamp()` itself; this only
/// keeps the index honest. Returns how many changed.
pub async fn expire_due(app: &App) -> Result<u64> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM changesets WHERE state IN ('proposed', 'approved', 'failed')
           AND expires_at <= clock_timestamp() ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await?;
    let mut n = 0;
    for id in ids {
        let changed: bool = retrying!(
            app,
            tx,
            async {
                let Some(cs) = load_cs(&mut tx, id, true).await? else {
                    return Ok(false);
                };
                if !matches!(cs.state.as_str(), "proposed" | "approved" | "failed")
                    || clock(&mut tx).await? < cs.expires_at
                {
                    return Ok(false);
                }
                end_queued(&mut tx, id, "changeset_expired").await?;
                sqlx::query("UPDATE changesets SET state = 'expired' WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                Ok(true)
            }
            .await
        )?;
        n += changed as u64;
    }
    Ok(n)
}

/// `POST /changesets/{id}/revisions` (§8.5): the owner writes a new version
/// within the original authority ceiling; origin, delegation, context,
/// ceiling and expiry stay; the source snapshot is the current R.
pub async fn owner_revise(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &Json,
) -> Result<ChangesetView> {
    let user = owner_of(&caller)?;
    let parsed_input: RevisionInput = serde_json::from_value(input.clone()).map_err(|e| {
        ApiError::new(ErrorCode::InvalidRequest, format!("not a revision: {e}"))
            .with_details(json!({"kind": "invalid_body"}))
    })?;
    let proposal = input["proposal"].clone();
    let expected = parsed_input.expected_current_version;
    let version: i32 = retrying!(
        app,
        tx,
        async {
            let cs = load_owned(&mut tx, user, id, true).await?;
            let parsed = commands::parse(&mut tx, &proposal, MAX_OPS as usize).await?;
            let fp = commands::fingerprint(
                ACTION_OWNER_REVISE,
                &Sha256::digest(
                    [&parsed.hash[..], id.as_bytes(), &expected.to_be_bytes()].concat(),
                ),
            );
            let context = cs.context_id.ok_or_else(not_found)?;
            lock_context_row(&mut tx, context).await?;
            let dest = destination(&mut tx, &parsed, None).await?;
            let sources = lock_sources(
                &mut tx,
                context,
                &BTreeMap::from([(dest, !parsed.creates().is_empty())]),
            )
            .await?;
            if commands::open_request(&mut tx, user, operation_id, ACTION_OWNER_REVISE, &fp)
                .await?
                .is_none()
            {
                let (_, _, r) =
                    used_key(&mut tx, user, operation_id, ACTION_OWNER_REVISE, &fp).await?;
                return Ok(r
                    .and_then(|r| r["version"].as_i64())
                    .unwrap_or(cs.current_version as i64) as i32);
            }
            let req: Uuid = sqlx::query_scalar(
                "SELECT id FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
            )
            .bind(user)
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
            let (version, _) = owner_version_in(
                &mut tx, caller, user, &cs, &parsed, dest, &sources, expected,
            )
            .await?;
            commands::complete_request(
                &mut tx,
                req,
                &json!({"changeset_id": id, "version": version}),
            )
            .await?;
            Ok(version)
        }
        .await
    )?;
    get(app, caller, id, Some(version)).await
}

/// An owner's revision (§8.5) inside the caller's transaction: checks the
/// state, the expected version, the delegation, the sources and the ceiling,
/// validates and stores version `current + 1`. Returns it and its hash.
#[allow(clippy::too_many_arguments)]
async fn owner_version_in(
    conn: &mut PgConnection,
    caller: AuthContext,
    user: Uuid,
    cs: &CsRow,
    parsed: &Parsed,
    dest: Uuid,
    sources: &[(Uuid, i64)],
    expected: i32,
) -> Result<(i32, [u8; 32])> {
    let id = cs.id;
    let context = cs.context_id.ok_or_else(not_found)?;
    if let Some(e) = state_error(cs, None) {
        return Err(e);
    }
    if cs.state == "approved" || cs.state == "executing" {
        return Err(api(
            ErrorCode::ExecutionInProgress,
            "an approved execution is pending; it cannot be revised now",
        ));
    }
    if clock(&mut *conn).await? >= cs.expires_at {
        return Err(api(ErrorCode::ChangesetExpired, "the proposal expired"));
    }
    if expected != cs.current_version {
        return Err(ApiError::new(
            ErrorCode::VersionSuperseded,
            "the proposal has a newer version",
        )
        .with_details(json!({"current_version": cs.current_version}))
        .into());
    }
    let deleg_ok: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NULL AND expires_at > clock_timestamp() FROM delegations WHERE id = $1 FOR SHARE",
    )
    .bind(cs.delegation_id)
    .fetch_one(&mut *conn)
    .await?;
    if !deleg_ok {
        return Err(delegation_gone());
    }
    let old = load_version(&mut *conn, id, cs.current_version)
        .await?
        .ok_or_else(not_found)?;
    if old.body.as_ref().is_some_and(crate::wiki::is_wiki_doc) {
        return Err(wiki_not_revisable());
    }
    sources_kept(&mut *conn, cs, sources).await?;
    let ceiling = ceiling_map(&old.authority_ceiling);
    if !ceiling.contains_key(&dest) {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "the revision writes outside what the agent was allowed to propose",
        )
        .with_details(json!({"reason": "ceiling"}))
        .into());
    }
    if parsed.op_list().len() > ceiling[&dest].max_ops as usize {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "the revision has more ops than the agent was allowed",
        )
        .with_details(json!({"reason": "ceiling"}))
        .into());
    }
    // Within the ceiling: the simulation runs with it as the delegation.
    let ectx = EvalCtx {
        mode: Mode::Validate,
        caller,
        ceiling: Some(ceiling),
        deadlines: Vec::new(),
        author: Some((cs.proposer_id, Some(cs.on_behalf_of_id))),
    };
    // Contract 05 §3.6.3: the rules version is read before the check, so
    // rules published while it runs leave the version looking changed.
    let pre_pairs = crate::forms::doc_pairs(&mut *conn, &parsed.norm).await?;
    let pre_rules = crate::forms::pairs_version(&mut *conn, &pre_pairs).await?;
    let sim = commands::simulate(&mut *conn, &ectx, parsed).await;
    if let Err(e) = &sim
        && commands::is_rejection(e)
    {
        return Err(sim.err().expect("an error"));
    }
    let checked_at = clock(&mut *conn).await?;
    let validation = stored_validation(&sim, checked_at);
    let version = cs.current_version + 1;
    let hash = store_version(
        &mut *conn,
        parsed,
        &StoreVersion {
            origin: &cs.origin,
            group_id: cs.group_id,
            cs_id: id,
            version,
            new_changeset: false,
            context_project_id: cs.context_project_id,
            proposer: cs.proposer_id,
            on_behalf_of: cs.on_behalf_of_id,
            delegation: cs.delegation_id.unwrap_or_default(),
            context,
            expires_at: cs.expires_at,
            ceiling: &old.authority_ceiling,
            sources,
            editor: user,
            edit_method: "owner_revise",
            based_on: Some(cs.current_version),
            validation: &validation,
            rules_version: pre_rules,
            // G13: only the proposing agent's own versions record one.
            baseline: None,
        },
    )
    .await?;
    Ok((version, hash))
}

// ---------------------------------------------------------------- approve and apply (§8.6)

enum Accepted {
    /// Queued: run transaction B.
    Pending(Uuid),
    /// The key's execution is finished: answer with it.
    Finished(Uuid),
}

/// `POST /changesets/{id}/approve-and-apply`.
pub async fn approve_and_apply(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &ApproveInput,
) -> Result<ApplyOutcome> {
    let who = session_executor(&caller)?;
    let hash = parse_hash(&input.proposal_hash)?;
    let review_mode = input.review_mode.clone().unwrap_or_else(|| "detail".into());
    let fp = commands::fingerprint(
        ACTION_APPROVE,
        &digest(&json!({"changeset_id": id, "version": input.version,
                        "proposal_hash": hex::encode(&hash), "review_mode": review_mode})),
    );
    fault("approve.before_request").await?;

    // Transaction A.
    let accepted = retrying!(
        app,
        tx,
        async {
            accept(
                &mut tx,
                caller,
                &who,
                operation_id,
                id,
                input.version,
                &hash,
                &review_mode,
                &fp,
            )
            .await
        }
        .await
    )?;
    let exec_id = match accepted {
        Accepted::Finished(e) => return finished(app, caller, e, None).await,
        Accepted::Pending(e) => e,
    };
    run_after_a(app, caller, &who, exec_id, "approve.after_a").await
}

/// Transactions B and C for a queued execution (§8.6).
async fn run_after_a(
    app: &App,
    caller: AuthContext,
    who: &commands::Executor,
    exec_id: Uuid,
    after_a: &str,
) -> Result<ApplyOutcome> {
    let pending = || ApplyOutcome {
        pending: true,
        execution_id: exec_id,
        execution: None,
    };
    if fault(after_a).await.is_err() {
        return Ok(pending());
    }

    // Transaction B, retried as the same execution on deadlocks (T-I6).
    let mut attempt = 0;
    // From here on the execution exists: anything that leaves its outcome
    // unknown is 202 with where to look, never an error (§8.6).
    let failure = loop {
        let Ok(mut tx) = begin_apply(app).await else {
            return Ok(pending());
        };
        match apply_tx(&mut tx, caller, who, exec_id).await {
            Ok(()) => {
                if tx.commit().await.is_err() || fault("apply.after_commit").await.is_err() {
                    return Ok(pending());
                }
                return Ok(finished(app, caller, exec_id, None)
                    .await
                    .unwrap_or_else(|_| pending()));
            }
            Err(e) => {
                drop(tx);
                if commands::is_retryable_db(&e) && attempt < RETRIES {
                    attempt += 1;
                    tracing::info!(execution = %exec_id, attempt, "apply retried after a deadlock");
                    continue;
                }
                // Lock wait over budget, a cut-off request or the DB away:
                // rolled back, still queued; the same request continues it.
                if transient(&e) {
                    return Ok(pending());
                }
                // A domain failure, or a permanent fault (a CHECK, a trigger):
                // rolled back with zero effect, recorded as failed so the
                // active execution does not stay queued forever.
                if classify(&e).is_none() {
                    tracing::error!(execution = %exec_id, error = %format!("{e:#}"), "apply failed on a database fault");
                }
                break e;
            }
        }
    };
    if fault("apply.before_fail_record").await.is_err() {
        return Ok(pending());
    }

    // Transaction C.
    let mut attempt = 0;
    loop {
        let Ok(mut tx) = begin_apply(app).await else {
            return Ok(pending());
        };
        match record_failure(&mut tx, exec_id, &failure).await {
            Ok(()) => {
                if tx.commit().await.is_err() {
                    return Ok(pending());
                }
                return Ok(finished(app, caller, exec_id, Some(&failure))
                    .await
                    .unwrap_or_else(|_| pending()));
            }
            Err(e) => {
                drop(tx);
                if commands::is_retryable_db(&e) && attempt < RETRIES {
                    attempt += 1;
                    continue;
                }
                return Ok(pending());
            }
        }
    }
}

async fn begin_apply(app: &App) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// The outcome is unknown and the same request may finish it: the DB away,
/// a cut-off connection, a lock wait over budget.
fn transient(e: &anyhow::Error) -> bool {
    commands::is_lock_timeout(e)
        || classify(e).is_some_and(|a| a.code == ErrorCode::TemporarilyUnavailable)
}

/// HTTP 200 with the execution as it is now; `failure` adds the live error
/// (with its details, for the owner) when this request recorded it.
async fn finished(
    app: &App,
    caller: AuthContext,
    exec_id: Uuid,
    failure: Option<&anyhow::Error>,
) -> Result<ApplyOutcome> {
    let mut tx = read_tx(app).await?;
    let ex = load_exec(&mut tx, exec_id, false)
        .await?
        .ok_or_else(not_found)?;
    let mut view = exec_view(&mut tx, caller, &ex).await?;
    // The live error only adds details to the recorded one; when the gap
    // ended the execution another way (T-R12) the record wins.
    if let (Some(f), "failed") = (failure, view.status.as_str()) {
        let live = error_view(f);
        if view.error.as_ref().is_none_or(|e| e.code == live.code) {
            view.error = Some(live);
        }
    }
    Ok(ApplyOutcome {
        pending: view.status == "queued" || view.status == "running",
        execution_id: exec_id,
        execution: Some(view),
    })
}

#[allow(clippy::too_many_arguments)]
async fn accept(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &commands::Executor,
    operation_id: Uuid,
    id: Uuid,
    version: i32,
    hash: &[u8],
    review_mode: &str,
    fp: &[u8; 32],
) -> Result<Accepted> {
    let cs = load_owned(&mut *conn, who.user_id, id, true).await?;
    let Some(request) =
        commands::open_request(&mut *conn, who.user_id, operation_id, ACTION_APPROVE, fp).await?
    else {
        let (req, _, _) =
            used_key(&mut *conn, who.user_id, operation_id, ACTION_APPROVE, fp).await?;
        let ex: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT id, status FROM changeset_executions WHERE operation_request_id = $1",
        )
        .bind(req)
        .fetch_optional(&mut *conn)
        .await?;
        return match ex {
            Some((e, s)) if s == "succeeded" || s == "failed" => Ok(Accepted::Finished(e)),
            Some((e, _)) => Ok(Accepted::Pending(e)),
            None => Err(api(
                ErrorCode::ExecutionInProgress,
                "this operation is still running",
            )),
        };
    };
    if review_mode != "detail" && review_mode != "queue" {
        // `revise_and_apply` is its own endpoint.
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "review_mode must be \"detail\" or \"queue\"",
        )
        .with_details(json!({"path": "review_mode", "allowed": ["detail", "queue"]}))
        .into());
    }
    let applied: Option<Uuid> = sqlx::query_scalar(
        "SELECT execution_id FROM changeset_applications WHERE changeset_id = $1 ORDER BY applied_at DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(e) = state_error(&cs, applied) {
        return Err(e);
    }
    let ver = check_version(&mut *conn, &cs, version, hash).await?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM changeset_executions
                        WHERE changeset_id = $1 AND status IN ('queued', 'running'))",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if active {
        return Err(api(
            ErrorCode::ExecutionInProgress,
            "another approval of this proposal is running",
        ));
    }
    let deadline = commands::lock_executor(&mut *conn, who).await?;
    commands::check_deadlines(
        &mut *conn,
        &deadline
            .map(|d| (d, ErrorCode::Unauthenticated))
            .into_iter()
            .collect::<Vec<_>>(),
    )
    .await?;
    lock_context_row(&mut *conn, cs.context_id.unwrap_or_default()).await?;
    let deleg_ok: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NULL AND expires_at > clock_timestamp() FROM delegations WHERE id = $1 FOR SHARE",
    )
    .bind(cs.delegation_id)
    .fetch_one(&mut *conn)
    .await?;
    if !deleg_ok {
        return Err(delegation_gone());
    }
    // Design 05 §3.12.5 (appendix D V25): a proposal of a paused role is not
    // approved, like one of a revoked delegation.
    crate::ai_team::check_not_paused(
        &mut *conn,
        cs.proposer_id,
        cs.delegation_id.unwrap_or_default(),
    )
    .await?;
    if readable_targets(&mut *conn, caller, ver.body.as_ref().expect("checked"))
        .await?
        .is_none()
    {
        return Err(api(
            ErrorCode::ProposalUnavailable,
            "you can no longer read every issue this proposal changes",
        ));
    }
    // §15.3: a queue approval only for what still qualifies (the whole
    // transaction A rolls back: no decision, execution or request row).
    if review_mode == "queue" {
        let status = ver
            .validation
            .as_ref()
            .and_then(|j| j["status"].as_str())
            .unwrap_or("blocked")
            .to_string();
        let e = crate::review_queue::eligibility_for(
            &mut *conn,
            caller,
            ver.body.as_ref().expect("checked"),
            &status,
            ver.rules_version.as_deref(),
            cs.chain_review.as_deref(),
        )
        .await?;
        if !e.queue_ok() {
            return Err(crate::review_queue::outdated(&e));
        }
    }

    let exec_id = queue_execution(&mut *conn, who, id, version, hash, request, review_mode).await?;
    Ok(Accepted::Pending(exec_id))
}

/// The approval and its queued execution (end of transaction A, §8.6).
async fn queue_execution(
    conn: &mut PgConnection,
    who: &commands::Executor,
    id: Uuid,
    version: i32,
    hash: &[u8],
    request: Uuid,
    review_mode: &str,
) -> Result<Uuid> {
    let decision: Uuid = sqlx::query_scalar(
        "INSERT INTO changeset_decisions (changeset_id, version, proposal_hash, decider_id, decision,
                                          session_id, review_mode)
         VALUES ($1, $2, $3, $4, 'approve', $5, $6) RETURNING id",
    )
    .bind(id)
    .bind(version)
    .bind(hash)
    .bind(who.user_id)
    .bind(who.session_id)
    .bind(review_mode)
    .fetch_one(&mut *conn)
    .await?;
    let exec_id = Uuid::now_v7();
    sqlx::query("INSERT INTO execution_refs (id, kind) VALUES ($1, 'changeset')")
        .bind(exec_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO changeset_executions (id, changeset_id, version, proposal_hash, attempt_no,
                                           operation_request_id, decision_id, executor_id,
                                           executor_session_id, status)
         SELECT $1, $2, $3, $4, COALESCE(max(attempt_no), 0) + 1, $5, $6, $7, $8, 'queued'
         FROM changeset_executions WHERE changeset_id = $2 AND version = $3",
    )
    .bind(exec_id)
    .bind(id)
    .bind(version)
    .bind(hash)
    .bind(request)
    .bind(decision)
    .bind(who.user_id)
    .bind(who.session_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE changesets SET state = 'approved' WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE operation_requests SET result_ref = $2 WHERE id = $1")
        .bind(request)
        .bind(json!({"changeset_id": id, "version": version, "execution_id": exec_id}))
        .execute(&mut *conn)
        .await?;
    Ok(exec_id)
}

/// Transaction B (§6.7 apply steps 1–18). `Ok` also when the execution was
/// finished by an earlier request with the same key.
async fn apply_tx(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &commands::Executor,
    exec_id: Uuid,
) -> Result<()> {
    let cs_id: Uuid =
        sqlx::query_scalar("SELECT changeset_id FROM changeset_executions WHERE id = $1")
            .bind(exec_id)
            .fetch_one(&mut *conn)
            .await?;
    // Steps 1–3.
    let cs = load_cs(&mut *conn, cs_id, true)
        .await?
        .ok_or_else(not_found)?;
    let ex = load_exec(&mut *conn, exec_id, true)
        .await?
        .ok_or_else(not_found)?;
    if ex.executor_id != who.user_id {
        return Err(not_found());
    }
    let applied: Option<Uuid> = sqlx::query_scalar(
        "SELECT execution_id FROM changeset_applications WHERE changeset_id = $1 AND version = $2",
    )
    .bind(cs_id)
    .bind(ex.version)
    .fetch_optional(&mut *conn)
    .await?;
    if applied.is_some() || ex.status == "succeeded" || ex.status == "failed" {
        return Ok(());
    }
    // Step 4.
    if cs.current_version != ex.version {
        return Err(api(
            ErrorCode::VersionSuperseded,
            "the proposal was revised after the approval",
        ));
    }
    let ver = load_version(&mut *conn, cs_id, ex.version)
        .await?
        .ok_or_else(not_found)?;
    let Some(body) = ver.body.clone() else {
        return Err(api(
            ErrorCode::ApprovalInvalidated,
            "the proposal content was removed",
        ));
    };
    if ver.proposal_hash != ex.proposal_hash {
        return Err(api(ErrorCode::ApprovalInvalidated, "the proposal changed"));
    }
    if cs.state != "approved" {
        return Err(api(
            ErrorCode::ChangesetNotExecutable,
            format!("the proposal is {}", cs.state),
        ));
    }
    // Step 4b.
    let context = cs.context_id.ok_or_else(not_found)?;
    lock_context_row(&mut *conn, context).await?;
    fault("apply.before_step5").await?;
    // Step 5: principals (id order), session, delegation, client.
    let mut principals = vec![who.user_id, cs.proposer_id];
    principals.sort();
    principals.dedup();
    let active: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM (SELECT id, status FROM principals WHERE id = ANY($1) ORDER BY id FOR SHARE) x
         WHERE status = 'active'",
    )
    .bind(&principals)
    .fetch_all(&mut *conn)
    .await?;
    // The approver suspended: their session no longer counts (§4.4); the
    // agent suspended: its delegations are gone.
    if !active.contains(&who.user_id) {
        return Err(api(
            ErrorCode::Unauthenticated,
            "your account is no longer active",
        ));
    }
    if !active.contains(&cs.proposer_id) {
        return Err(delegation_gone());
    }
    let session_deadline = commands::lock_executor(&mut *conn, who).await?;
    let deleg: Option<(DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT expires_at, revoked_at IS NULL FROM delegations WHERE id = $1 FOR SHARE",
    )
    .bind(cs.delegation_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((deleg_expires, true)) = deleg else {
        return Err(delegation_gone());
    };
    // Design 05 §3.12.5 (V25): paused since the approval → no effect.
    crate::ai_team::check_not_paused(
        &mut *conn,
        cs.proposer_id,
        cs.delegation_id.unwrap_or_default(),
    )
    .await?;
    let client: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT c.id, c.enabled FROM agent_clients c
         JOIN agent_principals ap ON ap.agent_client_id = c.id
         WHERE ap.agent_principal_id = $1 FOR SHARE OF c",
    )
    .bind(cs.proposer_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((client_id, true)) = client else {
        return Err(delegation_gone());
    };
    fault("apply.after_step5").await?;
    if crate::wiki::is_wiki_doc(&body) {
        let mut deadlines = vec![
            (cs.expires_at, ErrorCode::ChangesetExpired),
            (deleg_expires, ErrorCode::DelegationUnavailable),
        ];
        if let Some(d) = session_deadline {
            deadlines.push((d, ErrorCode::Unauthenticated));
        }
        return apply_wiki(
            &mut *conn, caller, who, &cs, &ex, &ver, &body, client_id, deadlines,
        )
        .await;
    }
    // Step 6: R and the destination, epochs as recorded.
    let parsed = commands::parse(&mut *conn, &body, MAX_OPS as usize).await?;
    // §16.3: an operation the delegator has since turned off is not applied
    // (review and direct both allow applying an approved version).
    if let Some(d) = cs.delegation_id
        && let Some(e) = policy::load(&mut *conn, d).await?.decide(&parsed).refusal()
    {
        return Err(e);
    }
    let mut dest: BTreeMap<Uuid, bool> = ceiling_map(&ver.authority_ceiling)
        .keys()
        .map(|p| (*p, false))
        .collect();
    for (_, p, _) in parsed.creates() {
        dest.insert(p, true);
    }
    lock_sources(&mut *conn, context, &dest).await?;

    sqlx::query(
        "UPDATE changeset_executions SET status = 'running', started_at = clock_timestamp() WHERE id = $1",
    )
    .bind(exec_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE changesets SET state = 'executing' WHERE id = $1")
        .bind(cs_id)
        .execute(&mut *conn)
        .await?;

    // Steps 7–14, deadlines checked after the locks.
    let mut deadlines = vec![
        (cs.expires_at, ErrorCode::ChangesetExpired),
        (deleg_expires, ErrorCode::DelegationUnavailable),
    ];
    if let Some(d) = session_deadline {
        deadlines.push((d, ErrorCode::Unauthenticated));
    }
    let ectx = EvalCtx {
        mode: Mode::Apply,
        caller,
        ceiling: Some(ceiling_map(&ver.authority_ceiling)),
        deadlines,
        author: Some((cs.proposer_id, Some(cs.on_behalf_of_id))),
    };
    let sim = commands::simulate(&mut *conn, &ectx, &parsed).await?;
    // Step 13a: a queue approval qualifies again, evidence in the executor's scope.
    let mode: String = sqlx::query_scalar(
        "SELECT d.review_mode FROM changeset_decisions d
             JOIN changeset_executions e ON e.decision_id = d.id WHERE e.id = $1",
    )
    .bind(exec_id)
    .fetch_one(&mut *conn)
    .await?;
    if mode == "queue" {
        let e = crate::review_queue::eligibility_for(
            &mut *conn,
            caller,
            &body,
            "passed",
            ver.rules_version.as_deref(),
            cs.chain_review.as_deref(),
        )
        .await?;
        if !e.queue_ok() {
            return Err(crate::review_queue::outdated(&e));
        }
    }
    // Step 14: the audience against this version's source snapshot (M6).
    let snapshot: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM changeset_version_source_projects
         WHERE changeset_id = $1 AND version = $2 ORDER BY project_id",
    )
    .bind(cs_id)
    .bind(ex.version)
    .fetch_all(&mut *conn)
    .await?;
    crate::audience::check(&mut *conn, &parsed, &snapshot, cs.on_behalf_of_id).await?;
    fault("apply.after_locks").await?;

    // Steps 15–18.
    let at = Attribution {
        origin: "external_agent",
        actor_id: cs.proposer_id,
        actor_kind: "agent",
        author_id: cs.proposer_id,
        author_on_behalf: Some(cs.on_behalf_of_id),
        request_id: ex.operation_request_id,
        command_execution_id: None,
        execution_ref: exec_id,
        changeset: Some((cs_id, ex.version)),
        changeset_execution_id: Some(exec_id),
        proposer_id: Some(cs.proposer_id),
        approver_id: Some(who.user_id),
        on_behalf_of_id: Some(cs.on_behalf_of_id),
        delegation_id: cs.delegation_id,
        agent_client_id: Some(client_id),
        record_changes: true,
    };
    let effects = commands::write_effects(&mut *conn, &parsed, &sim, &at).await?;
    let token = sim.token.clone().unwrap_or_default();
    let item_results = json!({
        "items": sim.items.iter().map(|i: &ItemResult| json!({
            "op_id": i.op_id, "status": i.status, "issue_id": i.issue_id,
            "version_id": i.version_id, "time_entry_id": i.time_entry_id})).collect::<Vec<_>>(),
        "versions": sim.version.iter().map(|p| p.effect()).collect::<Vec<_>>(),
        "time_entries": sim.time.as_ref().map(|t| t.effects()).unwrap_or_default(),
        "effects": effects.iter().map(commands::stored_effect).collect::<Vec<_>>(),
    });
    sqlx::query(
        "INSERT INTO changeset_applications (changeset_id, version, execution_id, effect_count)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(cs_id)
    .bind(ex.version)
    .bind(exec_id)
    .bind(
        (effects.iter().filter(|e| e.changed).count()
            + sim.version.iter().filter(|p| p.effect().changed).count()
            + sim
                .time
                .as_ref()
                .map_or(0, |t| t.effects().iter().filter(|e| e.changed).count())) as i32,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE changeset_executions SET status = 'succeeded', item_results = $2,
                consistent_after = $3, finished_at = clock_timestamp()
         WHERE id = $1",
    )
    .bind(exec_id)
    .bind(&item_results)
    .bind(&token)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE changesets SET state = 'succeeded' WHERE id = $1")
        .bind(cs_id)
        .execute(&mut *conn)
        .await?;
    commands::complete_request(
        &mut *conn,
        ex.operation_request_id,
        &json!({"changeset_id": cs_id, "version": ex.version, "execution_id": exec_id,
                "consistent_after": token}),
    )
    .await?;
    fault("apply.before_commit").await?;
    Ok(())
}

/// Transaction B for an approved wiki proposal (design 06 §3.12): the
/// delegation's policy, R-AG1 (the approver reads the wiki, W(P) ⊇ A(P)) and
/// R-AG3 against the version's sources again, then the write as the agent
/// for the delegator, approved by the owner (`external_agent`). A page
/// changed since the proposal is a `conflict` (the execution fails).
#[allow(clippy::too_many_arguments)]
async fn apply_wiki(
    conn: &mut PgConnection,
    caller: AuthContext,
    who: &commands::Executor,
    cs: &CsRow,
    ex: &ExecRow,
    ver: &VerRow,
    body: &Json,
    client: Uuid,
    deadlines: Vec<(DateTime<Utc>, ErrorCode)>,
) -> Result<()> {
    let doc = crate::wiki::parse(body)?;
    let (kind, op_id) = match &doc.ops[0] {
        crate::wiki::WikiOp::WikiPut(o) => ("wiki_put", o.op_id),
        crate::wiki::WikiOp::WikiMove(o) => ("wiki_move", o.op_id),
        _ => return Err(api(ErrorCode::InvalidRequest, "not an agent wiki op")),
    };
    let project = wiki_project(body).ok_or_else(not_found)?;
    let context = cs.context_id.ok_or_else(not_found)?;
    let delegation = cs.delegation_id.ok_or_else(delegation_gone)?;
    // §16.3: an op the delegator has since turned off is not applied.
    let ops: Vec<String> = sqlx::query_scalar("SELECT proposal_ops FROM delegations WHERE id = $1")
        .bind(delegation)
        .fetch_one(&mut *conn)
        .await?;
    if let Some(e) = wiki_decision(&mut *conn, delegation, &ops, kind)
        .await?
        .refusal()
    {
        return Err(e);
    }
    if !ceiling_map(&ver.authority_ceiling).contains_key(&project) {
        return Err(not_found());
    }
    let dest: BTreeMap<Uuid, bool> = [(project, false)].into_iter().collect();
    lock_sources(&mut *conn, context, &dest).await?;

    sqlx::query(
        "UPDATE changeset_executions SET status = 'running', started_at = clock_timestamp() WHERE id = $1",
    )
    .bind(ex.id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE changesets SET state = 'executing' WHERE id = $1")
        .bind(cs.id)
        .execute(&mut *conn)
        .await?;

    // R-W1 for the approver (the delegator), R-AG1's W(P) ⊇ A(P), R-AG3
    // against this version's sources.
    let access = crate::wiki::access_of(&mut *conn, caller, project)
        .await?
        .ok_or_else(|| {
            api(
                ErrorCode::ProposalUnavailable,
                "you can no longer read this wiki",
            )
        })?;
    let delegable = crate::audience::wiki_delegable(&mut *conn, project).await?;
    if !delegable.pages {
        return Err(crate::wiki::agent::not_delegable());
    }
    let snapshot: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT project_id, security_epoch FROM changeset_version_source_projects
         WHERE changeset_id = $1 AND version = $2 ORDER BY project_id",
    )
    .bind(cs.id)
    .bind(ex.version)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = snapshot.iter().map(|x| x.0).collect();
    crate::audience::check_wiki(&mut *conn, project, &ids).await?;
    fault("apply.after_locks").await?;

    let hash: [u8; 32] = ver
        .proposal_hash
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("a proposal hash is 32 bytes"))?;
    let writer = crate::wiki::agent::AgentWriter {
        agent: cs.proposer_id,
        delegator: cs.on_behalf_of_id,
        delegation,
        client,
        context,
        policy_version: 0,
        hash,
        sources: snapshot,
        deadlines,
        history: delegable.history,
        access,
        review: Some(crate::wiki::agent::ReviewApply {
            approver: who.user_id,
            changeset: cs.id,
            version: ex.version,
            op_id,
            exec: ex.id,
        }),
    };
    let r = crate::wiki::execute(
        &mut *conn,
        caller,
        crate::wiki::Writer::Agent(&writer),
        ex.operation_request_id,
        hash,
        &doc,
    )
    .await?;
    let token = r.search.consistent_after.clone();
    let item_results = json!({
        "items": r.items.iter().map(|i| json!({
            "op_id": i.op_id, "status": i.status, "wiki_page_id": i.wiki_page_id})).collect::<Vec<_>>(),
        "wiki": r.wiki.iter().map(|w| json!({
            "page_id": w.page_id, "project_id": w.project_id, "version": w.version,
            "lock_version": w.lock_version, "changed": w.changed, "created": w.created,
        })).collect::<Vec<_>>(),
        "effects": [],
    });
    sqlx::query(
        "INSERT INTO changeset_applications (changeset_id, version, execution_id, effect_count)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(cs.id)
    .bind(ex.version)
    .bind(ex.id)
    .bind(r.wiki.iter().filter(|w| w.changed).count() as i32)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE changeset_executions SET status = 'succeeded', item_results = $2,
                consistent_after = $3, finished_at = clock_timestamp()
         WHERE id = $1",
    )
    .bind(ex.id)
    .bind(&item_results)
    .bind(&token)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE changesets SET state = 'succeeded' WHERE id = $1")
        .bind(cs.id)
        .execute(&mut *conn)
        .await?;
    commands::complete_request(
        &mut *conn,
        ex.operation_request_id,
        &json!({"changeset_id": cs.id, "version": ex.version, "execution_id": ex.id,
                "consistent_after": token}),
    )
    .await?;
    fault("apply.before_commit").await?;
    Ok(())
}

/// Per-op results of a failed execution: the failing op `failed` with its
/// code, earlier ops `rolled_back`, later ones `not_run`.
fn failure_items(ops: &[(Uuid, i32)], e: &anyhow::Error) -> (String, Vec<ExecItem>) {
    let a = classify(e);
    let code = a
        .as_ref()
        .map_or(ErrorCode::TemporarilyUnavailable, |a| a.code)
        .as_str()
        .to_string();
    let at = a
        .and_then(|a| a.details)
        .and_then(|d| d.get("op_index").and_then(Json::as_u64))
        .map(|i| i as i32);
    let items = ops
        .iter()
        .map(|(op_id, pos)| {
            let (status, c) = match at {
                Some(i) if *pos < i => ("rolled_back", None),
                Some(i) if *pos == i => ("failed", Some(code.clone())),
                _ => ("not_run", None),
            };
            ExecItem {
                op_id: *op_id,
                status: status.into(),
                code: c,
                issue_id: None,
                wiki_page_id: None,
            }
        })
        .collect();
    (code, items)
}

/// Transaction C (§8.6): record a domain failure unless the execution
/// finished meanwhile; the ChangeSet becomes failed only if it is still
/// approved at that version.
async fn record_failure(conn: &mut PgConnection, exec_id: Uuid, e: &anyhow::Error) -> Result<()> {
    let cs_id: Uuid =
        sqlx::query_scalar("SELECT changeset_id FROM changeset_executions WHERE id = $1")
            .bind(exec_id)
            .fetch_one(&mut *conn)
            .await?;
    load_cs(&mut *conn, cs_id, true).await?;
    let ex = load_exec(&mut *conn, exec_id, true)
        .await?
        .ok_or_else(not_found)?;
    let applied: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM changeset_applications WHERE changeset_id = $1 AND version = $2)",
    )
    .bind(cs_id)
    .bind(ex.version)
    .fetch_one(&mut *conn)
    .await?;
    if applied || ex.status == "succeeded" || ex.status == "failed" {
        return Ok(());
    }
    let ops: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT op_id, position FROM changeset_version_ops WHERE changeset_id = $1 AND version = $2
         ORDER BY position",
    )
    .bind(cs_id)
    .bind(ex.version)
    .fetch_all(&mut *conn)
    .await?;
    let (code, items) = failure_items(&ops, e);
    sqlx::query(
        "UPDATE changeset_executions SET status = 'failed', error_code = $2, item_results = $3,
                finished_at = clock_timestamp()
         WHERE id = $1",
    )
    .bind(exec_id)
    .bind(&code)
    .bind(json!({"items": items, "effects": []}))
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE changesets SET state = 'failed'
         WHERE id = $1 AND state IN ('approved', 'executing') AND current_version = $2",
    )
    .bind(cs_id)
    .bind(ex.version)
    .execute(&mut *conn)
    .await?;
    end_request(&mut *conn, ex.operation_request_id).await?;
    Ok(())
}

// ---------------------------------------------------------------- executions and operations

/// `GET /executions/{id}`: the executor only. Never starts anything (§8.3).
pub async fn get_execution(app: &App, caller: AuthContext, id: Uuid) -> Result<ExecutionView> {
    let user = owner_of(&caller)?;
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let ex = load_exec(&mut tx, id, false).await?.ok_or_else(not_found)?;
    if ex.executor_id != user {
        return Err(not_found());
    }
    exec_view(&mut tx, caller, &ex).await
}

/// `GET /operations/{operation_id}`: what the caller's key did.
pub async fn get_operation(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
) -> Result<OperationView> {
    let user = owner_of(&caller)?;
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let row: Option<(String, String, Option<Json>, bool)> = sqlx::query_as(
        "SELECT action, status, result_ref, COALESCE(result_expires_at < now(), false)
         FROM operation_requests WHERE caller_id = $1 AND operation_id = $2",
    )
    .bind(user)
    .bind(operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((action, status, r, expired)) = row else {
        return Err(api(ErrorCode::NotFound, "no such operation"));
    };
    if expired || (status != "in_progress" && r.is_none()) {
        return Err(api(
            ErrorCode::OperationResultExpired,
            "the result of this operation is no longer kept; it is not run again",
        ));
    }
    let r = r.unwrap_or(Json::Null);
    let uuid = |k: &str| r[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
    let execution = match uuid("execution_id") {
        Some(e) if action == ACTION_APPROVE => match load_exec(&mut tx, e, false).await? {
            Some(ex) => Some(exec_view(&mut tx, caller, &ex).await?),
            None => None,
        },
        _ => None,
    };
    // A ChangeSet the caller no longer owns is not described.
    let changeset_id = match uuid("changeset_id") {
        Some(c) => load_cs(&mut tx, c, false)
            .await?
            .filter(|cs| cs.on_behalf_of_id == user)
            .map(|cs| cs.id),
        None => None,
    };
    Ok(OperationView {
        operation_id,
        action,
        status,
        changeset_id,
        version: r["version"].as_i64().map(|v| v as i32),
        execution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed vector for `proposal_hash` (§8.1: the canonical rules are
    /// versioned and tested). Changing the framing changes this value.
    #[test]
    fn proposal_hash_vector() {
        let u = |n: u128| Uuid::from_u128(n);
        let doc = json!({"command_schema_version": 1, "exec_mode": "atomic", "title": "é",
                         "ops": [{"op_id": u(1), "kind": "add_note",
                                  "target": {"type": "issue", "id": u(2)}, "body": "x", "is_private": false}],
                         "preconditions": [{"target": {"type": "issue", "id": u(2)}, "lock_version": 3,
                                            "field_values": {}}],
                         "evidence": []});
        let ceiling = json!({"projects": [{"project_id": u(3), "proposal_ops": ["add_note"],
                                           "writable_fields": [], "max_ops": 10}]});
        let t = DateTime::parse_from_rfc3339("2026-11-13T14:32:00.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        let h = |sources: &[(Uuid, i64)], doc: &Json| {
            hex::encode(proposal_hash(&HashInput {
                document: doc,
                expires_at: t,
                ceiling: &ceiling,
                origin: "external_agent",
                proposer: u(4),
                delegation: u(5),
                context: u(6),
                sources,
                audience_contract_ver: 1,
            }))
        };
        let a = h(&[(u(3), 2), (u(7), 1)], &doc);
        // Source order does not matter; key order does not matter.
        assert_eq!(a, h(&[(u(7), 1), (u(3), 2)], &doc));
        let mut reordered: serde_json::Map<String, Json> = serde_json::Map::new();
        for (k, v) in doc.as_object().unwrap().iter().rev() {
            reordered.insert(k.clone(), v.clone());
        }
        assert_eq!(a, h(&[(u(3), 2), (u(7), 1)], &Json::Object(reordered)));
        // No Unicode normalization: decomposed é is another proposal.
        let mut nfd = doc.clone();
        nfd["title"] = json!("e\u{301}");
        assert_ne!(a, h(&[(u(3), 2), (u(7), 1)], &nfd));
        assert_eq!(a, PROPOSAL_HASH_VECTOR, "the canonical form changed");
    }

    const PROPOSAL_HASH_VECTOR: &str =
        "98df9a803af9c70e1cb7f12c1581adcb34eea0c72d198620bce8192be679fdad";

    #[test]
    fn failure_items_mark_ops() {
        let e: anyhow::Error = ApiError::new(ErrorCode::InvalidTransition, "x")
            .with_details(json!({"op_index": 1}))
            .into();
        let ops = [
            (Uuid::from_u128(1), 0),
            (Uuid::from_u128(2), 1),
            (Uuid::from_u128(3), 2),
        ];
        let (code, items) = failure_items(&ops, &e);
        assert_eq!(code, "invalid_transition");
        let s: Vec<&str> = items.iter().map(|i| i.status.as_str()).collect();
        assert_eq!(s, ["rolled_back", "failed", "not_run"]);
    }
}

// ------------------------------------------- revision previews and revise-and-apply (§15.4)

const PREVIEW_PURPOSE: &str = "revision_preview";
static PREVIEW_TTL_SECS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(600);

/// Tests: how long a `revision_review_token` opens (default 10 minutes).
#[cfg(feature = "test-support")]
pub fn set_preview_ttl_secs(secs: i64) {
    PREVIEW_TTL_SECS.store(secs, std::sync::atomic::Ordering::SeqCst);
}

/// `POST /changesets/{id}/revision-previews`: per conflicting field, keep
/// the issue's `current` value or the `proposed` one (default `proposed`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewInput {
    /// The version the choices were made on (the current one).
    pub version: i32,
    /// `"<op_id>/<field>"` → `"current"` | `"proposed"`.
    #[serde(default)]
    pub choices: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevisionPreview {
    /// The complete revised document (every op, note and evidence).
    pub proposal: Json,
    pub validation: CompactValidation,
    pub revision_review_token: String,
    pub expires_at: DateTime<Utc>,
}

/// `POST /changesets/{id}/revise-and-apply`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseAndApplyInput {
    /// The version the preview was made from.
    pub version: i32,
    pub expected_current_version: i32,
    /// The document the preview returned.
    pub proposal: CommandDoc,
    pub revision_review_token: String,
}

/// The targets' current `lock_version`, in id order.
async fn target_locks(
    conn: &mut PgConnection,
    ids: &[Uuid],
    share: bool,
) -> Result<BTreeMap<Uuid, i32>> {
    let sql = if share {
        "SELECT id, lock_version FROM issues WHERE id = ANY($1) ORDER BY id FOR SHARE"
    } else {
        "SELECT id, lock_version FROM issues WHERE id = ANY($1) ORDER BY id"
    };
    let rows: Vec<(Uuid, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.into_iter().collect())
}

fn outdated(msg: &str) -> anyhow::Error {
    api(ErrorCode::ReviewOutdated, msg)
}

/// Applies the owner's choices to `body` and moves every precondition to
/// the issues' current `lock_version` (the owner has seen them).
fn revised_doc(
    body: &Json,
    choices: &BTreeMap<String, String>,
    locks: &BTreeMap<Uuid, i32>,
) -> Result<Json> {
    let mut doc = body.clone();
    let mut used = BTreeSet::new();
    if let Some(ops) = doc["ops"].as_array_mut() {
        for op in ops.iter_mut() {
            let op_id = op["op_id"].as_str().unwrap_or_default().to_string();
            if let Some(set) = op.get_mut("set").and_then(Json::as_object_mut) {
                let keys: Vec<String> = set.keys().cloned().collect();
                for k in keys {
                    let c = format!("{op_id}/{k}");
                    match choices.get(&c).map(String::as_str) {
                        None | Some("proposed") => {}
                        Some("current") => {
                            set.remove(&k);
                        }
                        Some(other) => {
                            return Err(ApiError::new(
                                ErrorCode::InvalidArgument,
                                format!("choice {other:?} must be current or proposed"),
                            )
                            .with_details(json!({"path": format!("choices.{c}"),
                                                 "allowed": ["current", "proposed"]}))
                            .into());
                        }
                    }
                    if choices.contains_key(&c) {
                        used.insert(c);
                    }
                }
            }
        }
        // An update left with nothing to set is dropped.
        ops.retain(|op| {
            op["kind"] != "update_fields" || op["set"].as_object().is_some_and(|m| !m.is_empty())
        });
    }
    if let Some(bad) = choices.keys().find(|k| !used.contains(*k)) {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("no field {bad:?} in this proposal"),
        )
        .with_details(json!({"path": format!("choices.{bad}")}))
        .into());
    }
    if doc["ops"].as_array().is_none_or(Vec::is_empty) {
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            "keeping every current value leaves nothing to apply; reject the proposal instead",
        )
        .with_details(json!({"path": "choices"}))
        .into());
    }
    if let Some(pre) = doc["preconditions"].as_array_mut() {
        for p in pre.iter_mut() {
            let id = p["target"]["id"]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok());
            if let Some(lv) = id.and_then(|i| locks.get(&i)) {
                p["lock_version"] = json!(lv);
            }
            if let Some(m) = p.as_object_mut() {
                m.remove("field_values");
            }
        }
    }
    Ok(doc)
}

pub async fn preview_revision(
    app: &App,
    caller: AuthContext,
    id: Uuid,
    input: &PreviewInput,
) -> Result<RevisionPreview> {
    let user = owner_of(&caller)?;
    let mut tx = read_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    let cs = load_owned(&mut tx, user, id, false).await?;
    if let Some(e) = state_error(&cs, None) {
        return Err(e);
    }
    if input.version != cs.current_version {
        return Err(ApiError::new(
            ErrorCode::VersionSuperseded,
            "the proposal has a newer version",
        )
        .with_details(json!({"current_version": cs.current_version}))
        .into());
    }
    let ver = load_version(&mut tx, id, cs.current_version)
        .await?
        .ok_or_else(not_found)?;
    let Some(body) = ver.body.clone() else {
        return Err(api(
            ErrorCode::ProposalUnavailable,
            "the proposal content was removed",
        ));
    };
    if readable_targets(&mut tx, caller, &body).await?.is_none() {
        return Err(api(
            ErrorCode::ProposalUnavailable,
            "you can no longer read every issue this proposal changes",
        ));
    }
    if crate::wiki::is_wiki_doc(&body) {
        return Err(wiki_not_revisable());
    }
    let ids = body_targets(&body);
    let locks = target_locks(&mut tx, &ids, false).await?;
    let doc = revised_doc(&body, &input.choices, &locks)?;
    let parsed = commands::parse(&mut tx, &doc, MAX_OPS as usize).await?;
    let ectx = EvalCtx {
        mode: Mode::Validate,
        caller,
        ceiling: Some(ceiling_map(&ver.authority_ceiling)),
        deadlines: Vec::new(),
        author: Some((cs.proposer_id, Some(cs.on_behalf_of_id))),
    };
    let sim = commands::simulate(&mut tx, &ectx, &parsed).await;
    if let Err(e) = &sim
        && commands::is_rejection(e)
    {
        return Err(sim.err().expect("an error"));
    }
    let at = clock(&mut tx).await?;
    let validation = compact(&stored_validation(&sim, at));
    tx.commit().await?;
    let claims = json!({
        "owner": user, "changeset_id": id, "version": cs.current_version,
        "source_hash": hex::encode(&ver.proposal_hash),
        "content": hex::encode(parsed.hash),
        "locks": locks.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
    });
    let ttl = chrono::Duration::seconds(PREVIEW_TTL_SECS.load(std::sync::atomic::Ordering::SeqCst));
    let (token, expires_at) = crate::cursor::seal(&app.pool, PREVIEW_PURPOSE, &claims, ttl).await?;
    Ok(RevisionPreview {
        proposal: parsed.norm.clone(),
        validation,
        revision_review_token: token,
        expires_at,
    })
}

/// §15.4: the owner's revision, its approval and the queued execution in one
/// transaction A, then B and C as for approve-and-apply.
pub async fn revise_and_apply(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &Json,
) -> Result<ApplyOutcome> {
    let who = session_executor(&caller)?;
    let user = who.user_id;
    let parsed_input: ReviseAndApplyInput = serde_json::from_value(input.clone()).map_err(|e| {
        ApiError::new(
            ErrorCode::InvalidRequest,
            format!("not a revise-and-apply: {e}"),
        )
        .with_details(json!({"kind": "invalid_body"}))
    })?;
    let proposal = input["proposal"].clone();
    let token = parsed_input.revision_review_token.clone();
    let accepted = retrying!(
        app,
        tx,
        async {
            let parsed = commands::parse(&mut tx, &proposal, MAX_OPS as usize).await?;
            let fp = commands::fingerprint(
                ACTION_REVISE_AND_APPLY,
                &digest(&json!({"changeset_id": id, "version": parsed_input.version,
                    "expected": parsed_input.expected_current_version,
                    "content": hex::encode(parsed.hash),
                    "token": hex::encode(Sha256::digest(token.as_bytes()))})),
            );
            // ② The key first: a known request continues without the token.
            let Some(request) =
                commands::open_request(&mut tx, user, operation_id, ACTION_REVISE_AND_APPLY, &fp)
                    .await?
            else {
                let (req, _, _) =
                    used_key(&mut tx, user, operation_id, ACTION_REVISE_AND_APPLY, &fp).await?;
                let ex: Option<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, status FROM changeset_executions WHERE operation_request_id = $1",
                )
                .bind(req)
                .fetch_optional(&mut *tx)
                .await?;
                return match ex {
                    Some((e, s)) if s == "succeeded" || s == "failed" => Ok(Accepted::Finished(e)),
                    Some((e, _)) => Ok(Accepted::Pending(e)),
                    None => Err(api(
                        ErrorCode::ExecutionInProgress,
                        "this operation is still running",
                    )),
                };
            };
            // ③ A new request: the token, then the locks.
            let claims = match crate::cursor::open(&app.pool, PREVIEW_PURPOSE, &token).await? {
                crate::cursor::Opened::Valid(c) => c,
                crate::cursor::Opened::Expired => {
                    return Err(outdated("the preview expired; preview the revision again"));
                }
                crate::cursor::Opened::Invalid => {
                    return Err(ApiError::new(
                        ErrorCode::InvalidRequest,
                        "revision_review_token is not a preview token",
                    )
                    .with_details(json!({"path": "revision_review_token"}))
                    .into());
                }
            };
            if claims["owner"] != json!(user) || claims["changeset_id"] != json!(id) {
                return Err(ApiError::new(
                    ErrorCode::InvalidRequest,
                    "this preview belongs to another proposal or person",
                )
                .with_details(json!({"path": "revision_review_token"}))
                .into());
            }
            if claims["version"] != json!(parsed_input.version)
                || claims["content"] != json!(hex::encode(parsed.hash))
            {
                return Err(outdated(
                    "the revision differs from the preview; preview it again",
                ));
            }
            let cs = load_owned(&mut tx, user, id, true).await?;
            let context = cs.context_id.ok_or_else(not_found)?;
            lock_context_row(&mut tx, context).await?;
            let dest = destination(&mut tx, &parsed, None).await?;
            let sources = lock_sources(
                &mut tx,
                context,
                &BTreeMap::from([(dest, !parsed.creates().is_empty())]),
            )
            .await?;
            let src = load_version(&mut tx, id, parsed_input.version)
                .await?
                .ok_or_else(not_found)?;
            if claims["source_hash"] != json!(hex::encode(&src.proposal_hash)) {
                return Err(outdated("the proposal changed since the preview"));
            }
            let ids = body_targets(&parsed.norm);
            let locks = target_locks(&mut tx, &ids, true).await?;
            for (k, v) in &locks {
                if claims["locks"][k.to_string()] != json!(v) {
                    return Err(outdated(
                        "an issue changed since the preview; preview the revision again",
                    ));
                }
            }
            // ④ The version, the approval and the execution.
            let (version, hash) = owner_version_in(
                &mut tx,
                caller,
                user,
                &cs,
                &parsed,
                dest,
                &sources,
                parsed_input.expected_current_version,
            )
            .await?;
            let exec_id = queue_execution(
                &mut tx,
                &who,
                id,
                version,
                &hash,
                request,
                "revise_and_apply",
            )
            .await?;
            Ok(Accepted::Pending(exec_id))
        }
        .await
    )?;
    let exec_id = match accepted {
        Accepted::Finished(e) => return finished(app, caller, e, None).await,
        Accepted::Pending(e) => e,
    };
    run_after_a(app, caller, &who, exec_id, "revise.after_a").await
}
