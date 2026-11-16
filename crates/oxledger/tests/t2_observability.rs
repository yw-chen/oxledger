// SPDX-License-Identifier: GPL-2.0-or-later
//! T2 observability (review 08 §6 D9, D11; research 06 I-1 – I-6):
//!
//! - one request id per request: `X-Request-Id`, `meta.request_id`,
//!   `error.request_id`, the `http` span and the MCP `mcp_tool` span; forged
//!   id headers are ignored, a malformed `traceparent` is not used;
//! - route labels are templates, unknown paths share one label;
//! - pool, HTTP and worker metrics on `/metrics`;
//! - `/readyz`: 503 without a database, with a pending migration, or with
//!   an expected worker not started; nothing internal in the body;
//! - the JSON log format.
//!
//! Needs PostgreSQL 18 (`common::test_pool`).

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig};
use oxledger::auth::SessionPolicy;
use oxledger::metrics::{self, HTTP, Worker};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::telemetry::{self, LogFormat};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

const TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// A `tracing` writer that keeps everything written to it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The JSON log lines (every line must parse).
    fn json(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect()
    }
}

/// The product's subscriber (`main`'s) with `RUST_LOG` = `rust_log`, on
/// this thread.
fn capture(
    format: LogFormat,
    rust_log: &str,
    allow_rmcp_debug: bool,
) -> (Captured, tracing::subscriber::DefaultGuard) {
    let buf = Captured::default();
    let sink = buf.clone();
    let sub = telemetry::subscriber(
        format,
        telemetry::env_filter(Some(rust_log)),
        allow_rmcp_debug,
        move || sink.clone(),
    );
    (buf, tracing::subscriber::set_default(sub))
}

/// A JSON-format subscriber at TRACE for every crate, on this thread.
fn capture_json() -> (Captured, tracing::subscriber::DefaultGuard) {
    capture(LogFormat::Json, "trace", false)
}

fn cfg() -> ApiConfig {
    ApiConfig {
        spa_origin: ORIGIN.into(),
        session: SessionPolicy::default(),
    }
}

fn app(pool: &sqlx::PgPool) -> Arc<App> {
    Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
        wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
    })
}

struct Resp {
    status: StatusCode,
    request_id: Option<String>,
    body: Value,
    text: String,
}

async fn send(r: &Router, req: Request<Body>) -> Resp {
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let request_id = resp
        .headers()
        .get(telemetry::REQUEST_ID_HEADER)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    Resp {
        status,
        request_id,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    }
}

fn get(uri: &str) -> axum::http::request::Builder {
    Request::get(uri)
}

/// The `http` spans in the JSON log lines.
fn http_spans(lines: &[Value]) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|l| l["spans"].as_array())
        .flatten()
        .filter(|s| s["name"] == "http")
        .cloned()
        .collect()
}

