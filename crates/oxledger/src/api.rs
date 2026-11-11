// SPDX-License-Identifier: GPL-2.0-or-later
//! HTTP: the SPA API under `/api/v1` (contract 02 §4), MCP at `/mcp`, and
//! `/healthz`.
//!
//! Credentials (§3.2): `/api/v1` accepts only the session cookie, `/mcp` only
//! a bearer token. A request with both is `invalid_request`
//! (`ambiguous_credentials`); a bearer on `/api/v1` is `unauthenticated`.
//!
//! Every non-GET request to `/api/v1` needs `Content-Type: application/json`
//! and `Origin` equal to the configured SPA origin; with a session it also
//! needs `X-CSRF-Token` equal to the session's token (login uses the login
//! challenge instead). Every `/api/v1` response is `Cache-Control: no-store`.

use crate::agents::{self, DelegationInput};
use crate::auth::{self, AuthContext, LoginInput, SessionPolicy};
use crate::describe;
use crate::error::{self, ApiError, ErrorCode, api};
use crate::mcp::{OxServer, ResolveParams};
use crate::service::{self, App, SearchParams};
use axum::extract::{ConnectInfo, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "__Host-ox_session";
pub const LOGIN_COOKIE: &str = "__Host-ox_login";
pub const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// The SPA's origin, e.g. `https://ledger.example.com`; non-GET requests
    /// must carry exactly this `Origin`.
    pub spa_origin: String,
    pub session: SessionPolicy,
}

pub struct ApiState {
    pub app: Arc<App>,
    pub cfg: ApiConfig,
    limiter: LoginLimiter,
}

/// Builds the whole HTTP router (GitHub off: its webhook path is 404).
pub fn router(app: Arc<App>, cfg: ApiConfig) -> Router {
    router_with_github(app, cfg, None)
}

/// The router with the GitHub webhook (§19.5) when configured. The webhook
/// sits outside `api_guard` (B21): it has no session and no CSRF.
pub fn router_with_github(
    app: Arc<App>,
    cfg: ApiConfig,
    github: Option<crate::github::GithubConfig>,
) -> Router {
    let hook = github.map(|g| crate::github::webhook::routes(app.clone(), g.webhook_secret));
    let r = base_router(app, cfg);
    match hook {
        Some(h) => h.merge(r),
        None => r,
    }
}

/// `GET /metrics` (03 §4.6, P2-D10): Prometheus text, only with
/// `Authorization: Bearer <token>`. Mounted by the server only when
/// `OXLEDGER_METRICS_TOKEN` is set; otherwise the path does not exist.
pub fn metrics_routes(app: Arc<App>, token: String) -> Router {
    let token = Arc::new(token);
    Router::new().route(
        "/metrics",
        get(move |headers: axum::http::HeaderMap| {
            let (app, token) = (app.clone(), token.clone());
            async move {
                let given = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                let ok = given.is_some_and(|g| {
                    use sha2::Digest;
                    // Compare digests: equal length, no early exit on content.
                    sha2::Sha256::digest(g.as_bytes()) == sha2::Sha256::digest(token.as_bytes())
                });
                if !ok {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                match crate::metrics::render(&app.pool, &app.index).await {
                    Ok(body) => (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "text/plain; version=0.0.4; charset=utf-8",
                        )],
                        body,
                    )
                        .into_response(),
                    Err(e) => {
                        tracing::error!("metrics: {e:#}");
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                }
            }
        }),
    )
}

