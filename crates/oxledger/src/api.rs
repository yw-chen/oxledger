// SPDX-License-Identifier: GPL-2.0-or-later
//! HTTP: the SPA API under `/api/v1` (contract 02 §4), MCP at `/mcp`,
//! `/healthz` (liveness) and `/readyz` (readiness, T2). Every route runs
//! under [`crate::telemetry::observe`]: one request id per request, in the
//! `X-Request-Id` header, the body's `request_id` and the log span.
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
    server_router(app, cfg, github, None)
}

/// What `serve` runs: the router with the GitHub webhook and `/metrics`
/// when configured, every route under the request middleware
/// ([`observed`]).
pub fn server_router(
    app: Arc<App>,
    cfg: ApiConfig,
    github: Option<crate::github::GithubConfig>,
    metrics_token: Option<String>,
) -> Router {
    let hook = github.map(|g| crate::github::webhook::routes(app.clone(), g.webhook_secret));
    let mut r = base_router(app.clone(), cfg);
    if let Some(h) = hook {
        r = h.merge(r);
    }
    if let Some(t) = metrics_token {
        r = r.merge(metrics_routes(app, t));
    }
    observed(r)
}

/// Puts [`crate::telemetry::observe`] (request id, span, HTTP metrics) on
/// every route of `r` and on its fallback. Applied once, after all routes
/// are in: axum layers only the routes that exist when `layer` is called,
/// and only there is the matched route template known.
pub fn observed(r: Router) -> Router {
    r.layer(middleware::from_fn(crate::telemetry::observe))
}

// ---------------------------------------------------------------- readiness

/// How long `/readyz` waits for the database before answering 503.
pub const READY_TIMEOUT: Duration = Duration::from_secs(2);

/// What `/readyz` found. Only these fixed words reach the response; error
/// text goes to the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Readiness {
    /// `ok` or `unavailable`.
    pub database: &'static str,
    /// `ok`, `pending` (a migration of this build is not applied) or
    /// `unknown` (could not be read).
    pub migrations: &'static str,
    /// `ok` or `not_started` (a worker this process runs has not started).
    pub workers: &'static str,
}

impl Readiness {
    pub fn ready(&self) -> bool {
        self.database == "ok" && self.migrations == "ok" && self.workers == "ok"
    }
}

/// The up-migration versions of this build.
pub fn expected_migrations() -> Vec<i64> {
    crate::db::MIGRATOR
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version)
        .collect()
}

/// How long a database check result is reused: many probes at once (one
/// per load balancer node, say) run one check per this window.
pub const READY_CACHE: Duration = Duration::from_secs(1);

/// The readiness pool's own acquire timeout: longer than
/// [`READY_TIMEOUT`], so the 2 s limit is the one that applies.
const READY_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Database checks `/readyz` actually ran (tests: the cache and the
/// single flight bound them).
pub static READY_DB_CHECKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Checks the database (one connection, two queries, [`READY_TIMEOUT`]),
/// that every version in `expected` is applied, and
/// [`crate::metrics::workers_ready`]. No cache: see [`readyz_routes`].
pub async fn readiness(pool: &sqlx::PgPool, expected: &[i64]) -> Readiness {
    let (database, migrations) = check_database(pool, expected).await;
    Readiness {
        database,
        migrations,
        workers: workers_state(),
    }
}

fn workers_state() -> &'static str {
    if crate::metrics::workers_ready() {
        "ok"
    } else {
        "not_started"
    }
}