/// The id in the header is a UUIDv7 of ours, the same as in the body (data
/// and error), and forged id headers are ignored.
#[tokio::test]
async fn request_id_is_one_per_request_and_forged_ids_are_ignored() {
    let pool = test_pool().await;
    let r = api::router(app(&pool), cfg());
    let (buf, guard) = capture_json();
    let ok = send(
        &r,
        get("/api/v1/auth/login-context")
            .header("x-request-id", "FORGEDREQ")
            .header("x-correlation-id", "FORGEDCORR")
            .header("request-id", "FORGEDPLAIN")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    let id = ok.request_id.clone().expect("X-Request-Id");
    let parsed: Uuid = id.parse().unwrap();
    assert_eq!(parsed.get_version_num(), 7);
    assert_eq!(ok.body["meta"]["request_id"], json!(id));

    // An error answer: the same id in `error.request_id`.
    let err = send(
        &r,
        Request::post("/api/v1/commands")
            .header("x-request-id", "FORGEDREQ")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(err.status.is_client_error(), "{}", err.text);
    let eid = err.request_id.clone().unwrap();
    assert_ne!(eid, id, "one id per request");
    assert_eq!(err.body["error"]["request_id"], json!(eid));
    drop(guard);

    let logs = buf.text();
    for forged in ["FORGEDREQ", "FORGEDCORR", "FORGEDPLAIN"] {
        assert!(!logs.contains(forged), "{forged} reached the log:\n{logs}");
    }
    let spans = http_spans(&buf.json());
    assert!(spans.iter().any(|s| s["request_id"] == json!(id)), "{logs}");
    assert!(
        spans.iter().any(|s| s["request_id"] == json!(eid)),
        "{logs}"
    );
}

/// A valid `traceparent` gives the span its trace-id; malformed ones are
/// not used (and their text never reaches the log).
#[tokio::test]
async fn traceparent_is_used_only_when_well_formed() {
    let pool = test_pool().await;
    let r = api::router(app(&pool), cfg());
    let (buf, guard) = capture_json();
    let good = send(
        &r,
        get("/healthz")
            .header(telemetry::TRACEPARENT_HEADER, TRACE)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let mut bad_ids = Vec::new();
    for bad in [
        "00-BADTRACEUPPERCASE0000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01 BADTRACEEXTRA",
        "BADTRACEGARBAGE",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
    ] {
        let resp = send(
            &r,
            get("/healthz")
                .header(telemetry::TRACEPARENT_HEADER, bad)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        bad_ids.push(resp.request_id.unwrap());
    }
    drop(guard);
    let logs = buf.text();
    assert!(!logs.contains("BADTRACE"), "{logs}");
    let spans = http_spans(&buf.json());
    let good_id = good.request_id.unwrap();
    assert!(
        spans
            .iter()
            .any(|s| s["request_id"] == json!(good_id) && s["trace_id"] == json!(TRACE_ID)),
        "{logs}"
    );
    for id in &bad_ids {
        let mine: Vec<&Value> = spans
            .iter()
            .filter(|s| s["request_id"] == json!(id))
            .collect();
        assert!(!mine.is_empty(), "span of {id}:\n{logs}");
        assert!(mine.iter().all(|s| s.get("trace_id").is_none()), "{logs}");
    }
}

/// Route labels are templates (no keys, ids or query text), unknown paths
/// share `unmatched`, odd methods share `OTHER`.
#[tokio::test]
async fn route_labels_are_templates_without_values() {
    let pool = test_pool().await;
    let r = api::router(app(&pool), cfg());
    let (buf, guard) = capture_json();
    for uri in [
        "/api/v1/issues/ROUTEKEY-77?q=ROUTEQUERY",
        "/api/v1/versions/7c1b6a54-0000-7000-8000-ROUTEUUID000",
        "/api/v1/no-such-endpoint/ROUTEPATH1",
        "/no/such/ROUTEPATH2?x=ROUTEQUERY",
        "/mcp/ROUTEPATH3",
    ] {
        send(&r, get(uri).body(Body::empty()).unwrap()).await;
    }
    send(
        &r,
        Request::builder()
            .method("ROUTEMETHOD")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    drop(guard);
    let routes = HTTP.routes();
    for r in &routes {
        assert!(!r.contains("ROUTE"), "a value in a route label: {routes:?}");
    }
    assert!(
        routes.contains(&"/api/v1/issues/{key}".to_string()),
        "{routes:?}"
    );
    assert!(
        routes.contains(&"/api/v1/versions/{id}".to_string()),
        "{routes:?}"
    );
    assert_eq!(HTTP.count("unmatched", "GET", "4xx"), 3, "{routes:?}");
    assert_eq!(HTTP.count("/healthz", "OTHER", "4xx"), 1, "{routes:?}");
    let logs = buf.text();
    assert!(!logs.contains("ROUTE"), "{logs}");
    let spans = http_spans(&buf.json());
    assert!(
        spans
            .iter()
            .any(|s| s["route"] == "/api/v1/issues/{key}" && s["method"] == "GET"),
        "{logs}"
    );
    assert!(spans.iter().any(|s| s["route"] == "unmatched"), "{logs}");
}

/// MCP: a tool error's `request_id` is the `X-Request-Id` of the POST that
/// carried the call, and the `mcp_tool` span has it; the bearer token
/// stays out of the log.
#[tokio::test]
async fn mcp_tool_errors_carry_the_http_request_id() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (token, _) = w.token(ctx).await;
    let r = api::router(w.app.clone(), cfg());
    let post = |body: Value, session: Option<&str>| {
        let mut b = Request::post("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("x-request-id", "FORGEDMCP");
        if let Some(s) = session {
            b = b.header("mcp-session-id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let resp = r
        .clone()
        .oneshot(post(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "t2", "version": "0"}}}),
            None,
        ))
        .await
        .unwrap();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    resp.into_body().collect().await.unwrap();
    send(
        &r,
        post(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            session.as_deref(),
        ),
    )
    .await;

    let (buf, guard) = capture_json();
    let call = send(
        &r,
        post(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "get", "arguments": {"key": "NOPE-404"}}}),
            session.as_deref(),
        ),
    )
    .await;
    // A tool name that is not ours: the span says `unknown`, so the label
    // set stays the tool list.
    let odd = send(
        &r,
        post(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "no_such_tool", "arguments": {}}}),
            session.as_deref(),
        ),
    )
    .await;
    drop(guard);
    let odd_id = odd.request_id.clone().expect("X-Request-Id");
    let id = call.request_id.clone().expect("X-Request-Id");
    let answer: Value = serde_json::from_str(&call.text)
        .ok()
        .or_else(|| {
            call.text
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                .find(|v| v["id"] == json!(2))
        })
        .unwrap_or_else(|| panic!("no answer in {}", call.text));
    let err = &answer["result"]["structuredContent"];
    assert_eq!(answer["result"]["isError"], json!(true), "{answer}");
    assert_eq!(err["request_id"], json!(id), "{answer}");
    let logs = buf.text();
    assert!(!logs.contains(&token), "token in the log");
    assert!(!logs.contains("FORGEDMCP"), "{logs}");
    let tool_spans: Vec<Value> = buf
        .json()
        .iter()
        .filter_map(|l| l["spans"].as_array())
        .flatten()
        .filter(|s| s["name"] == "mcp_tool")
        .cloned()
        .collect();
    assert!(
        tool_spans
            .iter()
            .any(|s| s["request_id"] == json!(id) && s["tool"] == "get"),
        "{logs}"
    );
    let odd_spans: Vec<&Value> = tool_spans
        .iter()
        .filter(|s| s["request_id"] == json!(odd_id))
        .collect();
    assert!(
        !odd_spans.is_empty(),
        "no span for the unknown tool:\n{logs}"
    );
    assert!(
        odd_spans.iter().all(|s| s["tool"] == "unknown"),
        "{odd_spans:?}"
    );
}