fn base_router(app: Arc<App>, cfg: ApiConfig) -> Router {
    let state = Arc::new(ApiState {
        app: app.clone(),
        cfg,
        limiter: LoginLimiter::default(),
    });
    let v1 = Router::new()
        .route("/auth/login-context", get(login_context))
        .route("/auth/login", post(login))
        .route("/auth/reauthenticate", post(reauthenticate))
        .route("/auth/session", get(session))
        .route("/auth/logout", post(logout))
        .route("/auth/sessions", get(list_sessions))
        .route("/auth/sessions/{id}", delete(revoke_session))
        .route("/issues/search", post(issues_search))
        .route("/filter-fields", get(filter_fields))
        .route("/issues/{key}", get(issue_get))
        .route("/issues/{key}/actions", get(issue_actions))
        .route(
            "/projects/{id}/trackers/{tracker_id}/schema",
            get(project_tracker_schema),
        )
        .route("/projects/{id}/assignees", get(project_assignees))
        .route(
            "/projects/{id}/versions",
            get(project_versions).post(create_version),
        )
        .route(
            "/versions/{id}",
            get(version_get).patch(version_patch).delete(version_delete),
        )
        .route("/versions/{id}/impact", get(version_impact))
        .route("/projects/{id}/timeline", get(project_timeline))
        .route("/projects/{id}/tabs", get(tabs_get).put(tabs_put))
        .route("/projects/{id}/tabs/reset", post(tabs_reset))
        .route("/projects/{id}/tabs/{tab}/issues", get(tab_issues))
        .route("/projects/{id}/setting-events", get(setting_events))
        .route("/projects/{id}/triage", get(triage_project))
        .route(
            "/projects/{id}/triage-settings",
            get(triage_settings_get).put(triage_settings_put),
        )
        .route("/time-entries", get(time_list))
        .route("/time-entries/summary", get(time_summary))
        .route("/time-entries/{id}", get(time_get))
        .route("/time-entries/{id}/events", get(time_events))
        .route("/me/timesheet", get(time_sheet))
        .route(
            "/me/preferences",
            get(me_preferences).put(me_preferences_put),
        )
        .route("/me/home", get(me_home).put(me_home_put))
        .route(
            "/me/notification-preferences",
            get(notification_prefs).put(notification_prefs_put),
        )
        .route("/notifications", get(notifications_list))
        .route("/notifications/unread-count", get(notifications_unread))
        .route("/notifications/read-all", post(notifications_read_all))
        .route("/notifications/{id}/read", post(notification_read))
        .route("/notifications/{id}/unread", post(notification_unread))
        .route(
            "/issues/{key}/watch",
            axum::routing::put(issue_watch).delete(issue_unwatch),
        )
        .route("/me/home/apply-template", post(me_home_apply))
        .route("/home/templates", get(home_templates))
        .route("/time-activities", get(time_activities))
        .route("/admin/time-activities", put(time_activities_put))
        .route(
            "/projects/{id}/time-activities",
            get(time_project_settings).put(time_project_settings_put),
        )
        .route("/projects/{id}/time-users", get(time_users))
        .route(
            "/org/timelog-settings",
            get(timelog_settings_get).put(timelog_settings_put),
        )
        .route("/triage", get(triage_mine))
        .route("/triage/counts", get(triage_counts))
        .route("/views", get(views_list).post(views_create))
        .route("/views/preview", post(views_preview))
        .route(
            "/views/{id}",
            get(views_get).patch(views_update).delete(views_delete),
        )
        .route("/views/{id}/copy", post(views_copy))
        .route("/views/{id}/run", post(views_run))
        .route("/views/{id}/events", get(views_events))
        .route("/projects/{id}/repos", get(repos_list).post(repos_link))
        .route("/projects/{id}/repos/linkable", get(repos_linkable))
        .route("/projects/{id}/repos/{link_id}", delete(repos_unlink))
        .route("/projects/{id}/dev", get(project_dev))
        .route(
            "/projects/{id}/dev/prs/{pr_id}/hide",
            post(pr_hide).delete(pr_unhide),
        )
        .route("/projects/{id}/dev/revoke", post(dev_revoke))
        .route("/issues/{key}/notes/{note_id}/redact", post(redact_note))
        .route(
            "/issues/{key}/events/{event_id}/redact",
            post(redact_issue_event),
        )
        .route(
            "/versions/{id}/events/{seq}/redact",
            post(redact_version_event),
        )
        .route(
            "/projects/{id}/setting-events/{event_id}/redact",
            post(redact_setting_event),
        )
        .route("/time-entries/{id}/redact", post(redact_time_entry))
        .route("/issues/{key}/dev", get(issue_dev))
        .route("/admin/github", get(admin_github_get).put(admin_github_put))
        .route(
            "/projects/{id}/schedule-settings",
            get(schedule_settings_get).put(schedule_settings_put),
        )
        .route(
            "/org/schedule-settings",
            get(org_schedule_settings_get).put(org_schedule_settings_put),
        )
        .route("/versions/{id}/events", get(version_events))
        .route("/resolve", post(resolve))
        .route("/commands", post(submit_command))
        .route("/commands/validate", post(validate_command))
        .route("/projects/{id}/trackers/{t}/form", get(form_describe))
        .route("/admin/forms", get(forms_list).post(forms_ensure))
        .route("/forms/{id}", get(form_get))
        .route("/forms/{id}/versions", get(form_versions))
        .route("/forms/{id}/drafts/{part}", put(form_draft_put))
        .route("/forms/{id}/drafts/{part}/impact", post(form_impact))
        .route("/forms/{id}/drafts/{part}/publish", post(form_publish))
        .route("/forms/{id}/drafts/{part}/discard", post(form_discard))
        .route("/forms/{id}/rollback", post(form_rollback))
        .route("/admin/custom-fields", get(fields_list).post(fields_create))
        .route(
            "/admin/custom-fields/{id}",
            get(field_get).patch(field_patch).delete(field_delete),
        )
        .route("/admin/custom-fields/{id}/options", post(field_option_add))
        .route(
            "/admin/custom-fields/{id}/options/{oid}",
            patch(field_option_patch).delete(field_option_delete),
        )
        .route(
            "/admin/custom-fields/{id}/conversions",
            post(field_conversion_preview),
        )
        .route("/admin/concepts", get(concepts_list).post(concepts_create))
        .route(
            "/admin/concepts/{id}",
            get(concept_get).patch(concept_patch),
        )
        .route("/admin/concepts/{id}/labels", post(concept_label_add))
        .route(
            "/admin/concepts/{id}/labels/{label}",
            patch(concept_label_patch),
        )
        .route("/admin/conversions/{id}", get(conversion_get))
        .route("/admin/conversions/{id}/start", post(conversion_start))
        .route("/changesets", get(changeset_list))
        .route("/changesets/{id}", get(changeset_get))
        .route("/changesets/{id}/versions/{v}", get(changeset_version))
        .route("/changesets/{id}/validations", post(changeset_validate))
        .route("/changesets/{id}/approve-and-apply", post(changeset_apply))
        .route("/changesets/{id}/reject", post(changeset_reject))
        .route("/changesets/{id}/cancel", post(changeset_cancel))
        .route("/changesets/{id}/revisions", post(changeset_revise))
        .route(
            "/changesets/{id}/revision-previews",
            post(changeset_preview),
        )
        .route(
            "/changesets/{id}/revise-and-apply",
            post(changeset_revise_apply),
        )
        .route("/executions/{id}", get(execution_get))
        .route("/executions/{id}/undo-preview", get(undo_preview))
        .route("/executions/{id}/undo", post(undo_apply))
        .route("/agents/{id}/activity", get(agent_activity))
        .route(
            "/agents/{id}/incident-recovery/preview",
            post(incident_preview),
        )
        .route("/home/agent-summary", get(agent_summary))
        .route("/operations/{id}", get(operation_get))
        .route("/agent-clients", get(agent_clients))
        .route(
            "/delegations",
            get(list_delegations).post(create_delegation),
        )
        .route("/delegations/{id}/revoke", post(revoke_delegation))
        .route(
            "/delegations/{id}/read-own-time",
            put(delegation_read_own_time),
        )
        .route(
            "/delegations/{id}/policies",
            get(delegation_policies).put(set_delegation_policies),
        )
        .route("/agent-contexts", get(list_contexts).post(new_context))
        .route("/agent-contexts/{id}/close", post(close_context))
        .route("/agent-contexts/{id}/suspend", post(suspend_context))
        .route("/agent-contexts/{id}/resume", post(resume_context))
        .route("/agent-contexts/{id}/credentials", post(issue_credential))
        .route("/agent-credentials/{id}/revoke", post(revoke_credential))
        .fallback(|| async { ApiFailure(api(ErrorCode::NotFound, "no such endpoint")) })
        .method_not_allowed_fallback(|| async {
            ApiFailure(
                ApiError::new(ErrorCode::InvalidRequest, "this method is not allowed here")
                    .with_details(json!({"kind": "method_not_allowed"}))
                    .into(),
            )
        })
        .layer(middleware::from_fn_with_state(state.clone(), api_guard))
        .with_state(state.clone());

    let mcp_app = app.clone();
    let mcp_origin = state.cfg.spa_origin.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(OxServer::new(mcp_app.clone(), None, mcp_origin.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let mcp = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(state, mcp_auth));

    Router::new()
        .nest("/api/v1", v1)
        .merge(mcp)
        .route("/healthz", get(|| async { "ok" }))
}

// ---------------------------------------------------------------- responses

/// `{"data": …, "meta": {…}}` (§4.3).
fn ok(data: impl serde::Serialize) -> Response {
    Json(json!({
        "data": data,
        "meta": {"request_id": Uuid::now_v7(), "observed_at": Utc::now()},
    }))
    .into_response()
}

/// [`ok`] with 201 Created (design 05 §3.3: a new view; a replay of the
/// same Idempotency-Key answers the same way).
fn created(data: impl serde::Serialize) -> Response {
    let mut r = ok(data);
    *r.status_mut() = StatusCode::CREATED;
    r
}

/// Any handler error. Domain errors become `{"error": …}` with their HTTP
/// status; everything else a 500 that names only a request id.
pub struct ApiFailure(pub anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiFailure {
    fn from(e: E) -> Self {
        ApiFailure(e.into())
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let request_id = Uuid::now_v7();
        match error::classify(&self.0) {
            Some(e) => {
                let status = StatusCode::from_u16(e.code.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let mut resp = (status, Json(json!({"error": e.body(request_id)}))).into_response();
                if let Some(ms) = e.retry_after_ms {
                    let secs = ms.div_ceil(1000).to_string();
                    if let Ok(v) = HeaderValue::from_str(&secs) {
                        resp.headers_mut().insert(header::RETRY_AFTER, v);
                    }
                }
                resp
            }
            None => {
                tracing::error!(%request_id, "request failed: {:#}", self.0);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {
                        "code": "internal_error", "request_id": request_id, "retryable": false,
                        "retry_after_ms": null, "next_actions": ["stop"],
                        "message": "internal error",
                    }})),
                )
                    .into_response()
            }
        }
    }
}

type ApiResult = Result<Response, ApiFailure>;

/// JSON body with unknown fields rejected as `invalid_request` (§4.3, T-A2).
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiFailure;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(ApiFailure(
                ApiError::new(ErrorCode::InvalidRequest, e.body_text())
                    .with_details(json!({"kind": "invalid_body"}))
                    .into(),
            )),
        }
    }
}

fn id_param(s: &str) -> Result<Uuid, ApiFailure> {
    s.parse()
        .map_err(|_| ApiFailure(api(ErrorCode::NotFound, "no such object")))
}

