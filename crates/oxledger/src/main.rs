// SPDX-License-Identifier: GPL-2.0-or-later
//! oxledger command line.
//!
//!   oxledger serve        HTTP server: /api/v1 (SPA, session cookie), /mcp (Streamable HTTP,
//!                         bearer agent token or legacy API key), /healthz
//!   oxledger stdio        MCP over stdio with the token in OXLEDGER_API_KEY (agent token or
//!                         legacy key); every tool call re-checks it
//!   oxledger user add <login> <name> [--admin]   password from stdin
//!   oxledger user set-password <login>           password from stdin; ends all sessions
//!   oxledger user lock|unlock <login>
//!   oxledger user revoke-sessions <login>        end all of the user's browser sessions
//!   oxledger agent-client add <name> <egress-policy-ref>   created disabled
//!   oxledger agent-client enable|disable <name>
//!   oxledger api-spec [dir]  write <dir>/openapi.json and <dir>/types.ts (default dir: api)
//!   oxledger seed-demo    fill an empty database with demo data, print API keys
//!                         (only with --features test-support)
//!   oxledger reindex      rebuild the search index from PostgreSQL
//!
//! Configuration (environment): DATABASE_URL, OXLEDGER_INDEX_DIR (default ./data/index),
//! OXLEDGER_BIND (default 127.0.0.1:8080), OXLEDGER_API_KEY (stdio),
//! OXLEDGER_SPA_ORIGIN (default http://<OXLEDGER_BIND>; non-GET /api/v1 requests must come from it).

use anyhow::{Context, Result, bail};
use oxledger::api::{self, ApiConfig};
use oxledger::auth::{self, SessionPolicy};
use oxledger::db;
use oxledger::mcp::OxServer;
use oxledger::search::SearchIndex;
use oxledger::service::App;
use rmcp::ServiceExt;
use std::io::Read as _;
use std::net::SocketAddr;
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

/// Deletes expired login challenges.
fn spawn_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = sqlx::query("DELETE FROM login_challenges WHERE expires_at < now()")
                .execute(&app.pool)
                .await
            {
                tracing::warn!("login challenge sweep failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

async fn serve() -> Result<()> {
    let app = open_app().await?;
    oxledger::cursor::init(&app.pool).await?;
    spawn_indexer(app.clone());
    spawn_field_indexer(app.clone());
    spawn_sweeper(app.clone());
    let bind = env("OXLEDGER_BIND", Some("127.0.0.1:8080"))?;
    let cfg = ApiConfig {
        spa_origin: env("OXLEDGER_SPA_ORIGIN", Some(&format!("http://{bind}")))?,
        session: SessionPolicy::default(),
    };
    let router = api::router(app, cfg);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on http://{bind} (SPA API at /api/v1, MCP at /mcp)");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn stdio() -> Result<()> {
    let app = open_app().await?;
    oxledger::cursor::init(&app.pool).await?;
    spawn_indexer(app.clone());
    let key = env("OXLEDGER_API_KEY", None)?;
    let Some(ctx) = auth::authenticate_bearer(&app.pool, key.trim()).await? else {
        bail!("OXLEDGER_API_KEY is not a valid agent token or API key")
    };
    // Fixed for the process; `db::effective` re-checks it on every tool call.
    let origin = env("OXLEDGER_SPA_ORIGIN", Some("http://127.0.0.1:8080"))?;
    let server = OxServer::new(app, Some(ctx), origin);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

fn read_password() -> Result<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let p = s.trim_end_matches(['\r', '\n']).to_string();
    auth::check_password_policy(&p)?;
    Ok(p)
}

async fn admin(args: &[String]) -> Result<()> {
    let pool = db::connect(&env("DATABASE_URL", None)?).await?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["user", "add", login, name, rest @ ..] => {
            let is_admin = rest == ["--admin"];
            if !rest.is_empty() && !is_admin {
                bail!("usage: oxledger user add <login> <name> [--admin]");
            }
            let id = auth::create_user(&pool, login, name, &read_password()?, is_admin).await?;
            println!("{id}");
        }
        ["user", "set-password", login] => {
            let id = auth::user_id_by_login(&pool, login).await?;
            auth::set_password(&pool, id, &read_password()?).await?;
            eprintln!("password set; all sessions of {login} ended");
        }
        ["user", verb @ ("lock" | "unlock"), login] => {
            let id = auth::user_id_by_login(&pool, login).await?;
            auth::set_user_status(&pool, id, *verb == "unlock").await?;
        }
        ["user", "revoke-sessions", login] => {
            let id = auth::user_id_by_login(&pool, login).await?;
            let n = auth::admin_revoke_sessions(&pool, id).await?;
            eprintln!("{n} session(s) of {login} ended");
        }
        ["agent-client", "add", name, egress] => {
            println!(
                "{}",
                oxledger::agents::create_agent_client(&pool, name, egress).await?
            );
        }
        ["agent-client", verb @ ("enable" | "disable"), name] => {
            oxledger::agents::set_agent_client_enabled(&pool, name, *verb == "enable").await?;
        }
        _ => bail!("unknown command; see `oxledger` with no arguments"),
    }
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
        #[cfg(feature = "test-support")]
        Some("seed-demo") => {
            let pool = db::connect(&env("DATABASE_URL", None)?).await?;
            for (login, key) in oxledger::seed::seed_demo(&pool).await? {
                println!("{login}\t{key}");
            }
            eprintln!("Demo data created. Keys above are shown once; only hashes are stored.");
            Ok(())
        }
        Some("api-spec") => {
            // Writes the OpenAPI document and the TypeScript types (no DB needed).
            let dir = PathBuf::from(std::env::args().nth(2).unwrap_or_else(|| "api".into()));
            std::fs::create_dir_all(&dir)?;
            let doc = oxledger::openapi::document();
            std::fs::write(
                dir.join("openapi.json"),
                serde_json::to_string_pretty(&doc)?
                    + "
",
            )?;
            std::fs::write(dir.join("types.ts"), oxledger::openapi::typescript(&doc))?;
            eprintln!("wrote {}/openapi.json and types.ts", dir.display());
            Ok(())
        }
        Some("user" | "agent-client") => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            admin(&args).await
        }
        Some("reindex") => {
            let app = open_app().await?;
            oxledger::cursor::init(&app.pool).await?;
            let n = app.index.rebuild(&app.pool).await?;
            eprintln!("reindexed ({n} outbox messages applied)");
            Ok(())
        }
        #[cfg(not(feature = "test-support"))]
        Some("seed-demo") => {
            bail!(
                "seed-demo is only in builds with the test-support feature: cargo run --features test-support -- seed-demo"
            )
        }
        _ => {
            eprintln!("usage: oxledger <serve|stdio|seed-demo|reindex|user|agent-client|api-spec>");
            std::process::exit(2);
        }
    }
}