/// rmcp logs whole requests and tool results at DEBUG. With `RUST_LOG=debug`
/// the product subscriber still keeps rmcp at INFO, so issue content does
/// not reach the log; `OXLEDGER_ALLOW_RMCP_DEBUG=1` is the only way in.
#[tokio::test]
async fn rmcp_debug_logs_are_capped_unless_allowed() {
    const SECRET: &str = "RMCPLEAKSUBJECT";
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "RMCPLEAKSUBJECT visible").await;
    let key: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (token, _) = w.token(ctx).await;
    let r = api::router(w.app.clone(), cfg());
    let run = async |allow: bool| {
        let (buf, guard) = capture(
            LogFormat::Text,
            "debug",
            telemetry::allow_rmcp_debug(allow.then_some("1")),
        );
        let mut m = world::mcp_client::Mcp::connect(r.clone(), token.clone()).await;
        let got = m.call("get", json!({"key": key})).await.unwrap();
        drop(guard);
        assert!(
            got.to_string().contains(SECRET),
            "the agent reads it: {got}"
        );
        buf.text()
    };
    let capped = run(false).await;
    assert!(capped.contains("mcp_tool"), "{capped}");
    assert!(
        !capped.contains(SECRET),
        "rmcp logged the tool result:\n{capped}"
    );
    assert!(!capped.contains(&token), "token in the log");
    // The check above can fail: allowed, rmcp does log the result.
    let allowed = run(true).await;
    assert!(allowed.contains(SECRET), "rmcp no longer logs results?");
}