// ---------------------------------------------------------------- cookies

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn set_cookie(resp: &mut Response, name: &str, value: &str, max_age_secs: i64, same_site: &str) {
    let v = format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite={same_site}; Max-Age={max_age_secs}"
    );
    if let Ok(v) = HeaderValue::from_str(&v) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

// ---------------------------------------------------------------- guard

/// What the guard found, for the handlers.
#[derive(Clone)]
pub struct RequestAuth {
    pub ctx: AuthContext,
    pub csrf_secret: Option<Vec<u8>>,
    pub client_ip: Option<String>,
    /// The raw session cookie the request carried (valid or not).
    pub had_session_cookie: bool,
}

fn has_bearer(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().len() >= 6 && v[..6].eq_ignore_ascii_case("bearer"))
}

async fn api_guard(State(st): State<Arc<ApiState>>, mut req: Request, next: Next) -> Response {
    let mut resp = match guard(&st, &mut req).await {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn guard(st: &ApiState, req: &mut Request) -> Result<(), ApiFailure> {
    let headers = req.headers();
    let session_cookie = cookie(headers, SESSION_COOKIE);
    if has_bearer(headers) {
        if session_cookie.is_some() {
            return Err(ApiFailure(
                ApiError::new(
                    ErrorCode::InvalidRequest,
                    "send either a session cookie or a bearer token, not both",
                )
                .with_details(json!({"kind": "ambiguous_credentials"}))
                .into(),
            ));
        }
        return Err(ApiFailure(api(
            ErrorCode::Unauthenticated,
            "this API only accepts a browser session",
        )));
    }
    let info = match &session_cookie {
        Some(c) => auth::session_from_cookie(&st.app.pool, &st.cfg.session, c).await?,
        None => None,
    };
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string());

    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let csrf = |msg: &str| ApiFailure(api(ErrorCode::CsrfFailed, msg));
        let json_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
        if !json_type {
            return Err(csrf("Content-Type must be application/json"));
        }
        let origin_ok = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| o == st.cfg.spa_origin);
        if !origin_ok {
            return Err(csrf("request origin is not allowed"));
        }
        // Login has its own challenge; everything else with a session needs the session token.
        let is_login = req.uri().path().ends_with("/auth/login");
        if let (Some(i), false) = (&info, is_login) {
            let presented = headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !auth::token_matches(presented, &i.csrf_secret) {
                return Err(csrf("missing or wrong CSRF token"));
            }
        }
    }
    let ra = RequestAuth {
        ctx: info.as_ref().map_or(AuthContext::anonymous(), |i| i.ctx),
        csrf_secret: info.map(|i| i.csrf_secret),
        client_ip,
        had_session_cookie: session_cookie.is_some(),
    };
    req.extensions_mut().insert(ra);
    Ok(())
}

/// The signed-in person or `unauthenticated`.
fn human(ra: &RequestAuth) -> Result<AuthContext, ApiFailure> {
    match ra.ctx.require_human_session() {
        Ok(_) => Ok(ra.ctx),
        Err(_) => Err(ApiFailure(api(ErrorCode::Unauthenticated, "sign in first"))),
    }
}

/// MCP: bearer only (agent token or legacy key).
async fn mcp_auth(State(st): State<Arc<ApiState>>, req: Request, next: Next) -> Response {
    let mut resp = mcp_auth_inner(&st, req, next).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn mcp_auth_inner(st: &ApiState, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    if cookie(headers, SESSION_COOKIE).is_some() && has_bearer(headers) {
        return ApiFailure(
            ApiError::new(
                ErrorCode::InvalidRequest,
                "send either a session cookie or a bearer token, not both",
            )
            .with_details(json!({"kind": "ambiguous_credentials"}))
            .into(),
        )
        .into_response();
    }
    let unauthenticated = || {
        ApiFailure(api(
            ErrorCode::Unauthenticated,
            "MCP needs an agent token (or a legacy API key) as a bearer token",
        ))
        .into_response()
    };
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
    else {
        return unauthenticated();
    };
    match auth::authenticate_bearer(&st.app.pool, raw.trim()).await {
        Ok(Some(ctx)) => {
            req.extensions_mut().insert(ctx);
            next.run(req).await
        }
        Ok(None) => unauthenticated(),
        Err(e) => ApiFailure(e).into_response(),
    }
}

// ---------------------------------------------------------------- login rate limit

/// Failed logins per normalized login and per client IP, in one process
/// (§3.3). Several server processes each count separately.
#[derive(Default)]
struct LoginLimiter {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

const LIMIT_WINDOW: Duration = Duration::from_secs(15 * 60);
const MAX_PER_LOGIN: u32 = 10;
const MAX_PER_IP: u32 = 50;
/// Wrong step-up passwords per session; the session is ended at this count.
const MAX_REAUTH_FAILURES: u32 = 5;

impl LoginLimiter {
    fn keys(login: &str, ip: Option<&str>) -> Vec<(String, u32)> {
        let mut k = vec![(
            format!("login:{}", login.trim().to_lowercase()),
            MAX_PER_LOGIN,
        )];
        if let Some(ip) = ip {
            k.push((format!("ip:{ip}"), MAX_PER_IP));
        }
        k
    }

    /// `Some(ms)` to wait when any key is over its limit.
    fn blocked(&self, login: &str, ip: Option<&str>) -> Option<u64> {
        let map = self.failures.lock().unwrap();
        let now = Instant::now();
        Self::keys(login, ip)
            .into_iter()
            .filter_map(|(k, max)| {
                let (n, start) = map.get(&k)?;
                let elapsed = now.duration_since(*start);
                (elapsed < LIMIT_WINDOW && *n >= max)
                    .then(|| (LIMIT_WINDOW - elapsed).as_millis() as u64)
            })
            .max()
    }

    fn fail(&self, login: &str, ip: Option<&str>) {
        let mut map = self.failures.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (_, start)| now.duration_since(*start) < LIMIT_WINDOW);
        for (k, _) in Self::keys(login, ip) {
            let e = map.entry(k).or_insert((0, now));
            e.0 += 1;
        }
    }

    /// Counts one failure for `key`; returns the count in the window.
    fn fail_key(&self, key: &str) -> u32 {
        let mut map = self.failures.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (_, start)| now.duration_since(*start) < LIMIT_WINDOW);
        let e = map.entry(key.to_string()).or_insert((0, now));
        e.0 += 1;
        e.0
    }

    fn succeed(&self, login: &str) {
        let mut map = self.failures.lock().unwrap();
        map.remove(&format!("login:{}", login.trim().to_lowercase()));
    }
}

// ---------------------------------------------------------------- auth handlers