/// (`database`, `migrations`) of [`Readiness`].
async fn check_database(pool: &sqlx::PgPool, expected: &[i64]) -> (&'static str, &'static str) {
    READY_DB_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let checked = tokio::time::timeout(READY_TIMEOUT, async {
        let mut conn = match pool.acquire().await {
            Ok(c) => c,
            Err(e) => return Err(("unavailable", "unknown", anyhow::Error::from(e))),
        };
        if let Err(e) = sqlx::query("SELECT 1").execute(&mut *conn).await {
            return Err(("unavailable", "unknown", e.into()));
        }
        let applied: Vec<i64> =
            match sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
                .fetch_all(&mut *conn)
                .await
            {
                Ok(v) => v,
                Err(e) => return Err(("ok", "unknown", e.into())),
            };
        Ok(expected.iter().all(|v| applied.contains(v)))
    })
    .await;
    match checked {
        Ok(Ok(true)) => ("ok", "ok"),
        Ok(Ok(false)) => ("ok", "pending"),
        Ok(Err((d, m, e))) => {
            crate::metrics::note_error(&e);
            tracing::warn!("readiness check failed: {e:#}");
            (d, m)
        }
        Err(_) => {
            tracing::warn!("readiness check timed out");
            ("unavailable", "unknown")
        }
    }
}

/// The state behind one `/readyz` route: its own one-connection pool and
/// the last database result.
struct ReadyProbe {
    pool: sqlx::PgPool,
    expected: Arc<[i64]>,
    /// When the last check finished and what it found. Held across a
    /// check, so concurrent probes wait for that one (single flight).
    last: tokio::sync::Mutex<Option<(std::time::Instant, &'static str, &'static str)>>,
}

impl ReadyProbe {
    async fn readiness(&self) -> Readiness {
        let (database, migrations) = {
            let mut last = self.last.lock().await;
            match *last {
                Some((at, d, m)) if at.elapsed() < READY_CACHE => (d, m),
                _ => {
                    let (d, m) = check_database(&self.pool, &self.expected).await;
                    *last = Some((std::time::Instant::now(), d, m));
                    (d, m)
                }
            }
        };
        Readiness {
            database,
            migrations,
            workers: workers_state(),
        }
    }
}