/// `/metrics` has the HTTP, pool and worker series.
#[tokio::test]
async fn metrics_show_http_pool_and_workers() {
    let pool = test_pool().await;
    let a = app(&pool);
    let r = api::server_router(a.clone(), cfg(), None, Some("t2token".into()));
    send(
        &r,
        get("/api/v1/auth/login-context")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    a.index.sync_all(&pool).await.unwrap();
    let released = oxledger::db::RELEASED_IN_TRANSACTION.load(std::sync::atomic::Ordering::Relaxed);
    let m = send(
        &r,
        get("/metrics")
            .header(header::AUTHORIZATION, "Bearer t2token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(m.status, StatusCode::OK);
    for want in [
        "http_requests_total{route=\"/api/v1/auth/login-context\",method=\"GET\",status_class=\"2xx\"} 1",
        "http_request_errors_total{route=\"/api/v1/auth/login-context\",method=\"GET\"} 0",
        "http_request_duration_seconds_count{route=\"/api/v1/auth/login-context\",method=\"GET\"} 1",
        "db_pool_connections{state=\"idle\"}",
        "db_pool_connections{state=\"in_use\"}",
        "db_pool_max_connections ",
        "db_pool_acquire_timeouts_total 0",
        &format!("db_pool_released_in_transaction_total {released}"),
        "worker_started{worker=\"github\"} 0",
        "worker_rounds_total{worker=\"ai_jobs\",result=\"failed\"} 0",
        "worker_round_duration_seconds_count{worker=\"search\"}",
        "worker_last_success_timestamp_seconds{worker=\"notify\"} 0",
        "worker_backlog{worker=\"search\"}",
        "worker_backlog{worker=\"notify\"}",
        "worker_backlog{worker=\"github\"}",
        "worker_backlog{worker=\"ai_run\"}",
    ] {
        assert!(m.text.contains(want), "missing {want}:\n{}", m.text);
    }
    let (ok, _) = metrics::worker_rounds(Worker::Search);
    assert!(ok >= 1, "sync_all rounds count for the search worker");
    assert!(
        !m.text
            .contains("worker_last_success_timestamp_seconds{worker=\"search\"} 0\n"),
        "{}",
        m.text
    );
    // The G20 counter is the one in db.rs.
    oxledger::db::RELEASED_IN_TRANSACTION.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    let m = send(
        &r,
        get("/metrics")
            .header(header::AUTHORIZATION, "Bearer t2token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        m.text.contains(&format!(
            "db_pool_released_in_transaction_total {}",
            released + 2
        )),
        "{}",
        m.text
    );
    // /metrics itself is a route under the middleware.
    assert!(m.request_id.is_some());
}

/// A request that cannot get a pooled connection counts as an acquire
/// timeout (and is `temporarily_unavailable`, 503).
#[tokio::test]
async fn pool_acquire_timeouts_are_counted() {
    let db = common::test_db();
    let pool = oxledger::db::connect_with(
        oxledger::db::pool_options()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(300)),
        &db.url(),
    )
    .await
    .unwrap();
    let r = api::router(app(&pool), cfg());
    let held = pool.acquire().await.unwrap();
    let before = metrics::POOL_ACQUIRE_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed);
    let resp = send(
        &r,
        get("/api/v1/auth/login-context")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text
    );
    assert_eq!(resp.body["error"]["code"], "temporarily_unavailable");
    assert_eq!(
        metrics::POOL_ACQUIRE_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed),
        before + 1
    );
    assert_eq!(HTTP.count("/api/v1/auth/login-context", "GET", "5xx"), 1);
    drop(held);
}

