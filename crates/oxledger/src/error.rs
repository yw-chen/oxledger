// SPDX-License-Identifier: GPL-2.0-or-later
//! Errors the caller can act on (contract 02 §4.3, §4.4, §14.2).
//!
//! Domain code returns `anyhow::Error`; an error the caller caused or can act
//! on wraps an [`ApiError`] with a stable [`ErrorCode`]. Anything else is an
//! internal error and is never shown in detail.
//!
//! Over MCP an `ApiError` becomes a tool result with `isError: true` whose
//! structured content is [`ErrorBody`] (contract 02 §4.6: the same object as the
//! REST `{"error": …}`, without the wrapper). Only malformed parameters are
//! JSON-RPC protocol errors.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Stable error codes. Programs depend on these, never on `message`.
/// Codes of the §4.4 table arrive with the code that produces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request: bad JSON, unknown or missing fields, both a cookie
    /// and a bearer token (`details.kind = "ambiguous_credentials"`).
    InvalidRequest,
    /// No valid credentials, or the principal behind them is no longer active.
    Unauthenticated,
    /// Login failed. Unknown login and wrong password look the same.
    InvalidCredentials,
    /// The action needs a recent step-up (password re-entry within 15 minutes).
    ReauthenticationRequired,
    /// Origin, CSRF token or Content-Type check failed.
    CsrfFailed,
    /// Only a person in a browser session may do this (not an agent or a legacy key).
    HumanSessionRequired,
    /// The target is visible but the caller may not do this to it.
    ForbiddenOp,
    /// Missing, or the caller may not know it exists (same shape for both).
    NotFound,
    /// A value in a write request is not allowed; `details.allowed`.
    InvalidValue,
    /// A read tool parameter has a bad value; `details.path`, `details.allowed`.
    InvalidArgument,
    /// The custom field `filter` cannot be used (design 03 §3.7).
    InvalidFilter,
    /// Tampered or foreign cursor.
    InvalidCursor,
    /// A name matches several visible objects; `details.candidates`.
    AmbiguousReference,
    /// The agent's work context cannot be used: `details.kind` is
    /// `"quarantined"` or `"closed"` (§4.4).
    ContextUnavailable,
    /// No usable delegation for this project.
    DelegationUnavailable,
    /// Too many attempts; `retry_after_ms`.
    RateLimited,
    /// Database or another dependency is down for a moment.
    TemporarilyUnavailable,
    /// `lock_version` does not match; re-read and decide again.
    Conflict,
    /// A readable precondition field value does not match.
    PreconditionFailed,
    /// The same operation id was used with different content.
    IdempotencyKeyReused,
    /// The same operation is still running.
    ExecutionInProgress,
    /// The stored result of this operation id was purged; it is not run again.
    OperationResultExpired,
    /// A command or field the first slice does not support.
    UnsupportedOperation,
    /// `exec_mode` other than `atomic`.
    UnsupportedExecutionMode,
    /// A required field is empty in the final state; only readable fields are named.
    RequiredFieldMissing,
    /// Workflow or field rules do not allow changing the field.
    ReadonlyField,
    /// Not an allowed status; `details.allowed` lists the caller's transitions.
    InvalidTransition,
    /// Duplicate of itself, or across projects.
    RelationNotAllowed,
    /// Contract 05 §3.10.1: the issue is not waiting in Triage (or, for
    /// `unsnooze`, not snoozed); `details.state` is its triage state.
    TriageStateInvalid,
    /// The newly chosen version is locked or closed (§17.3 R1).
    VersionNotOpen,
    /// Deleting a version still used by issues the caller can see (R5).
    VersionInUse,
    /// A version with the same name exists in the project (R8).
    VersionNameTaken,
    /// Not the current version of the ChangeSet.
    VersionSuperseded,
    /// The proposal hash does not match, or the content was redacted.
    ApprovalInvalidated,
    /// What was reviewed is no longer what would run (§15.3, §15.4).
    ReviewOutdated,
    /// §16.7: nothing of the execution can be undone now (SPA only).
    UndoNotApplicable,
    /// §19.8: the repository is unknown, not opened for linking, or the App
    /// cannot reach it (one shape for all three).
    RepoNotLinkable,
    /// §19.8: the project already links this repository.
    RepoAlreadyLinked,
    /// §19.8: the project links 20 repositories already (R-G9).
    RepoLinkLimit,
    /// This version was applied already; `details.execution_id`.
    AlreadyApplied,
    /// Rejected, canceled or succeeded.
    ChangesetNotExecutable,
    /// The delegator can no longer read the whole proposal.
    ProposalUnavailable,
    /// The ChangeSet expired.
    ChangesetExpired,
    /// The write would reach readers outside every source project (§3.8).
    AudienceExpansionForbidden,
    /// An evidence citation did not verify as `valid` (§6.7 step 7).
    SourceUnverifiable,
    /// A citation id cannot be decoded or has an unsupported kind (03 §3.7).
    InvalidCitation,
    /// The quote is not what the cited range says (03 §3.6).
    CitationMismatch,
    /// A proposal or write quota is used up (§8.9, §16.6).
    QuotaExceeded,
    /// The circuit breaker paused this agent work context (§16.6).
    AgentSuspended,
    /// `q` has non-blank characters but no searchable term (design 03 §3.7).
    InvalidQuery,
    /// The search index is broken or its worker stopped (not ordinary lag).
    SearchUnavailable,
    /// `consistent_after` is not a `c1.<epoch>.<xid>` token (03 §4.11).
    InvalidConsistencyToken,
    /// The token's data epoch is not the current one (03 §4.3): search
    /// again without it; never re-propose or ask for approval again.
    ConsistencyEpochChanged,
    /// `on_lag: "error"` and the index has not caught up; `retry_after_ms`.
    SearchNotCaughtUp,
    /// A filter names a custom field definition that changed since (03 §3.7).
    FieldDefinitionChanged,
    /// The query did not finish within the request's work budget (03 §5.4).
    QueryTimeout,
    /// A view definition is not valid (design 05 §5): `details.path`,
    /// `details.reason` (`unknown_field`, `invalid_shape`, `invalid_value`,
    /// `kind_mismatch`, `lanes_required`, `restricted_field`,
    /// `project_scope`, `too_large`, `duplicate_value`).
    InvalidView,
    /// Another view of the same scope has this name; `details.view_id`.
    ViewNameTaken,
    /// A project tab still shows this view; `details.tabs` (tab titles).
    ViewInUse,
    /// Contract 05 §3.6: `expected_rules_version` is not the rules version
    /// in force; `details.current`. Describe the form again.
    FormRulesChanged,
    /// A form draft is not valid: `details.kind` (`unknown_field`,
    /// `restricted_condition_field`, `condition_too_complex`, `cycle`,
    /// `hides_required`, `field_not_applicable`, `malformed`,
    /// `invalid_rule`), `details.path`, `rule_id` or `section_id`.
    FormInvalid,
    /// `mode = block` and existing issues break the rules; `details` is the
    /// impact report.
    FormPublishBlocked,
    /// Design 05 §3.9: the label already belongs (active) to another
    /// concept; `details.concept_id`, `details.pref_label`, `details.label`.
    ConceptLabelTaken,
    /// Design 05 §5: a role or delegation for a runtime whose work context
    /// is closed; start a new work context first.
    RuntimeContextClosed,
    /// Design 05 §5 (Q2): the agent software already runs in another active
    /// runtime of this sponsor; `details.runtime_id`, `details.label`.
    RuntimeClientTaken,
    /// Design 05 §5: the built-in AI cannot run now; `details.kind` is
    /// `disabled`, `circuit_open`, `not_configured` or `endpoint_blocked`.
    AiUnavailable,
    /// The AI provider failed; `details.kind` is `timeout`, `http_status`
    /// (with `status`) or `malformed`. Never the provider's own text.
    AiProviderError,
    /// The model or the endpoint's host is on the D17 list; `details.field`.
    AiModelDisallowed,
    /// `details.kind`: `unregistered`, `no_key` or `endpoint_blocked`.
    AiEndpointInvalid,
    /// The conversation became read-only (a new work context); start a new one.
    AiConversationReadOnly,
    /// The provider in use changed since this conversation's last run;
    /// resend with `ack_provider_id`. `details.provider`.
    AiProviderChanged,
    /// The caller already has a run queued or running.
    AiRunInProgress,
    /// Design 06 §5: the body is over a size limit (`POST /markdown/preview`
    /// text, later uploads, imports and wiki pages); `details.limit` in bytes.
    PayloadTooLarge,
    /// Design 06 §5 (R-A2): the disk of the attachment directory is below
    /// its free-space threshold; try again later.
    InsufficientStorage,
    /// Design 06 §5: a wiki page of this project already has that title
    /// (create, rename, restore); `details.page_id`.
    WikiPageExists,
    /// Design 06 §5: the page has child pages; delete with
    /// `children: "reparent"`.
    WikiHasChildren,
    /// Design 06 R-W2: the title is not allowed; `details.reason`: `length`,
    /// `char` (with `details.char`) or `leading_dot`.
    WikiTitleInvalid,
    /// Design 06 §5 (R-F2): a fix rule is not valid; `details.reason`
    /// (`keywords_empty`, `keyword_invalid`, `keyword_duplicate`,
    /// `too_many_keywords`, `status_unknown`, `tracker_not_in_project`,
    /// `too_many_rules`), `details.index` of the rule.
    FixRuleInvalid,
    /// Design 06 R-E6: more rows than one export holds (`details.limit`).
    ExportTooLarge,
    /// Design 06 R-I2, R-I3: the uploaded file cannot be imported;
    /// `details.kind`: `encoding` (`row`), `too_many_rows`,
    /// `too_many_columns`, `cell_too_large` (`row`, `column`), `no_header`,
    /// `empty`, `parse` (`row`).
    ImportInvalidFile,
    /// Design 06 §5: the action does not fit the import's state
    /// (`details.state`).
    ImportStateInvalid,
    /// Design 06 R-I10: the settings changed after the dry run.
    ImportSettingsChanged,
    /// Design 06 R-I5: the mapping is not valid (`details.field`,
    /// `details.reason`).
    ImportMappingInvalid,
}

