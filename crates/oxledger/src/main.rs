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
//!   oxledger setup init-defaults   default roles, trackers, statuses, workflow (empty site only)
//!   oxledger project add <KEY> <name> [--public] [--modules issue_tracking,time_tracking,wiki]
//!   oxledger project list
//!   oxledger member add <KEY> <login> <role>[,<role>...]   roles by name (`role list`)
//!   oxledger role list
//!   oxledger api-spec [dir]  write <dir>/openapi.json and <dir>/types.ts (default dir: api)
//!   oxledger seed-demo    fill an empty database with demo data, print API keys
//!                         (only with --features test-support)
//!   oxledger reindex      ask the running server for a blue-green rebuild of the search
//!                         indexes (issues and wiki)
//!   oxledger reindex --offline   rebuild both in this process (server stopped)
//!
//! Configuration (environment): DATABASE_URL, OXLEDGER_INDEX_DIR (default ./data/index; the
//! wiki index lives in its `wiki` subdirectory),
//! OXLEDGER_BIND (default 127.0.0.1:8080), OXLEDGER_API_KEY (stdio),
//! OXLEDGER_SPA_ORIGIN (default http://<OXLEDGER_BIND>; non-GET /api/v1 requests must come from it),
//! OXLEDGER_METRICS_TOKEN (enables GET /metrics with `Authorization: Bearer <token>`),
//! OXLEDGER_LOG_FORMAT (`text`, the default, or `json`), OXLEDGER_DB_MAX_CONNECTIONS.
//! Attachments (design 06 R-A2, R-A11): OXLEDGER_FILES_DIR (unset: attachments off; back it
//! up at the same point in time as PostgreSQL), OXLEDGER_ATTACHMENT_MAX_BYTES (default 20 MB,
//! at most 100 MB), OXLEDGER_UPLOAD_DAILY_BYTES (per person, default 1 GB),
//! OXLEDGER_FILES_MIN_FREE_BYTES (default 5 GB), OXLEDGER_FILES_ORIGIN (optional download origin).
//! `serve` also answers GET /readyz (database, migrations, workers started).

use anyhow::{Context, Result, bail};
use oxledger::api::{self, ApiConfig};
use oxledger::auth::{self, SessionPolicy};
use oxledger::db;
use oxledger::mcp::OxServer;
use oxledger::metrics::{Worker, timed_round, worker_expected, worker_round, worker_started};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::wiki_search::WikiIndex;
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
    // The first sync round (background) validates the generation on disk or
    // builds one; until then free-text search answers search_unavailable.
    let index = SearchIndex::open(Some(&dir))?;
    // Design 06 §3.14: the wiki index, its own generations under `wiki/`
    // (the issue index only ever touches `CURRENT` and `gen-*`).
    let wiki = WikiIndex::open(Some(&dir.join("wiki")))?;
    Ok(Arc::new(App { pool, index, wiki }))
}

/// Rebuilds the wiki index blue-green in the background (`oxledger
/// reindex` asks for both indexes).
fn spawn_wiki_rebuild(app: Arc<App>) {
    tokio::spawn(async move {
        match app.wiki.rebuild(&app.pool).await {
            Ok(r) => tracing::info!(?r, "wiki index rebuilt"),
            Err(e) => tracing::error!("wiki index rebuild failed: {e:#}"),
        }
    });
}