async fn login_context(State(st): State<Arc<ApiState>>) -> ApiResult {
    let c = auth::new_login_challenge(&st.app.pool, &st.cfg.session).await?;
    let mut resp = ok(LoginContextData {
        csrf_token: c.csrf_token,
        expires_at: c.expires_at,
    });
    set_cookie(
        &mut resp,
        LOGIN_COOKIE,
        &c.cookie,
        st.cfg.session.login_challenge.num_seconds(),
        "Strict",
    );
    Ok(resp)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoginBody {
    pub login: String,
    pub password: String,
}

async fn login(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<LoginBody>,
) -> ApiResult {
    if let Some(ms) = st.limiter.blocked(&body.login, ra.client_ip.as_deref()) {
        return Err(ApiFailure(
            ApiError::new(
                ErrorCode::RateLimited,
                "too many failed sign-ins; try again later",
            )
            .with_retry_after(ms)
            .into(),
        ));
    }
    let challenge = cookie(&headers, LOGIN_COOKIE);
    let csrf = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let result = auth::login(
        &st.app.pool,
        &st.cfg.session,
        LoginInput {
            challenge_cookie: challenge.as_deref(),
            challenge_csrf: csrf,
            login: &body.login,
            password: &body.password,
        },
    )
    .await;
    let s = match result {
        Ok(s) => s,
        Err(e) => {
            if error::as_api_error(&e).is_some_and(|a| a.code == ErrorCode::InvalidCredentials) {
                st.limiter.fail(&body.login, ra.client_ip.as_deref());
            }
            return Err(ApiFailure(e));
        }
    };
    st.limiter.succeed(&body.login);
    // Session fixation: whatever session the browser had before is ended.
    if let (Some(old), true) = (cookie(&headers, SESSION_COOKIE), ra.had_session_cookie)
        && let Some(info) = auth::session_from_cookie(&st.app.pool, &st.cfg.session, &old).await?
        && let Some(id) = info.ctx.session_id()
    {
        sqlx::query("UPDATE browser_sessions SET revoked_at = now(), revoked_reason = 'replaced_by_login' WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(&st.app.pool)
            .await?;
    }
    let max_age = (s.absolute_expires_at - Utc::now()).num_seconds().max(0);
    let mut resp = ok(LoginData {
        user_id: s.user_id,
        session_id: s.session_id,
        csrf_token: s.csrf_token,
        expires_at: s.absolute_expires_at,
    });
    set_cookie(&mut resp, SESSION_COOKIE, &s.token, max_age, "Lax");
    set_cookie(&mut resp, LOGIN_COOKIE, "", 0, "Strict");
    Ok(resp)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordBody {
    pub password: String,
}

async fn reauthenticate(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<PasswordBody>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let (_, session) = ctx.require_human_session()?;
    match auth::reauthenticate(&st.app.pool, &ctx, &body.password).await {
        Ok(()) => Ok(ok(Done::default())),
        Err(e)
            if error::as_api_error(&e).map(|a| a.code) == Some(ErrorCode::InvalidCredentials) =>
        {
            // A stolen session must not become a password oracle: after a few
            // wrong passwords the session itself ends.
            let n = st.limiter.fail_key(&format!("reauth:{session}"));
            if n >= MAX_REAUTH_FAILURES {
                auth::revoke_session(&st.app.pool, &ctx, session, "reauth_failures").await?;
                return Err(ApiFailure(api(
                    ErrorCode::Unauthenticated,
                    "too many wrong passwords; this session has ended, sign in again",
                )));
            }
            Err(ApiFailure(e))
        }
        Err(e) => Err(ApiFailure(e)),
    }
}

async fn session(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let (user, session) = ctx.require_human_session()?;
    let row: (
        String,
        String,
        bool,
        chrono::DateTime<Utc>,
        chrono::DateTime<Utc>,
    ) = sqlx::query_as(
        "SELECT p.login, p.name, p.is_admin, s.reauthenticated_at, s.absolute_expires_at
         FROM browser_sessions s JOIN principals p ON p.id = s.user_id WHERE s.id = $1",
    )
    .bind(session)
    .fetch_one(&st.app.pool)
    .await?;
    let csrf = ra.csrf_secret.as_deref().map(auth::encode);
    Ok(ok(SessionData {
        user_id: user,
        session_id: session,
        login: row.0,
        name: row.1,
        is_admin: row.2,
        csrf_token: csrf,
        reauthenticated_at: row.3,
        expires_at: row.4,
        capabilities: Capabilities {
            command_schema_versions: vec![1],
            command_features: COMMAND_FEATURES.iter().map(|f| f.to_string()).collect(),
        },
    }))
}

async fn logout(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let (_, session) = ctx.require_human_session()?;
    auth::revoke_session(&st.app.pool, &ctx, session, "logout").await?;
    let mut resp = ok(Done::default());
    set_cookie(&mut resp, SESSION_COOKIE, "", 0, "Lax");
    Ok(resp)
}

async fn list_sessions(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let (user, current) = ctx.require_human_session()?;
    let sql = format!(
        "SELECT s.id, s.created_at, s.last_seen_at, s.absolute_expires_at
         FROM browser_sessions s JOIN principals p ON p.id = s.user_id
         JOIN user_passwords up ON up.user_id = s.user_id
         WHERE s.user_id = $1 AND {} ORDER BY s.created_at DESC",
        auth::SESSION_VALID
    );
    type Row = (
        Uuid,
        chrono::DateTime<Utc>,
        chrono::DateTime<Utc>,
        chrono::DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .fetch_all(&st.app.pool)
        .await?;
    let list: Vec<SessionItem> = rows
        .into_iter()
        .map(|(id, created_at, last_seen_at, expires_at)| SessionItem {
            id,
            created_at,
            last_seen_at,
            expires_at,
            current: id == current,
        })
        .collect();
    Ok(ok(list))
}

async fn revoke_session(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    auth::revoke_session(&st.app.pool, &ctx, id_param(&id)?, "revoked_by_user").await?;
    Ok(ok(Done::default()))
}

// ---------------------------------------------------------------- issues (S2)
// The same application services as MCP (§4.6); REST adds only the session.

async fn issues_search(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(p): ApiJson<SearchParams>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(service::search(&st.app, ctx, p).await?))
}

async fn filter_fields(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::filter::v2::filter_fields(
        &st.app,
        ctx,
        q.get("project").map(String::as_str),
    )
    .await?))
}

async fn issue_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(service::get(&st.app, ctx, &key).await?))
}

async fn issue_actions(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(describe::issue_actions(&st.app, ctx, &key).await?))
}

async fn project_tracker_schema(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, tracker_id)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        describe::tracker_schema(&st.app, ctx, &id, &tracker_id).await?
    ))
}

async fn project_assignees(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(describe::assignees(&st.app, ctx, &id).await?))
}

async fn resolve(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(p): ApiJson<ResolveParams>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(p.run(&st.app, ctx).await?))
}

// ---------------------------------------------------------------- response DTOs

#[derive(Serialize, JsonSchema)]
pub struct LoginContextData {
    /// Send back as `X-CSRF-Token` with `POST /auth/login`.
    pub csrf_token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Serialize, JsonSchema)]
pub struct LoginData {
    pub user_id: Uuid,
    pub session_id: Uuid,
    /// Send as `X-CSRF-Token` with every non-GET request of this session.
    pub csrf_token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Serialize, JsonSchema)]
pub struct Capabilities {
    /// Command document versions this server accepts (§4.5).
    pub command_schema_versions: Vec<u16>,
    /// Additions to command schema version 1 this server accepts (design 05
    /// §3.10), e.g. "estimated_hours". Clients that do not use one are not
    /// affected.
    pub command_features: Vec<String>,
}

/// See [`Capabilities::command_features`]; later M1 stages add theirs.
pub const COMMAND_FEATURES: &[&str] = &["estimated_hours", "triage_decide", "log_time"];

#[derive(Serialize, JsonSchema)]
pub struct SessionData {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub login: String,
    pub name: String,
    pub is_admin: bool,
    pub csrf_token: Option<String>,
    pub reauthenticated_at: chrono::DateTime<Utc>,
    pub expires_at: chrono::DateTime<Utc>,
    pub capabilities: Capabilities,
}

#[derive(Serialize, JsonSchema)]
pub struct SessionItem {
    pub id: Uuid,
    pub created_at: chrono::DateTime<Utc>,
    pub last_seen_at: chrono::DateTime<Utc>,
    pub expires_at: chrono::DateTime<Utc>,
    /// The session making this request.
    pub current: bool,
}

/// An action that returns nothing but success.
#[derive(Serialize, JsonSchema)]
pub struct Done {
    pub ok: bool,
}