/// What the caller may do next (contract 02 §14.2). A closed set; the list
/// on an error names options, not steps to run in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    FixInput,
    RefreshAndRevise,
    RetrySameRequest,
    WaitAndRetry,
    AskHuman,
    Stop,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 83] = [
        Self::InvalidRequest,
        Self::Unauthenticated,
        Self::InvalidCredentials,
        Self::ReauthenticationRequired,
        Self::CsrfFailed,
        Self::HumanSessionRequired,
        Self::ForbiddenOp,
        Self::NotFound,
        Self::InvalidValue,
        Self::InvalidArgument,
        Self::InvalidFilter,
        Self::InvalidCursor,
        Self::AmbiguousReference,
        Self::ContextUnavailable,
        Self::DelegationUnavailable,
        Self::RateLimited,
        Self::TemporarilyUnavailable,
        Self::Conflict,
        Self::PreconditionFailed,
        Self::IdempotencyKeyReused,
        Self::ExecutionInProgress,
        Self::OperationResultExpired,
        Self::UnsupportedOperation,
        Self::UnsupportedExecutionMode,
        Self::RequiredFieldMissing,
        Self::ReadonlyField,
        Self::InvalidTransition,
        Self::RelationNotAllowed,
        Self::TriageStateInvalid,
        Self::VersionNotOpen,
        Self::VersionInUse,
        Self::VersionNameTaken,
        Self::VersionSuperseded,
        Self::ApprovalInvalidated,
        Self::ReviewOutdated,
        Self::UndoNotApplicable,
        Self::RepoNotLinkable,
        Self::RepoAlreadyLinked,
        Self::RepoLinkLimit,
        Self::AlreadyApplied,
        Self::ChangesetNotExecutable,
        Self::ProposalUnavailable,
        Self::ChangesetExpired,
        Self::AudienceExpansionForbidden,
        Self::SourceUnverifiable,
        Self::InvalidCitation,
        Self::CitationMismatch,
        Self::QuotaExceeded,
        Self::AgentSuspended,
        Self::InvalidQuery,
        Self::SearchUnavailable,
        Self::InvalidConsistencyToken,
        Self::ConsistencyEpochChanged,
        Self::SearchNotCaughtUp,
        Self::FieldDefinitionChanged,
        Self::QueryTimeout,
        Self::InvalidView,
        Self::ViewNameTaken,
        Self::ViewInUse,
        Self::FormRulesChanged,
        Self::FormInvalid,
        Self::FormPublishBlocked,
        Self::ConceptLabelTaken,
        Self::RuntimeContextClosed,
        Self::RuntimeClientTaken,
        Self::AiUnavailable,
        Self::AiProviderError,
        Self::AiModelDisallowed,
        Self::AiEndpointInvalid,
        Self::AiConversationReadOnly,
        Self::AiProviderChanged,
        Self::AiRunInProgress,
        Self::PayloadTooLarge,
        Self::InsufficientStorage,
        Self::WikiPageExists,
        Self::WikiHasChildren,
        Self::WikiTitleInvalid,
        Self::FixRuleInvalid,
        Self::ExportTooLarge,
        Self::ImportInvalidFile,
        Self::ImportStateInvalid,
        Self::ImportSettingsChanged,
        Self::ImportMappingInvalid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::InvalidCredentials => "invalid_credentials",
            Self::ReauthenticationRequired => "reauthentication_required",
            Self::CsrfFailed => "csrf_failed",
            Self::HumanSessionRequired => "human_session_required",
            Self::ForbiddenOp => "forbidden_op",
            Self::NotFound => "not_found",
            Self::InvalidValue => "invalid_value",
            Self::InvalidArgument => "invalid_argument",
            Self::InvalidFilter => "invalid_filter",
            Self::InvalidCursor => "invalid_cursor",
            Self::AmbiguousReference => "ambiguous_reference",
            Self::ContextUnavailable => "context_unavailable",
            Self::DelegationUnavailable => "delegation_unavailable",
            Self::RateLimited => "rate_limited",
            Self::TemporarilyUnavailable => "temporarily_unavailable",
            Self::Conflict => "conflict",
            Self::PreconditionFailed => "precondition_failed",
            Self::IdempotencyKeyReused => "idempotency_key_reused",
            Self::ExecutionInProgress => "execution_in_progress",
            Self::OperationResultExpired => "operation_result_expired",
            Self::UnsupportedOperation => "unsupported_operation",
            Self::UnsupportedExecutionMode => "unsupported_execution_mode",
            Self::RequiredFieldMissing => "required_field_missing",
            Self::ReadonlyField => "readonly_field",
            Self::InvalidTransition => "invalid_transition",
            Self::RelationNotAllowed => "relation_not_allowed",
            Self::TriageStateInvalid => "triage_state_invalid",
            Self::VersionNotOpen => "version_not_open",
            Self::VersionInUse => "version_in_use",
            Self::VersionNameTaken => "version_name_taken",
            Self::VersionSuperseded => "version_superseded",
            Self::ApprovalInvalidated => "approval_invalidated",
            Self::ReviewOutdated => "review_outdated",
            Self::UndoNotApplicable => "undo_not_applicable",
            Self::RepoNotLinkable => "repo_not_linkable",
            Self::RepoAlreadyLinked => "repo_already_linked",
            Self::RepoLinkLimit => "repo_link_limit",
            Self::AlreadyApplied => "already_applied",
            Self::ChangesetNotExecutable => "changeset_not_executable",
            Self::ProposalUnavailable => "proposal_unavailable",
            Self::ChangesetExpired => "changeset_expired",
            Self::AudienceExpansionForbidden => "audience_expansion_forbidden",
            Self::SourceUnverifiable => "source_unverifiable",
            Self::InvalidCitation => "invalid_citation",
            Self::CitationMismatch => "citation_mismatch",
            Self::QuotaExceeded => "quota_exceeded",
            Self::AgentSuspended => "agent_suspended",
            Self::InvalidQuery => "invalid_query",
            Self::SearchUnavailable => "search_unavailable",
            Self::InvalidConsistencyToken => "invalid_consistency_token",
            Self::ConsistencyEpochChanged => "consistency_epoch_changed",
            Self::SearchNotCaughtUp => "search_not_caught_up",
            Self::FieldDefinitionChanged => "field_definition_changed",
            Self::QueryTimeout => "query_timeout",
            Self::InvalidView => "invalid_view",
            Self::ViewNameTaken => "view_name_taken",
            Self::ViewInUse => "view_in_use",
            Self::FormRulesChanged => "form_rules_changed",
            Self::FormInvalid => "form_invalid",
            Self::FormPublishBlocked => "form_publish_blocked",
            Self::ConceptLabelTaken => "concept_label_taken",
            Self::RuntimeContextClosed => "runtime_context_closed",
            Self::RuntimeClientTaken => "runtime_client_taken",
            Self::AiUnavailable => "ai_unavailable",
            Self::AiProviderError => "ai_provider_error",
            Self::AiModelDisallowed => "ai_model_disallowed",
            Self::AiEndpointInvalid => "ai_endpoint_invalid",
            Self::AiConversationReadOnly => "ai_conversation_read_only",
            Self::AiProviderChanged => "ai_provider_changed",
            Self::AiRunInProgress => "ai_run_in_progress",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InsufficientStorage => "insufficient_storage",
            Self::WikiPageExists => "wiki_page_exists",
            Self::WikiHasChildren => "wiki_has_children",
            Self::WikiTitleInvalid => "wiki_title_invalid",
            Self::FixRuleInvalid => "fix_rule_invalid",
            Self::ExportTooLarge => "export_too_large",
            Self::ImportInvalidFile => "import_invalid_file",
            Self::ImportStateInvalid => "import_state_invalid",
            Self::ImportSettingsChanged => "import_settings_changed",
            Self::ImportMappingInvalid => "import_mapping_invalid",
        }
    }

    /// HTTP status for the REST API (§4.4).
    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest
            | Self::InvalidArgument
            | Self::InvalidFilter
            | Self::InvalidCursor => 400,
            Self::Unauthenticated | Self::InvalidCredentials | Self::ReauthenticationRequired => {
                401
            }
            Self::CsrfFailed | Self::HumanSessionRequired | Self::ForbiddenOp => 403,
            Self::NotFound => 404,
            Self::ContextUnavailable | Self::DelegationUnavailable => 409,
            Self::InvalidValue | Self::AmbiguousReference => 422,
            Self::RateLimited => 429,
            Self::TemporarilyUnavailable => 503,
            Self::Conflict => 409,
            Self::PreconditionFailed => 409,
            Self::IdempotencyKeyReused => 409,
            Self::ExecutionInProgress => 409,
            Self::OperationResultExpired => 410,
            Self::UnsupportedOperation => 422,
            Self::UnsupportedExecutionMode => 422,
            Self::RequiredFieldMissing => 422,
            Self::ReadonlyField => 422,
            Self::InvalidTransition => 422,
            Self::RelationNotAllowed => 422,
            Self::TriageStateInvalid => 409,
            Self::VersionNotOpen => 422,
            Self::VersionInUse => 409,
            Self::VersionNameTaken => 409,
            Self::VersionSuperseded => 409,
            Self::ApprovalInvalidated => 409,
            Self::ReviewOutdated => 409,
            Self::UndoNotApplicable => 409,
            Self::RepoNotLinkable => 422,
            Self::RepoAlreadyLinked => 409,
            Self::RepoLinkLimit => 422,
            Self::AlreadyApplied => 409,
            Self::ChangesetNotExecutable => 409,
            Self::ProposalUnavailable => 409,
            Self::ChangesetExpired => 410,
            Self::AudienceExpansionForbidden => 422,
            Self::SourceUnverifiable => 422,
            Self::InvalidCitation => 400,
            Self::CitationMismatch => 422,
            Self::QuotaExceeded => 429,
            Self::AgentSuspended => 423,
            Self::InvalidQuery => 400,
            Self::SearchUnavailable => 503,
            Self::InvalidConsistencyToken => 400,
            Self::ConsistencyEpochChanged => 409,
            Self::SearchNotCaughtUp => 503,
            Self::FieldDefinitionChanged => 409,
            Self::QueryTimeout => 422,
            Self::InvalidView => 422,
            Self::ViewNameTaken => 409,
            Self::ViewInUse => 409,
            Self::FormRulesChanged | Self::FormPublishBlocked => 409,
            Self::FormInvalid => 422,
            Self::ConceptLabelTaken => 409,
            Self::RuntimeContextClosed | Self::RuntimeClientTaken => 409,
            Self::AiUnavailable => 503,
            Self::AiProviderError => 502,
            Self::AiModelDisallowed | Self::AiEndpointInvalid => 422,
            Self::AiConversationReadOnly | Self::AiProviderChanged | Self::AiRunInProgress => 409,
            Self::PayloadTooLarge => 413,
            Self::InsufficientStorage => 507,
            Self::WikiPageExists | Self::WikiHasChildren => 409,
            Self::WikiTitleInvalid | Self::FixRuleInvalid | Self::ExportTooLarge => 422,
            Self::ImportInvalidFile | Self::ImportMappingInvalid => 422,
            Self::ImportStateInvalid | Self::ImportSettingsChanged => 409,
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::TemporarilyUnavailable
                | Self::RateLimited
                | Self::ExecutionInProgress
                | Self::SearchUnavailable
                | Self::SearchNotCaughtUp
                | Self::AiProviderError
                | Self::AiRunInProgress
                | Self::InsufficientStorage
        )
    }

    /// §14.2 table. Codes the table does not name: `invalid_credentials` →
    /// fix_input (try again), `reauthentication_required` and `csrf_failed` →
    /// ask_human (only the person in the browser can resolve them).
    pub fn next_actions(self) -> &'static [NextAction] {
        use NextAction::*;
        match self {
            Self::InvalidRequest
            | Self::InvalidValue
            | Self::InvalidArgument
            | Self::InvalidFilter
            | Self::InvalidCursor
            | Self::InvalidCredentials => &[FixInput],
            Self::Unauthenticated | Self::ReauthenticationRequired | Self::CsrfFailed => {
                &[AskHuman]
            }
            Self::HumanSessionRequired
            | Self::ForbiddenOp
            | Self::ContextUnavailable
            | Self::DelegationUnavailable => &[AskHuman, Stop],
            Self::NotFound => &[FixInput, Stop],
            Self::AmbiguousReference => &[FixInput, AskHuman],
            Self::RateLimited => &[WaitAndRetry],
            Self::TemporarilyUnavailable => &[RetrySameRequest],
            Self::Conflict => &[RefreshAndRevise],
            Self::PreconditionFailed => &[RefreshAndRevise],
            Self::IdempotencyKeyReused => &[FixInput],
            Self::ExecutionInProgress => &[WaitAndRetry],
            Self::OperationResultExpired => &[Stop],
            Self::UnsupportedOperation => &[AskHuman, Stop],
            Self::UnsupportedExecutionMode => &[AskHuman, Stop],
            Self::RequiredFieldMissing => &[FixInput],
            Self::ReadonlyField => &[FixInput],
            Self::InvalidTransition => &[FixInput, AskHuman],
            Self::RelationNotAllowed => &[AskHuman, Stop],
            Self::TriageStateInvalid => &[RefreshAndRevise, Stop],
            Self::VersionNotOpen => &[FixInput, AskHuman],
            Self::VersionInUse => &[AskHuman],
            Self::VersionNameTaken => &[FixInput, AskHuman],
            Self::VersionSuperseded => &[RefreshAndRevise],
            Self::ApprovalInvalidated => &[RefreshAndRevise],
            Self::ReviewOutdated => &[RefreshAndRevise],
            Self::UndoNotApplicable => &[Stop],
            Self::RepoNotLinkable => &[FixInput, AskHuman],
            Self::RepoAlreadyLinked => &[Stop],
            Self::RepoLinkLimit => &[AskHuman],
            Self::AlreadyApplied => &[Stop],
            Self::ChangesetNotExecutable => &[Stop],
            Self::ProposalUnavailable => &[AskHuman, Stop],
            Self::ChangesetExpired => &[Stop],
            Self::AudienceExpansionForbidden => &[AskHuman, Stop],
            Self::SourceUnverifiable => &[FixInput],
            Self::InvalidCitation => &[FixInput],
            Self::CitationMismatch => &[FixInput],
            Self::QuotaExceeded => &[AskHuman, Stop],
            Self::AgentSuspended => &[AskHuman, Stop],
            Self::InvalidQuery => &[FixInput],
            Self::SearchUnavailable => &[WaitAndRetry],
            Self::InvalidConsistencyToken | Self::ConsistencyEpochChanged => &[FixInput],
            Self::SearchNotCaughtUp => &[WaitAndRetry],
            Self::FieldDefinitionChanged => &[FixInput],
            Self::QueryTimeout => &[FixInput, AskHuman],
            Self::InvalidView | Self::ViewNameTaken => &[FixInput],
            Self::ViewInUse => &[AskHuman],
            Self::FormRulesChanged | Self::FormInvalid => &[FixInput],
            Self::FormPublishBlocked => &[AskHuman],
            Self::ConceptLabelTaken => &[FixInput, AskHuman],
            Self::RuntimeContextClosed | Self::RuntimeClientTaken => &[AskHuman],
            Self::AiUnavailable | Self::AiModelDisallowed | Self::AiEndpointInvalid => &[AskHuman],
            Self::AiProviderError => &[WaitAndRetry],
            Self::AiConversationReadOnly => &[AskHuman, Stop],
            Self::AiProviderChanged => &[AskHuman],
            Self::AiRunInProgress => &[WaitAndRetry],
            Self::PayloadTooLarge => &[FixInput],
            Self::InsufficientStorage => &[WaitAndRetry],
            Self::WikiPageExists
            | Self::WikiTitleInvalid
            | Self::FixRuleInvalid
            | Self::ExportTooLarge
            | Self::ImportInvalidFile
            | Self::ImportMappingInvalid => &[FixInput],
            // Design 06 §5: "(SPA)": the wizard shows what to do next.
            Self::ImportStateInvalid | Self::ImportSettingsChanged => &[AskHuman],
            // Design 06 §5: "(SPA)": the person decides in the dialog.
            Self::WikiHasChildren => &[AskHuman],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,
    /// Human-readable; may be localised. Never SQL, SQLSTATE or constraint names.
    pub message: String,
    /// Documented per code. Only what the caller may know: anything about a
    /// target's existence or content goes in after the visibility check.
    pub details: Option<Value>,
    pub retry_after_ms: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}