/// `GET /readyz` (T2, research 06 I-6 / A4(a)): 200 when the database
/// answers, this build's migrations are applied and the workers this
/// process runs have started; 503 otherwise. A stalled worker does not
/// make it fail (that is an alert on its metrics). The body has only
/// fixed words, never an error text, a URL or a host. `/healthz` stays the
/// liveness check (no database).
///
/// The check never borrows from the main pool (a busy pool is not "not
/// ready"): it has its own pool of one connection, opened on first use
/// with the main pool's settings and `application_name` `oxledger-readyz`.
/// Results are reused for [`READY_CACHE`] and one check runs at a time.
/// One failed check is enough for 503 (no "N failures in a row").
pub fn readyz_routes(app: Arc<App>, expected: Arc<[i64]>) -> Router {
    let options = (*app.pool.connect_options())
        .clone()
        .application_name("oxledger-readyz");
    let probe = Arc::new(ReadyProbe {
        pool: sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(READY_ACQUIRE_TIMEOUT)
            .connect_lazy_with(options),
        expected,
        last: tokio::sync::Mutex::new(None),
    });
    Router::new().route(
        "/readyz",
        get(move || {
            let probe = probe.clone();
            async move {
                let r = probe.readiness().await;
                let status = if r.ready() {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                let body = json!({
                    "status": if r.ready() { "ready" } else { "not_ready" },
                    "checks": r,
                });
                let mut resp = (status, Json(body)).into_response();
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                resp
            }
        }),
    )
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
                        crate::metrics::note_error(&e);
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
        .route("/issues/{key}/view", get(issue_view_get))
        .route("/projects", get(projects_list))
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
        .route("/markdown/preview", post(markdown_preview))
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
        .route("/wiki/search", get(wiki_search))
        .route("/projects/{id}/wiki", get(wiki_index))
        .route("/projects/{id}/wiki/pages/{title}", get(wiki_page))
        .route(
            "/projects/{id}/wiki/pages/{title}/versions",
            get(wiki_versions),
        )
        .route(
            "/projects/{id}/wiki/pages/{title}/versions/{n}",
            get(wiki_version),
        )
        .route("/projects/{id}/wiki/pages/{title}/diff", get(wiki_diff))
        .route("/projects/{id}/wiki/pages/{title}/events", get(wiki_events))
        .route(
            "/projects/{id}/wiki/pages/{title}/versions/{n}/redact",
            post(redact_wiki_version),
        )
        .route(
            "/projects/{id}/wiki/pages/{title}/title/redact",
            post(redact_wiki_title),
        )
        .route(
            "/projects/{id}/wiki/pages/{title}/events/{event}/redact",
            post(redact_wiki_event),
        )
        .route("/projects/{id}/wiki/deleted", get(wiki_deleted))
        .route(
            "/projects/{id}/wiki/deleted/{page_id}",
            get(wiki_deleted_page),
        )
        .route(
            "/projects/{id}/wiki/settings",
            get(wiki_settings_get).put(wiki_settings_put),
        )
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
        .route("/agent-credentials/{id}/revoke", post(revoke_credential))
        .route("/ai-team", get(ai_team_list).post(ai_team_create))
        .route("/ai-team/templates", get(ai_team_templates))
        .route("/ai-team/activity", get(ai_team_activity_all))
        .route("/ai-team/{id}", get(ai_team_get))
        .route("/ai-team/{id}/activity", get(ai_team_activity))
        .route("/ai-team/{id}/pause", post(ai_team_pause))
        .route("/ai-team/{id}/resume", post(ai_team_resume))
        .route("/ai-team/{id}/retire", post(ai_team_retire))
        .route(
            "/ai-team/{id}/credentials",
            get(ai_team_list_credentials).post(ai_team_credentials),
        )
        .route("/me/agent-runtimes", get(my_runtimes))
        .route("/me/agent-runtimes/{id}", patch(rename_runtime))
        .route(
            "/me/agent-runtimes/{id}/new-context",
            post(runtime_new_context),
        )
        .route("/me/agent-runtimes/{id}/retire", post(retire_runtime))
        .route("/me/agent-runtimes/{id}/suspend", post(suspend_runtime))
        .route("/me/agent-runtimes/{id}/resume", post(resume_runtime))
        .route("/ai/status", get(ai_status))
        .route("/ai/assistant", post(ai_assistant_setup))
        .route("/ai/assistant/extend", post(ai_assistant_extend))
        .route(
            "/ai/conversations",
            get(ai_conversations).post(ai_conversation_create),
        )
        .route("/ai/conversations/{id}", delete(ai_conversation_delete))
        .route(
            "/ai/conversations/{id}/messages",
            get(ai_messages).post(ai_message_send),
        )
        .route("/ai/runs/{id}", get(ai_run_get))
        .route("/ai/runs/{id}/stream", get(ai_run_stream))
        .route("/ai/runs/{id}/cancel", post(ai_run_cancel))
        .route(
            "/admin/ai/settings",
            get(ai_admin_settings).put(ai_admin_put_settings),
        )
        .route("/admin/ai/endpoints", get(ai_admin_endpoints))
        .route(
            "/admin/ai/providers",
            get(ai_admin_providers).post(ai_admin_create_provider),
        )
        .route("/admin/ai/providers/{id}", patch(ai_admin_update_provider))
        .route(
            "/admin/ai/providers/{id}/test",
            post(ai_admin_test_provider),
        )
        .route(
            "/admin/ai/providers/{id}/retire",
            post(ai_admin_retire_provider),
        )
        .route("/admin/ai/usage", get(ai_admin_usage))
        .route("/admin/ai/events", get(ai_admin_events))
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
        .merge(readyz_routes(app, expected_migrations().into()))
}

// ---------------------------------------------------------------- responses

/// `{"data": …, "meta": {…}}` (§4.3). `request_id` is the request's
/// (`X-Request-Id`, the log lines), T2.
fn ok(data: impl serde::Serialize) -> Response {
    Json(json!({
        "data": data,
        "meta": {"request_id": crate::telemetry::request_id(), "observed_at": Utc::now()},
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
        let request_id = crate::telemetry::request_id();
        // Before classify: a pool timeout is the domain error
        // `temporarily_unavailable` (503), not a 500.
        crate::metrics::note_error(&self.0);
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

/// One project the caller can see (P1 gap G2: the SPA's navigation).
#[derive(Serialize, JsonSchema, sqlx::FromRow)]
pub struct ProjectItem {
    pub id: Uuid,
    /// The issue key prefix, e.g. `AUTH`.
    pub key: String,
    pub name: String,
    /// What you may do here (P3 gap G46): which buttons to offer.
    #[sqlx(skip)]
    pub permissions: crate::perm::ProjectFlags,
}

/// `GET /projects`: the projects whose issues the caller may see (the same
/// read scope as search), by key.
async fn projects_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let mut tx = service::read_tx(&st.app).await?;
    let scope = crate::db::load_scope(&mut tx, ctx).await?;
    let ids: Vec<Uuid> = scope.project_ids().collect();
    let mut items: Vec<ProjectItem> =
        sqlx::query_as("SELECT id, key, name FROM projects WHERE id = ANY($1) ORDER BY key")
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
    let actor = crate::perm::actor(&mut tx, ctx).await?;
    let mut flags = crate::perm::projects_flags(&mut tx, &actor, &ids).await?;
    for p in &mut items {
        p.permissions = flags.remove(&p.id).unwrap_or_default();
    }
    tx.commit().await?;
    Ok(ok(items))
}

/// `GET /issues/{key}/view?include=actions,recent_notes,recent_events,dev&note_limit=&event_limit=`:
/// every `get` field, the full detail, the ids an edit form needs and the
/// optional blocks (P1 gap G1).
async fn issue_view_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(key): Path<String>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let limit = |name: &str| -> Result<Option<u32>, ApiFailure> {
        match q.get(name) {
            None => Ok(None),
            Some(v) => v.parse::<u32>().map(Some).map_err(|_| {
                ApiError::new(ErrorCode::InvalidArgument, format!("{name} is a number"))
                    .with_details(json!({"path": name}))
                    .into()
            }),
        }
    };
    let include = q.get("include").map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect::<Vec<_>>()
    });
    Ok(ok(crate::issue_view::spa_view(
        &st.app,
        ctx,
        &key,
        include,
        limit("note_limit")?,
        limit("event_limit")?,
    )
    .await?))
}

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
    /// Design 05 §3.1: one of the caller's roles (from `GET /ai-team`).
    #[serde(default)]
    pub agent_principal_id: Option<Uuid>,
    /// The 02 form: the `personal` role of this client (created, with its
    /// runtime, on first use). Give exactly one of the two.
    #[serde(default)]
    pub agent_client_id: Option<Uuid>,
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
            "meta": {"request_id": crate::telemetry::request_id(), "observed_at": Utc::now()},
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
    Ok(ok(crate::undo::activity_page(
        &st.app,
        ctx,
        id_param(&id)?,
        &activity_query(&q)?,
    )
    .await?))
}