/// The GitHub worker and the AI jobs mark themselves started and count
/// rounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_and_ai_workers_report_rounds() {
    let pool = test_pool().await;
    let a = app(&pool);
    assert!(!metrics::worker_is_started(Worker::Github));
    oxledger::github::spawn_worker(a.clone(), Arc::new(oxledger::github::client::NoGithub));
    oxledger::ai::runner::spawn_jobs(pool.clone());
    common::poll_until(
        "a GitHub and an AI jobs round",
        common::WAIT_TIMEOUT,
        async || {
            let g = metrics::worker_rounds(Worker::Github);
            let ai = metrics::worker_rounds(Worker::AiJobs);
            // Counted either way (the slot database may hold other tests'
            // GitHub jobs, which the no-GitHub client fails).
            if g.0 + g.1 >= 1 && ai.0 + ai.1 >= 1 {
                common::Break(())
            } else {
                common::Continue(format!("rounds: github {g:?} ai {ai:?}"))
            }
        },
    )
    .await;
    assert!(metrics::worker_is_started(Worker::Github));
    assert!(metrics::worker_is_started(Worker::AiJobs));
}

async fn readyz(r: &Router) -> Resp {
    send(r, get("/readyz").body(Body::empty()).unwrap()).await
}

/// `/readyz`: 200 when ready; 503 without a database (and nothing about
/// the connection in the body), with a migration of this build not
/// applied, or with an expected worker not started. `/healthz` stays 200.
#[tokio::test]
async fn readyz_reports_database_migrations_and_workers() {
    let pool = test_pool().await;
    let r = api::router(app(&pool), cfg());
    let ok = readyz(&r).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    assert_eq!(
        ok.body,
        json!({"status": "ready", "checks": {"database": "ok", "migrations": "ok", "workers": "ok"}})
    );

    // A migration this build has and the database does not.
    let mut expected = api::expected_migrations();
    expected.push(99_999_999_999_999);
    let behind = api::observed(api::readyz_routes(app(&pool), expected.into()));
    let resp = readyz(&behind).await;
    assert_eq!(resp.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.body["checks"]["migrations"], "pending",
        "{}",
        resp.text
    );
    assert_eq!(resp.body["checks"]["database"], "ok");

    // No database: a lazy pool pointing at a closed port.
    let secret_url = "postgres://readyuser:READYSECRETPW@127.0.0.1:1/readydbname";
    let down = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy(secret_url)
        .unwrap();
    let rd = api::router(app(&down), cfg());
    let resp = readyz(&rd).await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text
    );
    assert_eq!(
        resp.body,
        json!({"status": "not_ready", "checks": {"database": "unavailable", "migrations": "unknown", "workers": "ok"}})
    );
    for leak in ["READYSECRETPW", "readyuser", "readydbname", "127.0.0.1"] {
        assert!(!resp.text.contains(leak), "{leak} in {}", resp.text);
    }
    let live = send(&rd, get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(live.status, StatusCode::OK);

    // A worker this process runs that has not started yet.
    metrics::worker_expected(Worker::Github);
    let resp = readyz(&r).await;
    assert_eq!(resp.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.body["checks"]["workers"], "not_started");
    metrics::worker_started(Worker::Github);
    assert_eq!(readyz(&r).await.status, StatusCode::OK);
    metrics::reset_worker(Worker::Github);
}

/// A pool of one connection with a 300 ms acquire timeout, not migrated.
async fn tiny_pool(url: &str) -> sqlx::PgPool {
    oxledger::db::pool_options()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(300))
        .connect_with(oxledger::db::connect_options(url).unwrap())
        .await
        .unwrap()
}