impl std::error::Error for ApiError {}

/// The error object of §4.3 (`{"error": …}` in REST, bare in MCP).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub request_id: Uuid,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
    pub next_actions: &'static [NextAction],
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            retry_after_ms: None,
        }
    }

    pub fn with_retry_after(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn body(&self, request_id: Uuid) -> ErrorBody {
        ErrorBody {
            code: self.code,
            request_id,
            retryable: self.code.retryable(),
            retry_after_ms: self.retry_after_ms,
            next_actions: self.code.next_actions(),
            message: self.message.clone(),
            details: self.details.clone(),
        }
    }
}

/// Shorthand: `Err(api(ErrorCode::NotFound, "…"))`.
pub fn api(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    ApiError::new(code, message).into()
}

/// The [`ApiError`] inside `e`, if that is what it is.
pub fn as_api_error(e: &anyhow::Error) -> Option<&ApiError> {
    e.downcast_ref::<ApiError>()
}

/// Classifies an error for the response: an `ApiError` as is; a database that
/// cannot be reached as `temporarily_unavailable`; everything else `None`
/// (internal error, logged, not shown).
pub fn classify(e: &anyhow::Error) -> Option<ApiError> {
    if let Some(a) = as_api_error(e) {
        return Some(a.clone());
    }
    // A text value PostgreSQL cannot store (NUL byte, invalid encoding): the
    // caller's input, never a server fault.
    let bad_text = e.chain().any(|c| {
        matches!(c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::Database(d)) if matches!(d.code().as_deref(), Some("22021" | "22P05")))
    });
    if bad_text {
        return Some(
            ApiError::new(
                ErrorCode::InvalidArgument,
                "a text value contains a NUL byte or invalid characters",
            )
            .with_details(serde_json::json!({"kind": "invalid_text"})),
        );
    }
    let transient = e.chain().any(|c| {
        matches!(
            c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_))
        )
    });
    transient.then(|| {
        ApiError::new(
            ErrorCode::TemporarilyUnavailable,
            "the database is not reachable right now; send the same request again",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_matches_contract_shape() {
        let id = Uuid::nil();
        let e = ApiError::new(
            ErrorCode::AmbiguousReference,
            "tracker \"Bug\" is ambiguous",
        )
        .with_details(json!({"candidates": [{"id": "a", "name": "Bug"}]}));
        let v = serde_json::to_value(e.body(id)).unwrap();
        assert_eq!(
            v,
            json!({
                "code": "ambiguous_reference",
                "request_id": "00000000-0000-0000-0000-000000000000",
                "retryable": false,
                "retry_after_ms": null,
                "next_actions": ["fix_input", "ask_human"],
                "message": "tracker \"Bug\" is ambiguous",
                "details": {"candidates": [{"id": "a", "name": "Bug"}]}
            })
        );
    }

    #[test]
    fn codes_serialize_as_their_names() {
        for c in ErrorCode::ALL {
            assert_eq!(serde_json::to_value(c).unwrap(), json!(c.as_str()));
            // §14.2: never both stop and retry_same_request.
            let a = c.next_actions();
            assert!(!(a.contains(&NextAction::Stop) && a.contains(&NextAction::RetrySameRequest)));
        }
    }

    /// Every variant's name, read from the derived JSON schema (so it
    /// follows the enum itself, not a hand-kept list).
    fn schema_codes() -> std::collections::BTreeSet<String> {
        fn walk(v: &Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                Value::Object(m) => {
                    if let Some(Value::String(s)) = m.get("const") {
                        out.insert(s.clone());
                    }
                    if let Some(Value::Array(xs)) = m.get("enum") {
                        out.extend(xs.iter().filter_map(|x| x.as_str().map(String::from)));
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                Value::Array(xs) => xs.iter().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        let mut out = Default::default();
        walk(schemars::schema_for!(ErrorCode).as_value(), &mut out);
        out
    }

    /// Golden (09b G7 (1)): code → HTTP status → retryable → next_actions,
    /// sorted by code. `ALL` must hold every variant (checked against the
    /// derived schema), so a new code always lands in the table. The front
    /// end checks its messages against the same codes (web errors.ts).
    #[test]
    fn error_code_table() {
        let all: std::collections::BTreeSet<String> = ErrorCode::ALL
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        assert_eq!(all.len(), ErrorCode::ALL.len(), "ALL has a duplicate");
        assert_eq!(all, schema_codes(), "ErrorCode::ALL misses a variant");
        let mut rows: Vec<(&str, u16, bool, String)> = ErrorCode::ALL
            .iter()
            .map(|c| {
                let next = c
                    .next_actions()
                    .iter()
                    .map(|a| {
                        serde_json::to_value(a)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                (c.as_str(), c.http_status(), c.retryable(), next)
            })
            .collect();
        rows.sort();
        let w = rows.iter().map(|r| r.0.len()).max().unwrap();
        let mut table = format!("{:w$}  status  retryable  next_actions\n", "code", w = w);
        for (code, status, retry, next) in &rows {
            table.push_str(&format!(
                "{code:w$}  {status:<6}  {:<9}  {next}\n",
                if *retry { "yes" } else { "no" },
                w = w
            ));
        }
        insta::with_settings!({
            snapshot_path => "../tests/snapshots/errors",
            prepend_module_to_snapshot => false,
            omit_expression => true,
        }, {
            insta::assert_snapshot!("error_code_table", table);
        });
    }

    #[test]
    fn classify_keeps_api_errors_and_hides_others() {
        let e = api(ErrorCode::NotFound, "no issue X-1");
        assert_eq!(classify(&e).unwrap().code, ErrorCode::NotFound);
        let e = anyhow::anyhow!("relation \"issues\" does not exist");
        assert!(classify(&e).is_none());
        let e = anyhow::Error::new(sqlx::Error::PoolTimedOut);
        assert_eq!(
            classify(&e).unwrap().code,
            ErrorCode::TemporarilyUnavailable
        );
    }
}