/// G24: `from`, `to` (RFC 3339), `issue` (a key), `cursor`, `limit` (1–200).
fn activity_query(q: &HashMap<String, String>) -> Result<crate::undo::ActivityQuery, ApiFailure> {
    Ok(crate::undo::ActivityQuery {
        from: time_param(q, "from")?,
        to: time_param(q, "to")?,
        issue: q.get("issue").cloned(),
        cursor: q.get("cursor").cloned(),
        limit: int_param(q, "limit")?
            .map(|l| l.clamp(1, crate::undo::MAX_ACTIVITY_PAGE as i64) as u32),
    })
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
        tracker: q.get("tracker").cloned(),
        fixed_version: q.get("fixed_version").cloned(),
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

/// `POST /markdown/preview` (design 06 §3.1, R-RF6).
async fn markdown_preview(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(body): ApiJson<crate::markdown::MarkdownPreviewInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::markdown::preview(&st.app, ctx, &body).await?))
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

// ---------------------------------------------------------------- wiki (design 06 §3.1, M2.2)

fn version_param(s: &str) -> Result<i32, ApiFailure> {
    s.parse::<i32>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| ApiFailure(api(ErrorCode::NotFound, "no such version")))
}

fn opt_version(q: &HashMap<String, String>, name: &str) -> Result<Option<i32>, ApiFailure> {
    match q.get(name) {
        None => Ok(None),
        Some(v) => v
            .parse::<i32>()
            .ok()
            .filter(|n| *n >= 0)
            .map(Some)
            .ok_or_else(|| {
                ApiFailure(
                    ApiError::new(
                        ErrorCode::InvalidArgument,
                        format!("{name} is a version number"),
                    )
                    .with_details(json!({"path": name}))
                    .into(),
                )
            }),
    }
}