/// [`tiny_pool`] on this test's (migrated) slot database.
async fn tiny_slot_pool() -> sqlx::PgPool {
    test_pool().await.close().await;
    tiny_pool(&common::test_db().url()).await
}

/// [`tiny_pool`] on the server's `postgres` database, which has none of
/// oxledger's tables (never migrated here).
async fn tiny_admin_pool() -> sqlx::PgPool {
    tiny_pool(&common::test_db().admin_url()).await
}

/// A busy main pool is not "not ready": `/readyz` has its own connection.
#[tokio::test]
async fn readyz_does_not_wait_for_the_main_pool() {
    let pool = tiny_slot_pool().await;
    let r = api::router(app(&pool), cfg());
    let held = pool.acquire().await.unwrap();
    let resp = readyz(&r).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text);
    assert_eq!(
        metrics::POOL_ACQUIRE_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    drop(held);
}

/// Many probes at once run one database check; the result is reused for
/// `READY_CACHE`, then checked again.
#[tokio::test]
async fn readyz_checks_are_single_flight_and_cached() {
    use std::sync::atomic::Ordering::Relaxed;
    let pool = test_pool().await;
    let r = api::router(app(&pool), cfg());
    let before = api::READY_DB_CHECKS.load(Relaxed);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let r = r.clone();
        set.spawn(async move { readyz(&r).await.status });
    }
    common::wait_for("64 parallel readyz checks", common::WAIT_TIMEOUT, async {
        while let Some(s) = set.join_next().await {
            assert_eq!(s.unwrap(), StatusCode::OK);
        }
    })
    .await;
    assert_eq!(api::READY_DB_CHECKS.load(Relaxed) - before, 1);
    tokio::time::sleep(api::READY_CACHE + Duration::from_millis(100)).await;
    assert_eq!(readyz(&r).await.status, StatusCode::OK);
    assert_eq!(api::READY_DB_CHECKS.load(Relaxed) - before, 2);
}

/// A database that accepts the connection and never answers: 503 after
/// `READY_TIMEOUT` (2 s), not after the pool's own, longer timeout.
#[tokio::test]
async fn readyz_gives_up_after_two_seconds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let stuck = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&format!("postgres://u:p@127.0.0.1:{port}/db"))
        .unwrap();
    let r = api::router(app(&stuck), cfg());
    let started = std::time::Instant::now();
    let resp = readyz(&r).await;
    let took = started.elapsed();
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text
    );
    assert_eq!(resp.body["checks"]["database"], "unavailable");
    assert!(
        took >= api::READY_TIMEOUT - Duration::from_millis(100) && took < Duration::from_secs(5),
        "took {took:?}"
    );
    silent.abort();
}

/// A database without `_sqlx_migrations` (the server's `postgres`
/// database): the migrations are `unknown` and that is not ready.
#[tokio::test]
async fn readyz_unreadable_migrations_are_not_ready() {
    let pool = tiny_admin_pool().await;
    let r = api::router(app(&pool), cfg());
    let resp = readyz(&r).await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text
    );
    assert_eq!(
        resp.body["checks"],
        json!({"database": "ok", "migrations": "unknown", "workers": "ok"})
    );
}

