// SPDX-License-Identifier: GPL-2.0-or-later
//! GitHub integration (contract 02 §19, U45): a read-only projection of
//! pull requests and CI onto issues. Nothing here writes issues (R-G4).
//!
//! - `webhook`: the signed receiver; `jobs`: the worker and dispatch;
//! - `model`: snapshots and the R-G8 write rules; `refs`: R-G1;
//!   `rollup`: R-G6, R-G7; `client`: what the worker asks GitHub;
//! - `commits`: commits from `push` events (design 06 §3.7).

pub mod client;
pub mod commits;
pub mod fixes;
#[cfg(feature = "github")]
pub mod http;
pub mod jobs;
pub mod links;
pub mod model;
pub mod read;
pub mod refs;
pub mod rollup;
pub mod sync;
pub mod webhook;

/// Settings read from the environment (never stored in the database).
#[derive(Clone)]
pub struct GithubConfig {
    pub webhook_secret: Vec<u8>,
}

impl std::fmt::Debug for GithubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GithubConfig { webhook_secret: <redacted> }")
    }
}

#[cfg(feature = "test-support")]
pub(crate) async fn fault(point: &str) -> anyhow::Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
pub(crate) async fn fault(_: &str) -> anyhow::Result<()> {
    Ok(())
}

/// What `serve` reads from the environment. All of app id, key file and
/// webhook secret, or none (GitHub off: no webhook route, no worker).
pub struct GithubSettings {
    pub app_id: i64,
    pub private_key_pem: String,
    pub webhook_secret: Vec<u8>,
    pub api_url: String,
}

impl std::fmt::Debug for GithubSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GithubSettings {{ app_id: {}, api_url: {}, private_key_pem: <redacted>, webhook_secret: <redacted> }}",
            self.app_id, self.api_url
        )
    }
}

pub const DEFAULT_API_URL: &str = "https://api.github.com";

impl GithubSettings {
    /// `get` reads one variable (the environment in `serve`).
    pub fn from_vars(
        get: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<GithubSettings>> {
        let id = get("OXLEDGER_GITHUB_APP_ID").filter(|s| !s.is_empty());
        let key = get("OXLEDGER_GITHUB_PRIVATE_KEY_FILE").filter(|s| !s.is_empty());
        let secret = get("OXLEDGER_GITHUB_WEBHOOK_SECRET").filter(|s| !s.is_empty());
        let (id, key, secret) = match (id, key, secret) {
            (None, None, None) => return Ok(None),
            (Some(i), Some(k), Some(s)) => (i, k, s),
            _ => anyhow::bail!(
                "GitHub needs all of OXLEDGER_GITHUB_APP_ID, OXLEDGER_GITHUB_PRIVATE_KEY_FILE and OXLEDGER_GITHUB_WEBHOOK_SECRET"
            ),
        };
        let app_id = id
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("OXLEDGER_GITHUB_APP_ID is not a number"))?;
        let private_key_pem = std::fs::read_to_string(&key)
            .map_err(|e| anyhow::anyhow!("cannot read OXLEDGER_GITHUB_PRIVATE_KEY_FILE: {e}"))?;
        Ok(Some(GithubSettings {
            app_id,
            private_key_pem,
            webhook_secret: secret.into_bytes(),
            api_url: get("OXLEDGER_GITHUB_API_URL").unwrap_or_else(|| DEFAULT_API_URL.into()),
        }))
    }

    pub fn config(&self) -> GithubConfig {
        GithubConfig {
            webhook_secret: self.webhook_secret.clone(),
        }
    }
}

/// The REST client for these settings (`http://` only with test-support).
#[cfg(feature = "github")]
pub fn client(s: &GithubSettings) -> anyhow::Result<std::sync::Arc<dyn client::GithubApi>> {
    Ok(std::sync::Arc::new(http::HttpGithub::new(
        &s.api_url,
        s.app_id,
        &s.private_key_pem,
        cfg!(feature = "test-support"),
    )?))
}

#[cfg(not(feature = "github"))]
pub fn client(_: &GithubSettings) -> anyhow::Result<std::sync::Arc<dyn client::GithubApi>> {
    anyhow::bail!("GitHub is configured but this build has no `github` feature")
}

/// `pg_try_advisory_lock` key of the scheduled GitHub passes.
pub const SCHEDULE_LOCK: i64 = 0x6f78_6c67_6768_0001;

/// The background loop of `serve`: jobs every second, backfill and
/// resume as they come, open PRs every 15 minutes, redelivery every 10,
/// the full reconcile and retention every hour.
pub fn spawn_worker(
    app: std::sync::Arc<crate::service::App>,
    api: std::sync::Arc<dyn client::GithubApi>,
) {
    use crate::metrics::{Worker, worker_expected, worker_round, worker_started};
    use std::time::{Duration, Instant};
    worker_expected(Worker::Github);
    tokio::spawn(async move {
        worker_started(Worker::Github);
        let start = Instant::now();
        let every = |last: &mut Option<Instant>, d: Duration| {
            let due = last.is_none_or(|l| l.elapsed() >= d);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        let (mut open, mut redeliver, mut full) = (None, None, None);
        let mut asked = std::collections::HashSet::new();
        loop {
            let round_started = Instant::now();
            let r: anyhow::Result<()> = async {
                // Jobs are shared through SKIP LOCKED; the scheduled passes
                // run in one process at a time (B22).
                jobs::drain(&app, api.as_ref()).await?;
                let mut lock = app.pool.acquire().await?;
                let mine: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(SCHEDULE_LOCK)
                    .fetch_one(&mut *lock)
                    .await?;
                if !mine {
                    return Ok(());
                }
                let r: anyhow::Result<()> = async {
                    let now = chrono::Utc::now();
                    sync::resume_paused_once(&app, now).await?;
                    sync::backfill_once(&app, api.as_ref(), now).await?;
                    // Design 06 R-F3: confirm merged PRs with fix keywords
                    // and move their issues.
                    fixes::run_pending_once(&app, api.as_ref(), now).await?;
                    if every(&mut open, Duration::from_secs(900)) {
                        sync::reconcile_open_once(&app, api.as_ref()).await?;
                    }
                    if start.elapsed() >= Duration::from_secs(60)
                        && every(&mut redeliver, Duration::from_secs(600))
                    {
                        sync::redeliver_once(&app, api.as_ref(), now, &mut asked).await?;
                    }
                    if every(&mut full, Duration::from_secs(3600)) {
                        sync::reconcile_full_once(&app, api.as_ref(), now).await?;
                        sync::sweep_once(&app, now).await?;
                    }
                    Ok(())
                }
                .await;
                // Always released, or the pooled connection would keep it.
                let unlocked = sqlx::query("SELECT pg_advisory_unlock($1)")
                    .bind(SCHEDULE_LOCK)
                    .execute(&mut *lock)
                    .await;
                if unlocked.is_err() {
                    lock.detach();
                }
                r
            }
            .await;
            worker_round(
                Worker::Github,
                round_started.elapsed(),
                r.as_ref().map(|_| ()),
            );
            if let Err(e) = r {
                tracing::warn!("github worker: {}", e.root_cause());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