async fn wiki_index(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::index(&st.app, ctx, &id).await?))
}

async fn wiki_page(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, title)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::page(&st.app, ctx, &id, &title).await?))
}

async fn wiki_search(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let wait_ms = match int_param(&q, "wait_ms")? {
        None => None,
        Some(v) => Some(u32::try_from(v).map_err(|_| {
            ApiFailure(api(
                ErrorCode::InvalidArgument,
                format!("wait_ms {v}: use 0..=2000"),
            ))
        })?),
    };
    let p = crate::wiki_search::WikiSearchParams {
        q: q.get("q").cloned(),
        project: q.get("project").cloned(),
        cursor: q.get("cursor").cloned(),
        consistent_after: q.get("consistent_after").cloned(),
        wait_ms,
        on_lag: q.get("on_lag").cloned(),
    };
    Ok(ok(crate::wiki_search::search(&st.app, ctx, p).await?))
}

async fn wiki_versions(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, title)): Path<(String, String)>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::versions(
        &st.app,
        ctx,
        &id,
        &title,
        q.get("cursor").map(String::as_str),
    )
    .await?))
}

async fn wiki_version(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, title, n)): Path<(String, String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let n = version_param(&n)?;
    Ok(ok(crate::wiki::version(&st.app, ctx, &id, &title, n).await?))
}

async fn wiki_diff(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, title)): Path<(String, String)>,
    Query(q): Params,
) -> ApiResult {
    let ctx = human(&ra)?;
    let from = opt_version(&q, "from")?;
    let to = opt_version(&q, "to")?;
    Ok(ok(crate::wiki::page_diff(
        &st.app, ctx, &id, &title, from, to,
    )
    .await?))
}

async fn wiki_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, title)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::events(&st.app, ctx, &id, &title).await?))
}

async fn wiki_deleted(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::deleted(&st.app, ctx, &id).await?))
}

async fn wiki_deleted_page(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((id, page)): Path<(String, String)>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let page = id_param(&page).map_err(|_| ApiFailure(crate::wiki::not_found()))?;
    Ok(ok(crate::wiki::deleted_page(&st.app, ctx, &id, page).await?))
}

async fn wiki_settings_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::wiki::settings(&st.app, ctx, &id).await?))
}

async fn wiki_settings_put(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::wiki::WikiSettingsPut>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(ok(
        crate::wiki::put_settings(&st.app, ctx, key, &id, &body).await?
    ))
}

async fn redact_wiki_version(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((project, page, n)): Path<(String, String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let version = version_param(&n)?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::WikiVersion {
            project,
            page,
            version,
        },
        &body,
    )
    .await
}

/// `{reason_code, reason, new_title}`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiTitleRedactInput {
    pub reason_code: String,
    pub reason: String,
    /// The page's title from now on (R-W2; not "[已遮蔽]").
    pub new_title: String,
}

async fn redact_wiki_title(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((project, page)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<WikiTitleRedactInput>,
) -> ApiResult {
    let input = crate::redaction::RedactInput {
        reason_code: body.reason_code,
        reason: body.reason,
    };
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::WikiTitle {
            project,
            page,
            new_title: body.new_title,
        },
        &input,
    )
    .await
}