impl Default for Done {
    fn default() -> Self {
        Done { ok: true }
    }
}

#[derive(Serialize, JsonSchema)]
pub struct Created {
    pub id: Uuid,
}

// ---------------------------------------------------------------- agents

async fn agent_clients(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::list_agent_clients(&st.app.pool, &ctx).await?))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationBody {
    pub agent_client_id: Uuid,
    pub project_id: Uuid,
    pub proposal_ops: Vec<String>,
    pub writable_fields: Vec<String>,
    pub max_ops: Option<i32>,
    pub expires_in_days: Option<i64>,
    /// Modes inside the ceiling (§16.3); unlisted commands and fields are
    /// direct, except `log_time` and `update_time_entry` (review, contract 05 §4.3).
    pub policies: Option<agents::PolicyInput>,
    /// Contract 05 §3.5.4 A1: let the agent read your own time (default false).
    #[serde(default)]
    pub read_own_time: Option<bool>,
    /// Required with `read_own_time: true` (the §6.14 warning was acknowledged).
    #[serde(default)]
    pub read_own_time_ack: Option<bool>,
}

pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// The `Idempotency-Key` header: a UUID the client keeps for retries.
fn idempotency_key(headers: &HeaderMap) -> Result<Uuid, ApiFailure> {
    headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s.trim()).ok())
        .ok_or_else(|| {
            ApiFailure(
                ApiError::new(
                    ErrorCode::InvalidRequest,
                    "an Idempotency-Key header (a uuid) is required",
                )
                .with_details(json!({"kind": "idempotency_key"}))
                .into(),
            )
        })
}

/// `POST /commands` (§6.8): a person's command document.
async fn submit_command(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::commands::submit(&st.app, ctx, key, &body).await?))
}

// ---------------------------------------------------------------- changesets (S5, §8)

async fn changeset_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let group = match q.get("group") {
        None => None,
        Some(g) => Some(g.parse::<Uuid>().map_err(|_| {
            ApiError::new(ErrorCode::InvalidArgument, "group must be a group id")
                .with_details(serde_json::json!({"path": "group"}))
        })?),
    };
    Ok(ok(crate::changesets::list(
        &st.app,
        ctx,
        q.get("bucket").map(String::as_str),
        q.get("state").map(String::as_str),
        group,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

async fn changeset_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::changesets::get(
        &st.app,
        ctx,
        id_param(&id)?,
        None,
    )
    .await?))
}

async fn changeset_version(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, v)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let v: i32 = v
        .parse()
        .map_err(|_| ApiFailure(api(ErrorCode::NotFound, "no such version")))?;
    Ok(ok(crate::changesets::get(
        &st.app,
        ctx,
        id_param(&id)?,
        Some(v),
    )
    .await?))
}

async fn changeset_validate(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::changesets::ValidateInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::changesets::validate(
        &st.app,
        ctx,
        id_param(&id)?,
        &body,
    )
    .await?))
}

/// §8.6: 200 with the execution, or 202 while its result is not known.
async fn changeset_apply(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::changesets::ApproveInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let out =
        crate::changesets::approve_and_apply(&st.app, ctx, key, id_param(&id)?, &body).await?;
    Ok(apply_response(out))
}

async fn changeset_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::changesets::PreviewInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::changesets::preview_revision(
        &st.app,
        ctx,
        id_param(&id)?,
        &body,
    )
    .await?))
}

async fn changeset_revise_apply(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let out = crate::changesets::revise_and_apply(&st.app, ctx, key, id_param(&id)?, &body).await?;
    Ok(apply_response(out))
}

/// 200 with the execution, or 202 with where to look (§8.6).
fn apply_response(out: crate::changesets::ApplyOutcome) -> Response {
    if !out.pending {
        return ok(out.execution);
    }
    let mut resp = (
        StatusCode::ACCEPTED,
        Json(json!({
            "data": {"execution_id": out.execution_id, "status": "queued",
                     "client_action": "retry_same_request", "can_cancel_apply": false},
            "meta": {"request_id": Uuid::now_v7(), "observed_at": Utc::now()},
        })),
    )
        .into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&format!("/api/v1/executions/{}", out.execution_id)) {
        h.insert(header::LOCATION, v);
    }
    h.insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    resp
}

async fn changeset_reject(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::changesets::DecisionInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::changesets::reject(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
        &body,
    )
    .await?))
}

async fn changeset_cancel(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(_body): ApiJson<crate::changesets::CancelInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::changesets::cancel(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
    )
    .await?))
}

async fn changeset_revise(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::changesets::owner_revise(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
        &body,
    )
    .await?))
}

async fn execution_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::changesets::get_execution(
        &st.app,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

// ---------------------------------------------------------------- undo (S6b.4, §16.7)

fn time_param(q: &HashMap<String, String>, name: &str) -> Result<Option<DateTime<Utc>>, ApiError> {
    match q.get(name) {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::InvalidArgument,
                    format!("{name} must be an RFC 3339 time"),
                )
                .with_details(json!({"path": name}))
            }),
    }
}

async fn undo_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::undo::preview(&st.app, ctx, id_param(&id)?).await?))
}

async fn undo_apply(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::undo::UndoInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::undo::apply(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
        &body,
    )
    .await?))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentInput {
    pub since: DateTime<Utc>,
}

async fn incident_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<IncidentInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::undo::incident_preview(
        &st.app,
        ctx,
        id_param(&id)?,
        body.since,
    )
    .await?))
}

async fn agent_activity(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::undo::activity(
        &st.app,
        ctx,
        id_param(&id)?,
        time_param(&q, "from")?,
        time_param(&q, "to")?,
        q.get("issue").map(String::as_str),
    )
    .await?))
}

async fn agent_summary(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::undo::summary(&st.app, ctx).await?))
}

async fn operation_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::changesets::get_operation(
        &st.app,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

// ---------------------------------------------------------------- versions (S4, §17.8)

type Params = Query<HashMap<String, String>>;

fn int_param(q: &HashMap<String, String>, name: &str) -> Result<Option<i64>, ApiFailure> {
    q.get(name)
        .map(|v| {
            v.parse::<i64>().map_err(|_| {
                ApiFailure(api(
                    ErrorCode::InvalidArgument,
                    format!("{name} must be an integer"),
                ))
            })
        })
        .transpose()
}

// ---------------------------------------------------------------- project tabs (S6b.6, §18)

async fn tabs_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::project_tabs::get(&st.app, ctx, &id).await?))
}

async fn tabs_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::project_tabs::TabsPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::project_tabs::put(
        &st.app,
        ctx,
        &st.cfg.spa_origin,
        key,
        &id,
        &body,
    )
    .await?))
}

async fn tabs_reset(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::project_tabs::TabsReset>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::project_tabs::reset(
        &st.app, ctx, key, &id, &body,
    )
    .await?))
}

async fn tab_issues(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, tab)): Path<(String, String)>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::project_tabs::run_query(
        &st.app,
        ctx,
        &id,
        &tab,
        q.get("cursor").cloned(),
    )
    .await?))
}

async fn setting_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::project_tabs::events(
        &st.app,
        ctx,
        &id,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

// ---------------------------------------------------------------- time (design 05 §3.5, M1.4)

fn time_query(q: &HashMap<String, String>) -> Result<crate::time_reports::ListQuery, ApiFailure> {
    let activity = match q.get("activity") {
        None => None,
        Some(a) => Some(a.parse::<Uuid>().map_err(|_| {
            ApiFailure(
                ApiError::new(ErrorCode::InvalidArgument, "activity is an activity id")
                    .with_details(json!({"path": "activity"}))
                    .into(),
            )
        })?),
    };
    Ok(crate::time_reports::ListQuery {
        project: q.get("project").cloned(),
        issue: q.get("issue").cloned(),
        user: q.get("user").cloned(),
        activity,
        from: q.get("from").cloned(),
        to: q.get("to").cloned(),
        cursor: q.get("cursor").cloned(),
        limit: int_param(q, "limit")?.map(|l| l.clamp(1, 200) as u32),
    })
}

async fn time_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let lq = time_query(&q)?;
    Ok(ok(crate::time_reports::list(&st.app, ctx, &lq).await?))
}

