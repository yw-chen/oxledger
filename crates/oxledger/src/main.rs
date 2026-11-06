// SPDX-License-Identifier: GPL-2.0-or-later
//! oxledger command line.
//!
//!   oxledger serve        HTTP server: /mcp (Streamable HTTP, bearer API key), /healthz
//!   oxledger stdio        MCP over stdio as the user owning OXLEDGER_API_KEY
//!   oxledger seed-demo    fill an empty database with demo data, print API keys
//!   oxledger reindex      rebuild the search index from PostgreSQL
//!
//! Configuration (environment): DATABASE_URL, OXLEDGER_INDEX_DIR (default ./data/index),
//! OXLEDGER_BIND (default 127.0.0.1:8080), OXLEDGER_API_KEY (stdio).

use anyhow::{Context, Result, bail};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use oxledger::db::{self, Caller};
use oxledger::mcp::OxServer;
use oxledger::search::SearchIndex;
use oxledger::service::App;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn env(name: &str, default: Option<&str>) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => default
            .map(str::to_string)
            .with_context(|| format!("{name} is not set")),
    }
}

async fn open_app() -> Result<Arc<App>> {
    let pool = db::connect(&env("DATABASE_URL", None)?).await?;
    let dir = PathBuf::from(env("OXLEDGER_INDEX_DIR", Some("data/index"))?);
    let index = SearchIndex::open(Some(&dir))?;
    let n = index.sync(&pool).await?;
    tracing::info!(applied = n, "search index caught up");
    Ok(Arc::new(App { pool, index }))
}

/// Keeps the search index following the outbox.
fn spawn_indexer(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = app.index.sync(&app.pool).await {
                tracing::warn!("index sync failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// Background job for custom field indexes (custom_fields::reconcile_indexes):
/// builds pending/broken per-field indexes and finishes interrupted drops.
/// Only `serve` runs it, so stdio processes never race on the same DDL.
fn spawn_field_indexer(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            match oxledger::custom_fields::reconcile_indexes(&app.pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(fixed = n, "custom field indexes reconciled"),
                Err(e) => tracing::warn!("custom field index job failed: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

async fn auth(
    State(app): State<Arc<App>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let key = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string();
    let user = db::caller_for_key(&app.pool, &key).await.map_err(|e| {
        tracing::error!("auth lookup failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let user = user.ok_or(StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(Caller(Some(user)));
    Ok(next.run(req).await)
}

async fn serve() -> Result<()> {
    let app = open_app().await?;
    spawn_indexer(app.clone());
    spawn_field_indexer(app.clone());
    let bind = env("OXLEDGER_BIND", Some("127.0.0.1:8080"))?;
    let mcp_app = app.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(OxServer::new(mcp_app.clone(), None)),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .route("/healthz", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on http://{bind} (MCP at /mcp)");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn stdio() -> Result<()> {
    let app = open_app().await?;
    spawn_indexer(app.clone());
    let key = env("OXLEDGER_API_KEY", None)?;
    let Some(user) = db::caller_for_key(&app.pool, &key).await? else {
        bail!("OXLEDGER_API_KEY is not a valid key")
    };
    let server = OxServer::new(app, Some(Caller(Some(user))));
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so stdio MCP stays clean.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve().await,
        Some("stdio") => stdio().await,
        Some("seed-demo") => {
            let pool = db::connect(&env("DATABASE_URL", None)?).await?;
            for (login, key) in oxledger::seed::seed_demo(&pool).await? {
                println!("{login}\t{key}");
            }
            eprintln!("Demo data created. Keys above are shown once; only hashes are stored.");
            Ok(())
        }
        Some("reindex") => {
            let app = open_app().await?;
            let n = app.index.rebuild(&app.pool).await?;
            eprintln!("reindexed ({n} outbox messages applied)");
            Ok(())
        }
        _ => {
            eprintln!("usage: oxledger <serve|stdio|seed-demo|reindex>");
            std::process::exit(2);
        }
    }
}