async fn redact_wiki_event(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path((project, page, event)): Path<(String, String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<crate::redaction::RedactInput>,
) -> ApiResult {
    let event =
        id_param(&event).map_err(|_| ApiFailure(api(ErrorCode::NotFound, "no such event")))?;
    redact(
        &st,
        &ra,
        &headers,
        crate::redaction::Target::WikiEvent {
            project,
            page,
            event,
        },
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
    let input = |client: Uuid| DelegationInput {
        agent_client_id: client,
        project_id: b.project_id,
        proposal_ops: b.proposal_ops.clone(),
        writable_fields: b.writable_fields.clone(),
        max_ops: b.max_ops,
        expires_in_days: b.expires_in_days,
        policies: b.policies.clone(),
        read_own_time: b.read_own_time.unwrap_or(false),
        read_own_time_ack: b.read_own_time_ack.unwrap_or(false),
    };
    let d = match (b.agent_principal_id, b.agent_client_id) {
        (Some(p), None) => {
            agents::create_delegation_for(
                &st.app.pool,
                &st.cfg.session,
                &ctx,
                p,
                input(Uuid::nil()),
            )
            .await?
        }
        (None, Some(c)) => {
            agents::create_delegation(&st.app.pool, &st.cfg.session, &ctx, input(c)).await?
        }
        _ => {
            return Err(ApiFailure(
                ApiError::new(
                    ErrorCode::InvalidRequest,
                    "give either agent_principal_id or agent_client_id",
                )
                .with_details(json!({"path": "agent_principal_id"}))
                .into(),
            ));
        }
    };
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

// `/agent-contexts*` (02's compatibility aliases, 05 B-43) were removed in
// M2.0 (design 06 §3.1): /me/agent-runtimes* and /ai-team* replace them.

async fn suspend_runtime(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::suspend_runtime(&st.app, ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn resume_runtime(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::resume_runtime(&st.app, &st.cfg.session, ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

// ---------------------------------------------------------------- AI team (design 05 §3.12)

async fn ai_team_list(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    // An unparsable project id lists nothing, like a missing one.
    let project = q
        .get("project")
        .map(|p| p.parse::<Uuid>().unwrap_or(Uuid::nil()));
    Ok(ok(crate::ai_team::list(
        &st.app,
        ctx,
        project,
        q.get("handle").map(String::as_str),
    )
    .await?))
}

async fn ai_team_templates(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::templates(&st.app.pool, &ctx).await?))
}

async fn ai_team_create(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<crate::ai_team::CreateRoleInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    Ok(created(
        crate::ai_team::create(&st.app, &st.cfg.session, ctx, key, &b).await?,
    ))
}

async fn ai_team_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::ai_team::detail(&st.app, ctx, id_param(&id)?).await?
    ))
}

async fn ai_team_activity(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::activity_page(
        &st.app,
        ctx,
        id_param(&id)?,
        &activity_query(&q)?,
    )
    .await?))
}

/// `GET /ai-team/activity?role=&runtime=&from=&to=&issue=&cursor=&limit=`
/// (P3 gap G42).
async fn ai_team_activity_all(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let uuid_param = |name: &str| -> Result<Option<Uuid>, ApiFailure> {
        match q.get(name) {
            None => Ok(None),
            Some(v) => v.parse::<Uuid>().map(Some).map_err(|_| {
                ApiError::new(ErrorCode::InvalidArgument, format!("{name} is an id"))
                    .with_details(json!({"path": name}))
                    .into()
            }),
        }
    };
    let (role, runtime) = (uuid_param("role")?, uuid_param("runtime")?);
    Ok(ok(crate::ai_team::activity_all(
        &st.app,
        ctx,
        role,
        runtime,
        &activity_query(&q)?,
    )
    .await?))
}

async fn ai_team_pause(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai_team::PauseInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::pause(&st.app, ctx, id_param(&id)?, &b).await?;
    Ok(ok(Done::default()))
}

async fn ai_team_resume(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai_team::ResumeInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::resume(&st.app, &st.cfg.session, ctx, id_param(&id)?, &b).await?;
    Ok(ok(Done::default()))
}

async fn ai_team_retire(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::retire(&st.app, &st.cfg.session, ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn ai_team_credentials(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai_team::CredentialInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::ai_team::issue_credential(&st.app, &st.cfg.session, ctx, id_param(&id)?, &b).await?,
    ))
}

async fn ai_team_list_credentials(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::list_credentials(
        &st.app,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

async fn my_runtimes(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::runtimes(&st.app, ctx).await?))
}

async fn rename_runtime(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai_team::RenameRuntimeInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::rename_runtime(
        &st.app,
        ctx,
        id_param(&id)?,
        &b,
    )
    .await?))
}