async fn time_summary(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let sq = crate::time_reports::SummaryQuery {
        project: q.get("project").cloned(),
        from: q.get("from").cloned(),
        to: q.get("to").cloned(),
        group_by: q.get("group_by").cloned(),
        user: q.get("user").cloned(),
        issue: q.get("issue").cloned(),
    };
    Ok(ok(crate::time_reports::summary(&st.app, ctx, &sq).await?))
}

async fn time_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::get(&st.app, ctx, &id).await?))
}

async fn time_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::events(&st.app, ctx, &id).await?))
}

async fn time_sheet(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::timesheet(
        &st.app,
        ctx,
        q.get("week").map(String::as_str),
    )
    .await?))
}

// ---------------------------------------------------------------- me (design 05 §3.8, M1.6)

async fn me_preferences(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::home::preferences(&st.app, ctx).await?))
}

async fn me_preferences_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::home::PreferencesPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(
        crate::home::put_preferences(&st.app, ctx, key, &body).await?
    ))
}

async fn me_home(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::home::home(&st.app, ctx).await?))
}

async fn me_home_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::home::HomePut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::home::put_home(&st.app, ctx, key, &body).await?))
}

async fn me_home_apply(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::home::ApplyTemplate>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(
        crate::home::apply_template(&st.app, ctx, key, &body).await?
    ))
}

async fn home_templates(Extension(ra): Extension<RequestAuth>, Query(q): Params) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::home::templates(
        ctx,
        q.get("roles").map(String::as_str),
    )?))
}

async fn time_activities(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::activities(
        &st.app,
        ctx,
        q.get("project").map(String::as_str),
    )
    .await?))
}

async fn time_activities_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::time_reports::ActivitiesPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::put_site_activities(
        &st.app, ctx, &body,
    )
    .await?))
}

async fn time_project_settings(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::time_reports::project_settings(&st.app, ctx, &id).await?
    ))
}

async fn time_project_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::time_reports::ProjectTimePut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::put_project_settings(
        &st.app, ctx, &id, &body,
    )
    .await?))
}

async fn time_users(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::time_users(&st.app, ctx, &id).await?))
}

async fn timelog_settings_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::time_reports::timelog_settings(&st.app, ctx).await?
    ))
}

async fn timelog_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::time_reports::TimelogSettingsPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::time_reports::put_timelog_settings(
        &st.app, ctx, &body,
    )
    .await?))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOwnTimeBody {
    pub enabled: bool,
    /// Required (true) when enabling: the §6.14 warning was acknowledged.
    #[serde(default)]
    pub ack: bool,
}

async fn delegation_read_own_time(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<ReadOwnTimeBody>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::set_read_own_time(
        &st.app.pool,
        &st.cfg.session,
        &ctx,
        id_param(&id)?,
        b.enabled,
        b.ack,
    )
    .await?))
}

// ---------------------------------------------------------------- triage (design 05 §3.4, M1.3)

fn triage_query(q: &HashMap<String, String>) -> Result<crate::triage::ListQuery, ApiFailure> {
    let include_snoozed = match q.get("include_snoozed").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            return Err(ApiFailure(
                ApiError::new(
                    ErrorCode::InvalidArgument,
                    "include_snoozed is true or false",
                )
                .with_details(json!({"path": "include_snoozed"}))
                .into(),
            ));
        }
    };
    let decided = match q.get("view").map(String::as_str) {
        None | Some("inbox") => false,
        Some("decided") => true,
        Some(_) => {
            return Err(ApiFailure(
                ApiError::new(ErrorCode::InvalidArgument, "view is inbox or decided")
                    .with_details(json!({"path": "view"}))
                    .into(),
            ));
        }
    };
    Ok(crate::triage::ListQuery {
        include_snoozed,
        decided,
        self_triage: q.get("self_triage").cloned(),
        decided_by_kind: q.get("decided_by_kind").cloned(),
        cursor: q.get("cursor").cloned(),
        limit: int_param(q, "limit")?.map(|l| l.clamp(1, 100) as u32),
    })
}

async fn triage_project(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let lq = triage_query(&q)?;
    Ok(ok(
        crate::triage::project_inbox(&st.app, ctx, &id, &lq).await?
    ))
}

async fn triage_mine(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let lq = triage_query(&q)?;
    Ok(ok(crate::triage::my_inbox(&st.app, ctx, &lq).await?))
}

async fn triage_counts(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::triage::counts(&st.app, ctx).await?))
}

async fn triage_settings_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::triage::settings(&st.app, ctx, &id).await?))
}

async fn triage_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::triage::TriageSettingsPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::triage::put_settings(
        &st.app, ctx, key, &id, &body,
    )
    .await?))
}

// ---------------------------------------------------------------- views (design 05 §3.3, M1.2)

async fn views_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let limit = int_param(&q, "limit")?.map(|l| l.clamp(1, 1000) as u32);
    Ok(ok(crate::views::list(
        &st.app,
        ctx,
        q.get("project").map(String::as_str),
        q.get("scope").map(String::as_str),
        q.get("cursor").map(String::as_str),
        limit,
    )
    .await?))
}

async fn views_create(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::views::NewView>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(created(
        crate::views::create(&st.app, ctx, key, &body).await?,
    ))
}

async fn views_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::views::get(&st.app, ctx, &id).await?))
}

async fn views_update(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::views::ViewPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(
        crate::views::update(&st.app, ctx, key, &id, &body).await?
    ))
}

async fn views_delete(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let lv = int_param(&q, "lock_version")?
        .ok_or_else(|| ApiFailure(api(ErrorCode::InvalidArgument, "lock_version is required")))?;
    let lv = i32::try_from(lv).map_err(|_| {
        ApiFailure(api(
            ErrorCode::InvalidArgument,
            "lock_version is out of range",
        ))
    })?;
    Ok(ok(crate::views::delete(&st.app, ctx, key, &id, lv).await?))
}

async fn views_copy(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::views::ViewCopy>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(created(
        crate::views::copy(&st.app, ctx, key, &id, &body).await?,
    ))
}

async fn views_run(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::views::RunInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::views::run(&st.app, ctx, &id, &body).await?))
}

async fn views_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::views::PreviewInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::views::preview(&st.app, ctx, &body).await?))
}

async fn views_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::views::events(
        &st.app,
        ctx,
        &id,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

// ---------------------------------------------------------------- GitHub (S6b.9, §19.7)

fn uuid_path(s: &str, what: &str) -> Result<Uuid, ApiFailure> {
    Uuid::parse_str(s).map_err(|_| {
        ApiFailure(ApiError::new(ErrorCode::NotFound, format!("no such {what}")).into())
    })
}

async fn repos_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::github::links::list(&st.app, ctx, &id).await?))
}

async fn repos_linkable(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::github::links::linkable(&st.app, ctx, &id).await?))
}

async fn repos_link(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::github::links::LinkInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::github::links::link(
        &st.app, ctx, key, &id, &body,
    )
    .await?))
}

