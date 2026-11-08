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

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Stable error codes. Programs depend on these, never on `message`.
/// Codes of the §4.4 table arrive with the code that produces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// What the caller may do next (contract 02 §14.2). A closed set; the list
/// on an error names options, not steps to run in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    pub const ALL: [ErrorCode; 17] = [
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
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, Self::TemporarilyUnavailable | Self::RateLimited)
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
#[derive(Debug, Clone, Serialize)]
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