/// An internal error is a 500 and counts as 5xx; `/metrics` failing is
/// one too.
#[tokio::test]
async fn internal_errors_count_as_5xx() {
    // The `postgres` database has none of oxledger's tables.
    let pool = tiny_admin_pool().await;
    let r = api::server_router(app(&pool), cfg(), None, Some("t2token".into()));
    let resp = send(
        &r,
        get("/api/v1/auth/login-context")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        resp.text
    );
    assert_eq!(HTTP.count("/api/v1/auth/login-context", "GET", "5xx"), 1);
    assert_eq!(HTTP.count("/api/v1/auth/login-context", "GET", "4xx"), 0);
    let m = send(
        &r,
        get("/metrics")
            .header(header::AUTHORIZATION, "Bearer t2token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(m.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", m.text);
    assert_eq!(HTTP.count("/metrics", "GET", "5xx"), 1);
}

/// The GitHub webhook's 500 (it could not store the delivery) counts a pool
/// timeout like any other route.
#[tokio::test]
async fn webhook_pool_timeouts_are_counted() {
    use oxledger::github::webhook;
    let pool = tiny_slot_pool().await;
    let r = api::observed(webhook::routes(app(&pool), b"t2hook".to_vec()));
    let body = br#"{"zen": "t2"}"#;
    let held = pool.acquire().await.unwrap();
    let resp = send(
        &r,
        Request::post(webhook::PATH)
            .header("x-hub-signature-256", webhook::sign(b"t2hook", body))
            .header("x-github-delivery", Uuid::now_v7().to_string())
            .header("x-github-event", "ping")
            .body(Body::from(body.to_vec()))
            .unwrap(),
    )
    .await;
    drop(held);
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        metrics::POOL_ACQUIRE_TIMEOUTS.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(HTTP.count(webhook::PATH, "POST", "5xx"), 1);
}

/// Connections say who they are (research 06 I-4): `oxledger` for the main
/// pool, `oxledger-readyz` for the readiness check.
#[tokio::test]
async fn connections_carry_the_application_name() {
    let pool = test_pool().await;
    let mine: String = sqlx::query_scalar(
        "SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mine, oxledger::db::APPLICATION_NAME);
    let r = api::router(app(&pool), cfg());
    assert_eq!(readyz(&r).await.status, StatusCode::OK);
    let readyz_conns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
          WHERE application_name = 'oxledger-readyz' AND datname = current_database()",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(readyz_conns, 1);
}

/// U+2028, U+2029 and U+0085 are written as `\u` escapes: no line breaks
/// for JavaScript or log tools inside a JSON line.
#[tokio::test]
async fn json_log_escapes_unicode_line_breaks() {
    let (buf, guard) = capture_json();
    tracing::info!(v = "a\u{2028}b", "x\u{2029}y\u{0085}z");
    drop(guard);
    let text = buf.text();
    assert!(
        !text.contains(['\u{2028}', '\u{2029}', '\u{0085}']),
        "{text:?}"
    );
    let (v, msg) = (
        ["a", "u2028b"].join("\\"),
        ["x", "u2029y", "u0085z"].join("\\"),
    );
    assert!(text.contains(&v) && text.contains(&msg), "{text}");
    let lines = buf.json();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["fields"]["v"], "a\u{2028}b");
    assert_eq!(lines[0]["fields"]["message"], "x\u{2029}y\u{0085}z");
}

/// `OXLEDGER_LOG_FORMAT=json`: one JSON object per line with level,
/// target, fields and the span fields.
#[tokio::test]
async fn json_log_lines_carry_span_fields() {
    let (buf, guard) = capture_json();
    {
        let span = tracing::info_span!("outer", a = 1, b = tracing::field::Empty);
        let _e = span.enter();
        span.record("b", "two");
        tracing::warn!(n = 3, "hello \"quoted\"\nnext");
    }
    drop(guard);
    let lines = buf.json();
    let l = lines
        .iter()
        .find(|l| {
            l["fields"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("hello"))
        })
        .unwrap_or_else(|| panic!("{}", buf.text()));
    assert_eq!(l["level"], "WARN");
    assert_eq!(l["target"], "t2_observability");
    assert_eq!(l["fields"]["n"], 3);
    assert_eq!(l["fields"]["message"], "hello \"quoted\"\nnext");
    assert!(l["timestamp"].as_str().is_some());
    assert_eq!(
        l["spans"],
        json!([{"name": "outer", "a": 1, "b": "two"}]),
        "{l}"
    );
    assert_eq!(buf.text().lines().count(), lines.len());
}