async fn repos_unlink(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, link)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let link = uuid_path(&link, "repository link")?;
    Ok(ok(crate::github::links::unlink(
        &st.app, ctx, key, &id, link,
    )
    .await?))
}

async fn project_dev(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let include_hidden = match q.get("include_hidden").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            return Err(ApiFailure(
                ApiError::new(
                    ErrorCode::InvalidArgument,
                    "include_hidden is true or false",
                )
                .with_details(json!({"path": "include_hidden"}))
                .into(),
            ));
        }
    };
    Ok(ok(crate::github::read::project_dev(
        &st.app,
        ctx,
        &id,
        q.get("filter").map(String::as_str).unwrap_or("all"),
        include_hidden,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

async fn pr_hide_set(
    st: Arc<ApiState>,
    ra: RequestAuth,
    id: String,
    pr: String,
    headers: HeaderMap,
    hidden: bool,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let pr = uuid_path(&pr, "pull request in this project")?;
    Ok(ok(crate::github::links::hide(
        &st.app, ctx, key, &id, pr, hidden,
    )
    .await?))
}

async fn pr_hide(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, pr)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult {
    pr_hide_set(st, ra, id, pr, headers, true).await
}

async fn pr_unhide(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, pr)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult {
    pr_hide_set(st, ra, id, pr, headers, false).await
}

async fn dev_revoke(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::github::links::RevokeInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::github::links::revoke(
        &st.app,
        ctx,
        &st.cfg.session,
        key,
        &id,
        &body,
    )
    .await?))
}

async fn redact(
    st: &ApiState,
    ra: &RequestAuth,
    headers: &HeaderMap,
    target: crate::redaction::Target,
    body: &crate::redaction::RedactInput,
) -> ApiResult {
    let ctx = human(ra)?;
    let key = idempotency_key(headers)?;
    Ok(ok(crate::redaction::redact(
        &st.app,
        ctx,
        &st.cfg.session,
        key,
        &target,
        body,
    )
    .await?))
}

fn seq_param(s: &str) -> Result<i64, ApiFailure> {
    s.parse::<i64>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| ApiFailure(api(ErrorCode::NotFound, "no such event")))
}

async fn redact_note(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((key, note)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let note = id_param(&note)?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::Note { key, note },
        &body,
    )
    .await
}

async fn redact_issue_event(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((key, event)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let event = id_param(&event)?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::IssueEvent { key, event },
        &body,
    )
    .await
}

async fn redact_version_event(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, seq)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let version = id_param(&id)?;
    let seq = seq_param(&seq)?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::VersionEvent { version, seq },
        &body,
    )
    .await
}

async fn redact_setting_event(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((project, event)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let event = id_param(&event)?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::SettingEvent { project, event },
        &body,
    )
    .await
}

async fn redact_time_entry(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let entry =
        id_param(&id).map_err(|_| ApiFailure(api(ErrorCode::NotFound, "no such time entry")))?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::TimeEntry { entry },
        &body,
    )
    .await
}

async fn issue_dev(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::github::read::issue_dev(&st.app, ctx, &key).await?))
}

async fn admin_github_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::github::links::admin_get(&st.app, ctx).await?))
}

async fn admin_github_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::github::links::AdminGithubPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::github::links::admin_put(&st.app, ctx, &body).await?
    ))
}

async fn project_timeline(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let include_closed = match q.get("include_closed").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                "include_closed is true or false",
            )
            .with_details(json!({"path": "include_closed"}))
            .into());
        }
    };
    Ok(ok(crate::versions::timeline(
        &st.app,
        ctx,
        &id,
        q.get("from").map(String::as_str),
        q.get("to").map(String::as_str),
        q.get("versions").map(String::as_str),
        include_closed,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

async fn schedule_settings_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::versions::schedule_settings(&st.app, ctx, &id).await?
    ))
}

async fn schedule_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::versions::ScheduleSettingsInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::versions::put_schedule_settings(
        &st.app, ctx, key, &id, &body,
    )
    .await?))
}

async fn org_schedule_settings_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::versions::org_schedule_settings(&st.app, ctx).await?
    ))
}

async fn org_schedule_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::versions::OrgScheduleSettingsInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::versions::put_org_schedule_settings(
        &st.app, ctx, key, &body,
    )
    .await?))
}

async fn project_versions(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::versions::list(
        &st.app,
        ctx,
        &id,
        q.get("status").map(String::as_str),
        q.get("include").map(String::as_str),
    )
    .await?))
}

async fn create_version(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::versions::NewVersion>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(
        crate::versions::create(&st.app, ctx, key, &id, &body).await?
    ))
}

async fn version_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::versions::get(&st.app, ctx, id_param(&id)?).await?))
}

async fn version_patch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::versions::VersionPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::versions::update(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
        &body,
    )
    .await?))
}

async fn version_delete(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let lv = int_param(&q, "lock_version")?
        .ok_or_else(|| ApiFailure(api(ErrorCode::InvalidArgument, "lock_version is required")))?;
    Ok(ok(crate::versions::delete(
        &st.app,
        ctx,
        key,
        id_param(&id)?,
        lv as i32,
    )
    .await?))
}

async fn version_impact(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::versions::impact(
        &st.app,
        ctx,
        id_param(&id)?,
        q.get("effective_date").map(String::as_str),
        q.get("status").map(String::as_str),
    )
    .await?))
}

async fn version_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let limit = int_param(&q, "limit")?.map(|l| l.clamp(1, 1000) as u32);
    Ok(ok(crate::versions::events(
        &st.app,
        ctx,
        id_param(&id)?,
        q.get("cursor").map(String::as_str),
        limit,
    )
    .await?))
}

async fn create_delegation(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(b): ApiJson<DelegationBody>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let d = agents::create_delegation(
        &st.app.pool,
        &st.cfg.session,
        &ctx,
        DelegationInput {
            agent_client_id: b.agent_client_id,
            project_id: b.project_id,
            proposal_ops: b.proposal_ops,
            writable_fields: b.writable_fields,
            max_ops: b.max_ops,
            expires_in_days: b.expires_in_days,
            policies: b.policies,
            read_own_time: b.read_own_time.unwrap_or(false),
            read_own_time_ack: b.read_own_time_ack.unwrap_or(false),
        },
    )
    .await?;
    Ok(ok(d))
}

async fn list_delegations(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::list_delegations(&st.app.pool, &ctx).await?))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

async fn revoke_delegation(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    agents::revoke_delegation(&st.app.pool, &ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn delegation_policies(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::delegation_policies(
        &st.app.pool,
        &ctx,
        id_param(&id)?,
    )
    .await?))
}

async fn set_delegation_policies(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<agents::PolicyInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::set_delegation_policies(
        &st.app.pool,
        &st.cfg.session,
        &ctx,
        id_param(&id)?,
        b,
    )
    .await?))
}

