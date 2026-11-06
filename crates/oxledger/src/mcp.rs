// SPDX-License-Identifier: GPL-2.0-or-later
//! MCP server (rmcp). M0 exposes read-only tools: search, get, resolve.
//!
//! Identity: over stdio the caller is fixed at startup (API key from the
//! environment); over HTTP the axum layer authenticates the bearer key and
//! puts a [`Caller`] into the request extensions.

use crate::db::Caller;
use crate::service::{self, App, IssueDetail, ResolveResult, SearchParams, SearchResult};
use rmcp::RoleServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::sync::Arc;

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

fn to_error(e: anyhow::Error) -> ErrorData {
    match service::as_user_error(&e) {
        Some(u) => ErrorData::invalid_params(u.0.clone(), None),
        None => {
            tracing::error!("tool failed: {e:#}");
            ErrorData::internal_error("internal error", None)
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

    fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<Caller, ErrorData> {
        if let Some(c) = self.fixed_caller {
            return Ok(c);
        }
        ctx.extensions
            .get::<axum::http::request::Parts>()
            .and_then(|p| p.extensions.get::<Caller>().copied())
            .ok_or_else(|| ErrorData::invalid_request("not authenticated", None))
    }
}

#[tool_router]
impl OxServer {
    #[tool(
        name = "search",
        description = "Find issues the caller can see. Combine structured filters (project, tracker, status, status_category, assignee) with optional free text `q`. Returns one line per issue, newest first (or by relevance when `q` is given), with `next_cursor` for paging. Use `resolve` first if unsure of a name.",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SearchResult>, ErrorData> {
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
    ) -> Result<Json<IssueDetail>, ErrorData> {
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
    ) -> Result<Json<ResolveResult>, ErrorData> {
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