/// Keeps the wiki index following the outbox (design 06 §3.14, B-49) and
/// runs its generation GC hourly. Its own task, so a wiki baseline does not
/// hold up issue search; rebuild requests come through [`spawn_indexer`].
fn spawn_wiki_indexer(app: Arc<App>) {
    app.wiki
        .metrics
        .loop_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let mut last_gc = std::time::Instant::now();
        loop {
            if let Err(e) = app.wiki.sync(&app.pool).await {
                tracing::warn!("wiki index sync failed: {e:#}");
            }
            if last_gc.elapsed() >= Duration::from_secs(3600) {
                last_gc = std::time::Instant::now();
                match app.wiki.gc(&app.pool).await {
                    Ok((rows, gens)) => tracing::info!(rows, gens, "wiki index GC"),
                    Err(e) => tracing::warn!("wiki index GC failed: {e:#}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// Keeps the search index following the outbox; picks up `oxledger reindex`
/// requests, runs the outbox GC hourly and the long-transaction check every
/// 10 s (design 03 §4.7–4.9).
/// The upgrade claim of [`spawn_indexer`]: released when dropped unless
/// the rebuild succeeded (`.1`).
struct UpgradeClaim(Arc<App>, bool);

impl Drop for UpgradeClaim {
    fn drop(&mut self) {
        if !self.1 {
            self.0.index.release_upgrade();
        }
    }
}

fn spawn_indexer(app: Arc<App>) {
    app.index
        .metrics
        .loop_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    worker_expected(Worker::Search);
    tokio::spawn(async move {
        worker_started(Worker::Search);
        let mut last_gc = std::time::Instant::now();
        let mut last_check = std::time::Instant::now();
        loop {
            if let Err(e) = app.index.sync(&app.pool).await {
                tracing::warn!("index sync failed: {e:#}");
            }
            // An index written by an older release (M1.7 schema/semantics
            // change): rebuild blue-green while the old generation keeps
            // serving with its own query semantics (design 03 §4.8).
            if app.index.needs_upgrade() && app.index.claim_upgrade() {
                let a = app.clone();
                tokio::spawn(async move {
                    // Released on every way out but success, a panic
                    // included, so a later round can try again.
                    let mut claim = UpgradeClaim(a.clone(), false);
                    tracing::info!("search index layout is outdated: rebuilding");
                    match a.index.rebuild(&a.pool).await {
                        Ok(r) => {
                            claim.1 = true;
                            tracing::info!(?r, "search index upgraded");
                        }
                        Err(e) => {
                            tracing::error!("search index upgrade failed: {e:#}");
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }
                });
            }
            if last_check.elapsed() >= Duration::from_secs(10) {
                last_check = std::time::Instant::now();
                let requested: Result<Option<bool>, _> = sqlx::query_scalar(
                    "UPDATE search_settings SET rebuild_requested_at = NULL
                     WHERE rebuild_requested_at IS NOT NULL RETURNING true",
                )
                .fetch_optional(&app.pool)
                .await;
                if let Ok(Some(_)) = requested {
                    spawn_wiki_rebuild(app.clone());
                    let a = app.clone();
                    tokio::spawn(async move {
                        match a.index.rebuild(&a.pool).await {
                            Ok(r) => tracing::info!(?r, "search index rebuilt"),
                            Err(e) => tracing::error!("search index rebuild failed: {e:#}"),
                        }
                    });
                }
                if let Err(e) = oxledger::metrics::check_long_xacts(
                    &app.pool,
                    oxledger::metrics::XACT_WARN,
                    oxledger::metrics::XACT_CRIT,
                )
                .await
                {
                    tracing::debug!("long transaction check failed: {e:#}");
                }
            }
            if last_gc.elapsed() >= Duration::from_secs(3600) {
                last_gc = std::time::Instant::now();
                match app.index.gc(&app.pool).await {
                    Ok((rows, gens)) => tracing::info!(rows, gens, "search outbox GC"),
                    Err(e) => tracing::warn!("search outbox GC failed: {e:#}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// Background job for custom field indexes (custom_fields::reconcile_indexes):
/// builds pending/broken per-field indexes and finishes interrupted drops.
/// Only `serve` runs it, so stdio processes never race on the same DDL.
fn spawn_field_indexer(app: Arc<App>) {
    worker_expected(Worker::FieldIndex);
    tokio::spawn(async move {
        worker_started(Worker::FieldIndex);
        loop {
            let started = std::time::Instant::now();
            let mut round: Result<()> = Ok(());
            match oxledger::custom_fields::reconcile_indexes(&app.pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(fixed = n, "custom field indexes reconciled"),
                Err(e) => {
                    tracing::warn!("custom field index job failed: {e:#}");
                    round = Err(e);
                }
            }
            // Contract 05 §3.7: conversions a restart interrupted (each
            // batch is idempotent, so a second runner is harmless).
            match oxledger::field_admin::resume_conversions(&app.pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(resumed = n, "custom field conversions resumed"),
                Err(e) => {
                    tracing::warn!("custom field conversion job failed: {e:#}");
                    round = round.and(Err(e));
                }
            }
            worker_round(
                Worker::FieldIndex,
                started.elapsed(),
                round.as_ref().map(|_| ()),
            );
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

/// Notification dispatch (design 05 §3.11.2): follows the outbox as the
/// consumer `notify` (target: on the bell within 10 s of the event, B-26)
/// and removes notifications past their retention once an hour (B-25).
fn spawn_notifier(app: Arc<App>) {
    worker_expected(Worker::Notify);
    tokio::spawn(async move {
        worker_started(Worker::Notify);
        let mut last_purge: Option<std::time::Instant> = None;
        loop {
            let more = match oxledger::notifications::dispatch_round(&app.pool).await {
                Ok(r) => r.more,
                Err(e) => {
                    tracing::warn!("notification dispatch failed: {e:#}");
                    false
                }
            };
            if last_purge.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
                last_purge = Some(std::time::Instant::now());
                match oxledger::notifications::purge(&app.pool).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(rows = n, "old notifications removed"),
                    Err(e) => tracing::warn!("notification purge failed: {e:#}"),
                }
            }
            if !more {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    });
}

/// Deletes expired login challenges.
fn spawn_sweeper(app: Arc<App>) {
    worker_expected(Worker::LoginSweeper);
    tokio::spawn(async move {
        worker_started(Worker::LoginSweeper);
        loop {
            let swept = timed_round(Worker::LoginSweeper, async {
                sqlx::query("DELETE FROM login_challenges WHERE expires_at < now()")
                    .execute(&app.pool)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = swept {
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
    spawn_wiki_indexer(app.clone());
    spawn_field_indexer(app.clone());
    spawn_sweeper(app.clone());
    spawn_notifier(app.clone());
    // The built-in AI (design 05 §3.13.2 Q1): endpoints and keys from the
    // environment only; a name that does not qualify is not registered and
    // logged by name and reason (never the URL or the key).
    {
        let (ai, warnings) = oxledger::ai::config::from_vars(std::env::vars());
        for w in warnings {
            tracing::warn!("{w}");
        }
        tracing::info!(
            endpoints = ?ai.endpoints.keys().collect::<Vec<_>>(),
            allow_http = ai.allow_http,
            transport = oxledger::ai::client::TRANSPORT,
            "AI endpoints registered"
        );
        oxledger::ai::config::install(ai);
        oxledger::ai::runner::spawn_jobs(app.pool.clone());
    }
    // Attachments (design 06 R-A11): off without OXLEDGER_FILES_DIR.
    {
        let (files, warnings) = oxledger::attachments::from_vars(|k| std::env::var(k).ok());
        for w in warnings {
            tracing::warn!("{w}");
        }
        match &files.dir {
            Some(d) => {
                tracing::info!(dir = %d.display(), max_bytes = files.max_bytes, "attachments on")
            }
            None => tracing::info!("attachments off (OXLEDGER_FILES_DIR is not set)"),
        }
        oxledger::attachments::install(files);
    }
    let bind = env("OXLEDGER_BIND", Some("127.0.0.1:8080"))?;
    let cfg = ApiConfig {
        spa_origin: env("OXLEDGER_SPA_ORIGIN", Some(&format!("http://{bind}")))?,
        session: SessionPolicy::default(),
    };
    let github = oxledger::github::GithubSettings::from_vars(|k| std::env::var(k).ok())?;
    let hook = match &github {
        Some(s) => {
            let client = oxledger::github::client(s)?;
            oxledger::github::spawn_worker(app.clone(), client);
            tracing::info!(
                "GitHub integration on (webhook at {})",
                oxledger::github::webhook::PATH
            );
            Some(s.config())
        }
        None => None,
    };
    let metrics_token = std::env::var("OXLEDGER_METRICS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if metrics_token.is_some() {
        tracing::info!("metrics at /metrics (bearer token)");
    }
    let router = api::server_router(app, cfg, hook, metrics_token);
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
    spawn_wiki_indexer(app.clone());
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
    let p = oxledger::setup::password_from_input(&s).to_string();
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
        ["setup", "init-defaults"] => {
            oxledger::setup::init_defaults(&pool).await?;
            eprintln!("default roles, trackers, statuses and workflow loaded");
        }
        ["project", "add", key, name, rest @ ..] => {
            let (public, modules) = oxledger::setup::parse_project_flags(rest)?;
            let modules: Vec<&str> = modules.iter().map(String::as_str).collect();
            let id = oxledger::setup::add_project(&pool, key, name, public, &modules).await?;
            println!("{id}");
        }
        ["project", "list"] => {
            for line in oxledger::setup::list_projects(&pool).await? {
                println!("{line}");
            }
        }
        // PowerShell turns an unquoted `a,b` into two arguments: take every
        // argument after the login and split each at commas.
        ["member", "add", key, login, roles @ ..] => {
            let roles = oxledger::setup::split_list(roles);
            let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
            oxledger::setup::add_member(&pool, key, login, &roles).await?;
            eprintln!("{login} is a member of {key}: {}", roles.join(", "));
        }
        ["role", "list"] => {
            for line in oxledger::setup::list_roles(&pool).await? {
                println!("{line}");
            }
        }
        _ => bail!("unknown command; see `oxledger` with no arguments"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so stdio MCP stays clean; OXLEDGER_LOG_FORMAT=json
    // switches to one JSON object per line (T2). rmcp stays at info unless
    // OXLEDGER_ALLOW_RMCP_DEBUG=1: its debug logs carry tool results.
    {
        use oxledger::telemetry;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let env = |k: &str| std::env::var(k).ok();
        let format = telemetry::parse_log_format(env(telemetry::LOG_FORMAT_ENV).as_deref())?;
        telemetry::subscriber(
            format,
            telemetry::env_filter(env("RUST_LOG").as_deref()),
            telemetry::allow_rmcp_debug(env(telemetry::ALLOW_RMCP_DEBUG_ENV).as_deref()),
            std::io::stderr,
        )
        .init();
    }
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
        Some("user" | "agent-client" | "setup" | "project" | "member" | "role") => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            admin(&args).await
        }
        Some("reindex") => {
            if std::env::args().nth(2).as_deref() == Some("--offline") {
                let app = open_app().await?;
                let r = app.index.rebuild(&app.pool).await?;
                eprintln!(
                    "rebuilt: generation {} ({} documents, F {})",
                    r.generation, r.baseline_docs, r.frontier
                );
                let r = app.wiki.rebuild(&app.pool).await?;
                eprintln!(
                    "rebuilt the wiki index: generation {} ({} pages, F {})",
                    r.generation, r.baseline_docs, r.frontier
                );
            } else {
                let pool = db::connect(&env("DATABASE_URL", None)?).await?;
                sqlx::query("UPDATE search_settings SET rebuild_requested_at = now()")
                    .execute(&pool)
                    .await?;
                eprintln!(
                    "rebuild requested; the running server builds new generations of the issue and\n\
                     wiki indexes in the background\n\
                     (search keeps working). Without a running server use `oxledger reindex --offline`."
                );
            }
            Ok(())
        }
        #[cfg(not(feature = "test-support"))]
        Some("seed-demo") => {
            bail!(
                "seed-demo is only in builds with the test-support feature: cargo run --features test-support -- seed-demo"
            )
        }
        _ => {
            eprintln!(
                "usage: oxledger <serve|stdio|seed-demo|reindex|user|agent-client|setup|project|member|role|api-spec>"
            );
            std::process::exit(2);
        }
    }
}