async fn runtime_new_context(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai_team::RuntimeNewContextInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai_team::runtime_new_context(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
        &b,
    )
    .await?))
}

async fn retire_runtime(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai_team::retire_runtime(&st.app, &st.cfg.session, ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

// ---------------------------------------------------------------- built-in AI chat (design 05 §3.13)

async fn ai_status(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::chat::status(&st.app, ctx).await?))
}

async fn ai_assistant_setup(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::chat::setup(&st.app, ctx).await?))
}

async fn ai_assistant_extend(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::ai::chat::extend(&st.app, &st.cfg.session, ctx).await?
    ))
}

async fn ai_conversations(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let limit = match q.get("limit") {
        None => None,
        Some(l) => Some(l.parse::<u32>().map_err(|_| {
            ApiFailure(
                ApiError::new(ErrorCode::InvalidArgument, "limit is a number")
                    .with_details(json!({"path": "limit"}))
                    .into(),
            )
        })?),
    };
    let query = crate::ai::chat::ConversationsQuery {
        cursor: q.get("cursor").cloned(),
        limit,
    };
    Ok(ok(crate::ai::chat::list(&st.app, ctx, &query).await?))
}

async fn ai_conversation_create(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(b): ApiJson<crate::ai::chat::NewConversation>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(crate::ai::chat::create(&st.app, ctx, &b).await?))
}

async fn ai_conversation_delete(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    crate::ai::chat::delete(&st.app, ctx, id_param(&id)?).await?;
    Ok(ok(Done::default()))
}

async fn ai_messages(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::ai::chat::messages(&st.app, ctx, id_param(&id)?).await?
    ))
}

async fn ai_message_send(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai::chat::SendInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let key = idempotency_key(&headers)?;
    let id = id_param(&id)?;
    let out = crate::ai::chat::send(&st.app, ctx, key, id, &b, &st.cfg.spa_origin).await?;
    let mut r = ok(out);
    *r.status_mut() = StatusCode::ACCEPTED;
    Ok(r)
}

async fn ai_run_get(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::ai::runner::get_run(&st.app, ctx, id_param(&id)?).await?
    ))
}

async fn ai_run_stream(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    let ctx = human(&ra)?;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    Ok(crate::ai::runner::stream(&st.app, ctx, id_param(&id)?, last).await?)
}

async fn ai_run_cancel(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(
        crate::ai::runner::cancel(&st.app, ctx, id_param(&id)?).await?
    ))
}

async fn ai_admin_settings(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::settings(&st.app, ctx).await?))
}

async fn ai_admin_put_settings(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(b): ApiJson<crate::ai::admin::SettingsInput>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::put_settings(
        &st.app,
        &st.cfg.session,
        ctx,
        &b,
    )
    .await?))
}

async fn ai_admin_endpoints(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::endpoints(&st.app, ctx).await?))
}

async fn ai_admin_providers(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::providers(&st.app, ctx).await?))
}

async fn ai_admin_create_provider(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    ApiJson(b): ApiJson<crate::ai::admin::NewProvider>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(created(
        crate::ai::admin::create_provider(&st.app, &st.cfg.session, ctx, &b).await?,
    ))
}

async fn ai_admin_update_provider(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<crate::ai::admin::ProviderPatch>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::update_provider(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
        &b,
    )
    .await?))
}

async fn ai_admin_test_provider(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::test_provider(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

async fn ai_admin_retire_provider(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Path(id): Path<String>,
    ApiJson(_): ApiJson<Empty>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::retire_provider(
        &st.app,
        &st.cfg.session,
        ctx,
        id_param(&id)?,
    )
    .await?))
}

async fn ai_admin_usage(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::usage(
        &st.app,
        ctx,
        q.get("month").map(String::as_str),
    )
    .await?))
}

async fn ai_admin_events(
    State(st): State<Arc<ApiState>>,
    Extension(ra): Extension<RequestAuth>,
) -> ApiResult {
    let ctx = human(&ra)?;
    Ok(ok(crate::ai::admin::events(&st.app, ctx).await?))
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