async fn suspend_context(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    agents::suspend_context(&st.app.pool, &ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn resume_context(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    agents::resume_context(&st.app.pool, &st.cfg.session, &ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn list_contexts(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(agents::list_contexts(&st.app.pool, &ctx).await?))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewContextBody {
    pub agent_client_id: Uuid,
    pub fresh_context_ack: bool,
}

async fn new_context(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(b): ApiJson<NewContextBody>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let id = agents::new_context(
        &st.app.pool,
        &st.cfg.session,
        &ctx,
        b.agent_client_id,
        b.fresh_context_ack,
    )
    .await?;
    Ok(ok(Created { id }))
}

async fn close_context(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    agents::close_context(&st.app.pool, &ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialBody {
    #[serde(default)]
    pub label: String,
    pub expires_in_days: Option<i64>,
}

async fn issue_credential(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<CredentialBody>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let c = agents::issue_credential(
        &st.app.pool,
        &st.cfg.session,
        &ctx,
        id_param(&id)?,
        &b.label,
        b.expires_in_days,
    )
    .await?;
    Ok(ok(c))
}

async fn revoke_credential(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    agents::revoke_credential(&st.app.pool, &ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

// ---------------------------------------------------------------- forms and fields (design 05 §3.6, §3.7)

/// `POST /commands/validate`: read-only, CSRF only, no Idempotency-Key.
async fn validate_command(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::forms::ValidateCommandInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::forms::validate_human(&st.app, ctx, &body).await?))
}

async fn form_describe(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, t)): Path<(String, String)>,
    Query(q): Query<crate::forms::FormQuery>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let op = q.op.as_deref().unwrap_or("create");
    Ok(ok(crate::forms::describe_form(
        &st.app,
        ctx,
        &id,
        &t,
        op,
        q.issue.as_deref(),
    )
    .await?))
}

async fn forms_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<crate::forms::FormsQuery>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::forms::list(&st.app, ctx, &q).await?))
}

async fn forms_ensure(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::forms::NewForm>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::forms::ensure(&st.app, ctx, &body).await?))
}

async fn form_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::forms::get(&st.app, ctx, id_param(&id)?).await?))
}

fn form_part(s: &str) -> Result<crate::forms::Part, ApiFailure> {
    crate::forms::Part::parse(s).map_err(ApiFailure)
}

async fn form_versions(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let part = form_part(q.get("part").map(String::as_str).unwrap_or("rules"))?;
    Ok(ok(crate::forms::versions(
        &st.app,
        ctx,
        id_param(&id)?,
        part,
    )
    .await?))
}

async fn form_draft_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, part)): Path<(String, String)>,
    ApiJson(body): ApiJson<crate::forms::DraftInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let id = id_param(&id)?;
    let part = form_part(&part)?;
    Ok(ok(
        crate::forms::put_draft(&st.app, ctx, id, part, &body).await?
    ))
}

async fn form_impact(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, part)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let id = id_param(&id)?;
    let part = form_part(&part)?;
    Ok(ok(crate::forms::impact(&st.app, ctx, id, part).await?))
}

async fn form_publish(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, part)): Path<(String, String)>,
    ApiJson(body): ApiJson<crate::forms::PublishInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let id = id_param(&id)?;
    let part = form_part(&part)?;
    Ok(ok(crate::forms::publish(
        &st.app,
        &st.cfg.session,
        ctx,
        id,
        part,
        &body,
    )
    .await?))
}

async fn form_discard(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, part)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let id = id_param(&id)?;
    let part = form_part(&part)?;
    Ok(ok(crate::forms::discard(&st.app, ctx, id, part).await?))
}

async fn form_rollback(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::forms::RollbackInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::forms::rollback(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
        &body,
    )
    .await?))
}

async fn fields_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::field_admin::list(&st.app, ctx).await?))
}

async fn fields_create(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::field_admin::NewFieldInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::field_admin::create(&st.app, ctx, &body).await?,
    ))
}

async fn field_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Query<crate::field_admin::FieldQuery>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::field_admin::get(&st.app, ctx, &id, &q).await?))
}

async fn field_patch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::field_admin::FieldPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::field_admin::patch(&st.app, ctx, &id, &body).await?
    ))
}

fn lock_version_param(q: &HashMap<String, String>) -> Result<i32, ApiFailure> {
    q.get("lock_version")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            ApiFailure(
                ApiError::new(ErrorCode::InvalidArgument, "lock_version is required")
                    .with_details(json!({"path": "lock_version"}))
                    .into(),
            )
        })
}

/// 202 with `Location`: the field stays `dropping` until the background
/// delete finishes.
async fn field_delete(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let lv = lock_version_param(&q)?;
    let v = crate::field_admin::delete(&st.app, &st.cfg.session, ctx, &id, lv).await?;
    let mut r = ok(&v);
    *r.status_mut() = StatusCode::ACCEPTED;
    if let Ok(h) = HeaderValue::from_str(&format!("/api/v1/admin/custom-fields/{}", v.id)) {
        r.headers_mut().insert(header::LOCATION, h);
    }
    Ok(r)
}

async fn field_option_add(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::field_admin::NewOption>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::field_admin::add_option(&st.app, ctx, &id, &body).await?,
    ))
}

async fn field_option_patch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, oid)): Path<(String, String)>,
    ApiJson(body): ApiJson<crate::field_admin::OptionPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let oid = id_param(&oid)?;
    Ok(ok(crate::field_admin::patch_option(
        &st.app, ctx, &id, oid, &body,
    )
    .await?))
}

async fn field_option_delete(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, oid)): Path<(String, String)>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let oid = id_param(&oid)?;
    let lv = lock_version_param(&q)?;
    Ok(ok(crate::field_admin::delete_option(
        &st.app, ctx, &id, oid, lv,
    )
    .await?))
}

async fn field_conversion_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::field_admin::ConversionInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::field_admin::preview_conversion(&st.app, ctx, &id, &body).await?,
    ))
}

async fn conversion_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::field_admin::get_conversion(
        &st.app,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

async fn conversion_start(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::field_admin::start_conversion(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

// ---------------------------------------------------------------- notifications (design 05 §3.11)

async fn notifications_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let p = crate::notifications::ListQuery {
        filter: q.get("filter").cloned(),
        project: q.get("project").cloned(),
        cursor: q.get("cursor").cloned(),
        limit: int_param(&q, "limit")?.map(|l| l.clamp(1, 100) as u32),
    };
    Ok(ok(crate::notifications::list(&st.app, ctx, &p).await?))
}

async fn notifications_unread(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::notifications::unread_count(&st.app, ctx).await?))
}

async fn notification_read(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::notifications::mark_read(&st.app, ctx, &id).await?))
}

async fn notification_unread(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::notifications::mark_unread(&st.app, ctx, &id).await?
    ))
}

async fn notifications_read_all(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::notifications::ReadAllInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::notifications::read_all(&st.app, ctx, &body).await?
    ))
}

async fn issue_watch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::notifications::watch(&st.app, ctx, &key, true).await?
    ))
}

async fn issue_unwatch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::notifications::watch(&st.app, ctx, &key, false).await?
    ))
}

async fn notification_prefs(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::notifications::preferences(&st.app, ctx).await?))
}

async fn notification_prefs_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::notifications::PreferencesPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(crate::notifications::put_preferences(
        &st.app, ctx, key, &body,
    )
    .await?))
}

// ---------------------------------------------------------------- concepts (design 05 §3.9)

async fn concepts_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let p = crate::concepts::ConceptQuery {
        q: q.get("q").cloned(),
        limit: int_param(&q, "limit")?.map(|l| l.clamp(1, 100) as u32),
        cursor: q.get("cursor").cloned(),
    };
    Ok(ok(crate::concepts::list(&st.app, ctx, &p).await?))
}

async fn concepts_create(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::concepts::NewConcept>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(crate::concepts::create(&st.app, ctx, &body).await?))
}

async fn concept_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::concepts::get(&st.app, ctx, &id).await?))
}

async fn concept_patch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::concepts::ConceptPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::concepts::patch(&st.app, ctx, &id, &body).await?))
}

async fn concept_label_add(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<crate::concepts::NewLabel>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::concepts::add_label(&st.app, ctx, &id, &body).await?,
    ))
}

async fn concept_label_patch(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, label)): Path<(String, String)>,
    ApiJson(body): ApiJson<crate::concepts::LabelPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::concepts::patch_label(
        &st.app, ctx, &id, &label, &body,
    )
    .await?))
}
