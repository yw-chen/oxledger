// SPDX-License-Identifier: GPL-2.0-or-later
//! MCP server (rmcp). M0 exposes read-only tools: search, get, resolve.
//!
//! Identity: over stdio the caller is fixed at startup (API key from the
//! environment); over HTTP the axum layer authenticates the bearer key and
//! puts a [`Caller`] into the request extensions.

use crate::db::Caller;
use crate::error::{self, ApiError, ErrorCode};
use crate::service::{self, App, IssueDetail, ResolveResult, SearchParams, SearchResult};
use rmcp::RoleServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResponse, CallToolResult, ErrorData};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct OxServer {
    app: Arc<App>,
    /// Set for stdio; `None` means "take the caller from the HTTP request".
    fixed_caller: Option<Caller>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// Issue key such as "AUTH-12". Old keys of moved issues still work.
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveParams {
    /// Name, login, project key or alias to look up (e.g. "缺陷", "alice", "AUTH").
    pub text: String,
    /// Optional: project | tracker | status | principal.
    pub kind: Option<String>,
}

/// Why a tool call failed. A domain error the caller can act on is a tool
/// result with `isError: true` and the §4.3 error object as structured content
/// (contract 02 §4.6), so agents get a stable `code`, `next_actions` and
/// `details`. Anything else is a JSON-RPC error that says nothing about the
/// cause; its `request_id` is in the log.
#[derive(Debug)]
pub enum ToolError {
    Domain(ApiError),
    Protocol(ErrorData),
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        match self {
            ToolError::Domain(e) => {
                let request_id = Uuid::now_v7();
                tracing::info!(%request_id, code = e.code.as_str(), "tool error: {}", e.message);
                let body = serde_json::to_value(e.body(request_id)).map_err(|e| {
                    ErrorData::internal_error(format!("serialize error: {e}"), None)
                })?;
                Ok(CallToolResult::structured_error(body).into())
            }
            ToolError::Protocol(e) => Err(e),
        }
    }
}

fn to_error(e: anyhow::Error) -> ToolError {
    match error::classify(&e) {
        Some(a) => ToolError::Domain(a),
        None => {
            let request_id = Uuid::now_v7();
            tracing::error!(%request_id, "tool failed: {e:#}");
            ToolError::Protocol(ErrorData::internal_error(
                format!("internal error (request_id {request_id})"),
                None,
            ))
        }
    }
}

impl OxServer {
    pub fn new(app: Arc<App>, fixed_caller: Option<Caller>) -> Self {
        Self {
            app,
            fixed_caller,
            tool_router: Self::tool_router(),
        }
    }

    fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<Caller, ToolError> {
        if let Some(c) = self.fixed_caller {
            return Ok(c);
        }
        ctx.extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.extensions.get::<Caller>().copied())
            .ok_or_else(|| {
                ToolError::Domain(ApiError::new(
                    ErrorCode::Unauthenticated,
                    "not authenticated",
                ))
            })
    }
}

#[tool_router]
impl OxServer {
    #[tool(
        name = "search",
        description = "Find issues the caller can see. Combine structured filters (project, tracker, status, status_category, assignee), a custom field `filter` (conditions eq/ne/lt/le/gt/ge/between/in/is_null/not_null on fields by name, nested with and/or) and optional free text `q`. Returns one line per issue, newest first (or by relevance when `q` is given), with `next_cursor` for paging. Use `resolve` first if unsure of a name.",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SearchResult>, ToolError> {
        let caller = self.caller(&ctx)?;
        service::search(&self.app, caller, p)
            .await
            .map(Json)
            .map_err(to_error)
    }

    #[tool(
        name = "get",
        description = "Full detail of one issue by key (e.g. AUTH-12): description, people, parent, visible children, lock_version, and the statuses the caller may move it to.",
        annotations(read_only_hint = true)
    )]
    async fn get(
        &self,
        Parameters(p): Parameters<GetParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IssueDetail>, ToolError> {
        let caller = self.caller(&ctx)?;
        service::get(&self.app, caller, &p.key)
            .await
            .map(Json)
            .map_err(to_error)
    }

    #[tool(
        name = "resolve",
        description = "Turn a name or alias into ids (projects, trackers, statuses, users/groups). Matches names, logins, project keys and aliases such as 缺陷 = Bug. When nothing matches and `kind` is given, lists the valid values.",
        annotations(read_only_hint = true)
    )]
    async fn resolve(
        &self,
        Parameters(p): Parameters<ResolveParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<ResolveResult>, ToolError> {
        let caller = self.caller(&ctx)?;
        service::resolve(&self.app, caller, &p.text, p.kind.as_deref())
            .await
            .map(Json)
            .map_err(to_error)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "oxledger",
    instructions = "oxledger issue tracker (read-only in this version). Issues have keys like AUTH-12. Use `search` to find issues, `get` for detail, `resolve` to map names/aliases to ids. Results only include what the caller is allowed to see."
)]
impl ServerHandler for OxServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete(r: Result<CallToolResponse, ErrorData>) -> CallToolResult {
        match r.expect("tool result, not a protocol error") {
            CallToolResponse::Complete(c) => c,
            other => panic!("unexpected response {other:?}"),
        }
    }

    #[test]
    fn domain_error_is_a_tool_result_with_the_error_object() {
        let e = error::api(ErrorCode::NotFound, "no issue AUTH-9");
        let r = complete(Err::<Json<IssueDetail>, _>(to_error(e)).into_call_tool_result());
        assert_eq!(r.is_error, Some(true));
        let v = r.structured_content.expect("structured content");
        assert_eq!(v["code"], json!("not_found"));
        assert_eq!(v["next_actions"], json!(["fix_input", "stop"]));
        assert_eq!(v["retryable"], json!(false));
        assert!(v["request_id"].as_str().is_some_and(|s| s.len() == 36));
        assert!(v.get("error").is_none(), "MCP has no error wrapper");
    }

    #[test]
    fn internal_error_is_a_protocol_error_without_the_cause() {
        let e = anyhow::anyhow!("duplicate key value violates unique constraint issue_keys_pkey");
        let r = Err::<Json<IssueDetail>, _>(to_error(e)).into_call_tool_result();
        let err = r.expect_err("protocol error");
        assert!(!err.message.contains("issue_keys_pkey"), "{}", err.message);
        assert!(err.message.contains("request_id"), "{}", err.message);
    }
}
