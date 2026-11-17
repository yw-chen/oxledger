// SPDX-License-Identifier: GPL-2.0-or-later
//! Pulling from the API (§19.5): backfill of a new link, the 15-minute
//! open-PR pass, redelivery of failed webhooks (10 minutes) and the hourly
//! full reconcile with its `synced_through` watermark. Each is one call
//! that takes `now`, so tests drive them without waiting.
//!
//! Every write goes through `model` (R-G8) under the links' `FOR SHARE`
//! (`jobs::target_of`), the same path as webhooks.

use super::client::{GithubApi, RateLimited};
use super::jobs::{self, Refetch, Target};
use super::model::{self, PrSnap, RunSnap, StatusSnap, SuiteSnap, Write};
use crate::service::App;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value as Json;
use sqlx::PgConnection;
use std::collections::HashSet;
use uuid::Uuid;

/// Closed PRs the backfill takes.
pub const BACKFILL_CLOSED_DAYS: i64 = 30;
/// The full reconcile re-reads PRs updated after `synced_through − this`.
pub const WATERMARK_SLACK_MINUTES: i64 = 10;
/// Redelivery looks this far back.
pub const REDELIVER_DAYS: i64 = 3;
/// (d): CI of PRs still `pending` this recently, and of PRs merged or
/// closed this recently, is re-read on every full reconcile.
pub const CI_PENDING_DAYS: i64 = 30;
pub const CI_CLOSED_DAYS: i64 = 7;

/// The rate limit, when `e` is one.
pub fn rate_limited(e: &anyhow::Error) -> Option<&RateLimited> {
    e.chain().find_map(|c| c.downcast_ref::<RateLimited>())
}

#[derive(Debug, Clone)]
pub struct LinkRow {
    pub id: Uuid,
    pub repo: Uuid,
    pub inst: i64,
    pub full_name: String,
    pub synced_through: Option<DateTime<Utc>>,
}

async fn links(app: &App, state: &str) -> Result<Vec<LinkRow>> {
    type Row = (Uuid, Uuid, i64, String, Option<DateTime<Utc>>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT l.id, r.id, i.gh_installation_id, r.full_name, l.synced_through
         FROM project_repo_links l
         JOIN github_repos r ON r.id = l.repo_id
         JOIN github_installations i ON i.id = r.installation_id
         WHERE l.unlinked_at IS NULL AND l.sync_state = $1
           AND r.state = 'accessible' AND i.state = 'active'
         ORDER BY l.id",
    )
    .bind(state)
    .fetch_all(&app.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, repo, inst, full_name, synced_through)| LinkRow {
            id,
            repo,
            inst,
            full_name,
            synced_through,
        })
        .collect())
}

async fn pause(app: &App, link: Uuid, until: DateTime<Utc>) -> Result<()> {
    tracing::info!(%link, %until, "github sync paused by the rate limit");
    sqlx::query(
        "UPDATE project_repo_links SET sync_state = 'paused', paused_until = $2
         WHERE id = $1 AND unlinked_at IS NULL",
    )
    .bind(link)
    .bind(until)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// Paused links whose reset time has passed go back to where they were:
/// backfill when it never finished, otherwise `ok`.
pub async fn resume_paused_once(app: &App, now: DateTime<Utc>) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE project_repo_links
         SET sync_state = CASE WHEN synced_through IS NULL THEN 'backfilling' ELSE 'ok' END,
             paused_until = NULL
         WHERE sync_state = 'paused' AND unlinked_at IS NULL
           AND (paused_until IS NULL OR paused_until <= $1)",
    )
    .bind(now)
    .execute(&app.pool)
    .await?
    .rows_affected())
}

// ---------------------------------------------------------------- one PR, one head

/// Refs from the stored row (its title and branch are the newest known):
/// a link made after the PR was stored still gets its refs.
async fn refs_from_row(
    conn: &mut PgConnection,
    api: &dyn GithubApi,
    t: &Target,
    pr: Uuid,
) -> Result<()> {
    let (number, title, head_ref): (i32, String, String) =
        sqlx::query_as("SELECT number, title, head_ref FROM github_pull_requests WHERE id = $1")
            .bind(pr)
            .fetch_one(&mut *conn)
            .await?;
    let commits = api.list_pull_commits(t.inst, &t.full_name, number).await?;
    model::recompute_refs(conn, pr, &t.links, &title, &head_ref, &commits).await
}

/// A PR as the API returned it; then its head's checks.
pub async fn sync_pr(app: &App, api: &dyn GithubApi, l: &LinkRow, v: &Json) -> Result<()> {
    let snap = PrSnap::from_json(v)?;
    let mut refetch = Vec::new();
    {
        let mut tx = app.pool.begin().await?;
        let Some(t) = jobs::target_of(&mut tx, l.repo, l.inst, l.full_name.clone()).await? else {
            return Ok(());
        };
        match model::upsert_pr(&mut tx, t.repo, &snap).await? {
            Write::Applied(id) => jobs::pr_details(&mut tx, api, &t, id, &snap).await?,
            Write::Same(id) | Write::Ignored(id) => refs_from_row(&mut tx, api, &t, id).await?,
            Write::Refetch(_) => refetch.push(Refetch::Pr {
                repo: t.repo,
                inst: t.inst,
                full_name: t.full_name.clone(),
                number: snap.number,
            }),
        }
        tx.commit().await?;
    }
    for r in refetch {
        jobs::refetch(app, api, &r).await?;
    }
    sync_checks(app, api, l, &snap.head_sha).await
}

/// Suites, runs and statuses of one commit (R-G8 for each).
pub async fn sync_checks(app: &App, api: &dyn GithubApi, l: &LinkRow, sha: &str) -> Result<()> {
    let (suites, runs, statuses) = api.checks_for_ref(l.inst, &l.full_name, sha).await?;
    let mut refetch = Vec::new();
    {
        let mut tx = app.pool.begin().await?;
        let Some(t) = jobs::target_of(&mut tx, l.repo, l.inst, l.full_name.clone()).await? else {
            return Ok(());
        };
        let mk = |id| (t.repo, t.inst, t.full_name.clone(), id);
        for s in &suites {
            if s["updated_at"].as_str().is_none() {
                // B29: cannot be ordered → refetch by id.
                let (repo, inst, full_name, id) =
                    mk(s["id"].as_i64().context("suite id missing")?);
                refetch.push(Refetch::Suite {
                    repo,
                    inst,
                    full_name,
                    id,
                });
                continue;
            }
            let s = SuiteSnap::from_json(s)?;
            if let Write::Refetch(_) = model::upsert_suite(&mut tx, t.repo, &s).await? {
                let (repo, inst, full_name, id) = mk(s.id);
                refetch.push(Refetch::Suite {
                    repo,
                    inst,
                    full_name,
                    id,
                });
            }
        }
        for r in &runs {
            let r = RunSnap::from_json(r)?;
            if let Write::Refetch(_) = model::upsert_run(&mut tx, t.repo, &r).await? {
                let (repo, inst, full_name, id) = mk(r.id);
                refetch.push(Refetch::Run {
                    repo,
                    inst,
                    full_name,
                    id,
                });
            }
        }
        for s in &statuses {
            model::upsert_status(&mut tx, t.repo, &StatusSnap::from_json(s)?).await?;
        }
        model::recompute_ci_for_sha(&mut tx, t.repo, sha).await?;
        tx.commit().await?;
    }
    for r in refetch {
        jobs::refetch(app, api, &r).await?;
    }
    Ok(())
}

fn updated_at(v: &Json) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v["updated_at"].as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// What to do with a link pass's error: pause on the rate limit, log the rest.
async fn failed(app: &App, l: &LinkRow, what: &str, e: anyhow::Error) -> Result<()> {
    match rate_limited(&e) {
        Some(r) => pause(app, l.id, r.reset_at).await,
        None => {
            tracing::warn!(link = %l.id, "github {what} failed: {}", e.root_cause());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- backfill

/// Open and draft PRs, and those closed in the last 30 days, with their
/// checks; then `ok` with the watermark at the start. How many links
/// finished.
pub async fn backfill_once(app: &App, api: &dyn GithubApi, now: DateTime<Utc>) -> Result<usize> {
    let mut done = 0;
    for l in links(app, "backfilling").await? {
        match backfill_link(app, api, &l, now).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE project_repo_links SET sync_state = 'ok', synced_through = $2
                     WHERE id = $1 AND unlinked_at IS NULL AND sync_state = 'backfilling'",
                )
                .bind(l.id)
                .bind(now)
                .execute(&app.pool)
                .await?;
                done += 1;
            }
            Err(e) => failed(app, &l, "backfill", e).await?,
        }
    }
    Ok(done)
}

async fn backfill_link(
    app: &App,
    api: &dyn GithubApi,
    l: &LinkRow,
    now: DateTime<Utc>,
) -> Result<()> {
    for v in api.list_pulls(l.inst, &l.full_name, "open", None).await? {
        sync_pr(app, api, l, &v).await?;
    }
    let since = now - Duration::days(BACKFILL_CLOSED_DAYS);
    for v in api
        .list_pulls(l.inst, &l.full_name, "closed", Some(since))
        .await?
    {
        if updated_at(&v).is_some_and(|u| u >= since) {
            sync_pr(app, api, l, &v).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 15-minute pass

/// Open PRs of every `ok` link (catches missed webhooks quickly).
pub async fn reconcile_open_once(app: &App, api: &dyn GithubApi) -> Result<usize> {
    let mut n = 0;
    for l in links(app, "ok").await? {
        let r: Result<()> = async {
            for v in api.list_pulls(l.inst, &l.full_name, "open", None).await? {
                sync_pr(app, api, &l, &v).await?;
            }
            Ok(())
        }
        .await;
        match r {
            Ok(()) => n += 1,
            Err(e) => failed(app, &l, "open-PR reconcile", e).await?,
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------- redelivery

/// Deliveries GitHub saw fail whose guid never reached `github_deliveries`
/// are asked again, one request per guid for as long as `asked` lives (the
/// worker keeps it; the hourly reconcile covers what redelivery cannot).
/// A guid that is not a UUID is refused by the webhook (B19) and never
/// asked. How many were asked.
pub async fn redeliver_once(
    app: &App,
    api: &dyn GithubApi,
    now: DateTime<Utc>,
    asked: &mut HashSet<String>,
) -> Result<usize> {
    let list = api
        .list_hook_deliveries(now - Duration::days(REDELIVER_DAYS))
        .await?;
    let ok: HashSet<&str> = list
        .iter()
        .filter(|d| (200..300).contains(&d.status_code))
        .map(|d| d.guid.as_str())
        .collect();
    let mut n = 0;
    // Newest attempt first: GitHub lists them that way.
    for d in &list {
        if ok.contains(d.guid.as_str()) || asked.contains(&d.guid) {
            continue;
        }
        let Ok(id) = Uuid::parse_str(&d.guid) else {
            continue;
        };
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM github_deliveries WHERE delivery_id = $1)",
        )
        .bind(id)
        .fetch_one(&app.pool)
        .await?;
        if seen {
            continue;
        }
        asked.insert(d.guid.clone());
        api.redeliver(d.id).await?;
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------- hourly

/// Installations and their repositories from the API (authoritative).
async fn reconcile_installations(app: &App, api: &dyn GithubApi) -> Result<()> {
    let list = api.list_installations().await?;
    let mut seen = Vec::new();
    for (gh, login, suspended) in &list {
        let state = if *suspended { "suspended" } else { "active" };
        let mut tx = app.pool.begin().await?;
        let id = jobs::upsert_installation(
            &mut tx,
            &serde_json::json!({"id": gh, "account": {"login": login}}),
            state,
        )
        .await?;
        tx.commit().await?;
        seen.push(*gh);
        if *suspended {
            let mut c = app.pool.acquire().await?;
            jobs::repos_of(&mut c, id, "inaccessible", false).await?;
            continue;
        }
        let repos = api.list_installation_repos(*gh).await?;
        let mut tx = app.pool.begin().await?;
        let mut ids = Vec::new();
        for r in &repos {
            ids.push(r["id"].as_i64().context("repository id missing")?);
            jobs::upsert_repo(&mut tx, r, Some(id), "accessible").await?;
        }
        sqlx::query(
            "UPDATE github_repos SET state = 'inaccessible', installation_id = NULL, updated_at = now()
             WHERE installation_id = $1 AND gh_repo_id <> ALL($2)",
        )
        .bind(id)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    // Installations GitHub no longer lists were deleted.
    let gone: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE github_installations SET state = 'deleted', updated_at = now()
         WHERE state <> 'deleted' AND gh_installation_id <> ALL($1) RETURNING id",
    )
    .bind(&seen)
    .fetch_all(&app.pool)
    .await?;
    for id in gone {
        let mut c = app.pool.acquire().await?;
        jobs::repos_of(&mut c, id, "inaccessible", true).await?;
    }
    Ok(())
}

/// The hourly reconcile (§19.5): installations and repositories, then for
/// every `ok` link (a) PRs updated after `synced_through − 10 min`, (b)
/// local open/draft PRs GitHub no longer lists as open, (c) their heads'
/// checks, (d) CI of PRs still pending (30 days) or merged/closed in the
/// last 7 days. The watermark moves to `now` only when the whole link
/// went through. How many links advanced.
pub async fn reconcile_full_once(
    app: &App,
    api: &dyn GithubApi,
    now: DateTime<Utc>,
) -> Result<usize> {
    reconcile_installations(app, api).await?;
    let mut n = 0;
    for l in links(app, "ok").await? {
        match reconcile_link(app, api, &l, now).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE project_repo_links SET synced_through = $2
                     WHERE id = $1 AND unlinked_at IS NULL AND sync_state = 'ok'",
                )
                .bind(l.id)
                .bind(now)
                .execute(&app.pool)
                .await?;
                n += 1;
            }
            Err(e) => failed(app, &l, "reconcile", e).await?,
        }
    }
    Ok(n)
}

async fn reconcile_link(
    app: &App,
    api: &dyn GithubApi,
    l: &LinkRow,
    now: DateTime<Utc>,
) -> Result<()> {
    let since = l.synced_through.unwrap_or(now) - Duration::minutes(WATERMARK_SLACK_MINUTES);
    let mut done = HashSet::new();
    // (a) everything updated since the watermark, open or not.
    for v in api
        .list_pulls(l.inst, &l.full_name, "all", Some(since))
        .await?
    {
        if updated_at(&v).is_some_and(|u| u >= since) {
            sync_pr(app, api, l, &v).await?;
            done.insert(v["number"].as_i64().unwrap_or(-1));
        }
    }
    // (b) locally open but neither updated nor listed as open remotely.
    let open: HashSet<i64> = api
        .list_pulls(l.inst, &l.full_name, "open", None)
        .await?
        .iter()
        .filter_map(|v| v["number"].as_i64())
        .collect();
    let local: Vec<i32> = sqlx::query_scalar(
        "SELECT number FROM github_pull_requests WHERE repo_id = $1 AND state IN ('open', 'draft')
         ORDER BY number",
    )
    .bind(l.repo)
    .fetch_all(&app.pool)
    .await?;
    for num in local {
        let k = i64::from(num);
        if done.contains(&k) || open.contains(&k) {
            continue;
        }
        if let Some(v) = api.get_pull(l.inst, &l.full_name, num).await? {
            sync_pr(app, api, l, &v).await?;
            done.insert(k);
        }
    }
    // (d) CI separately from the PR watermark.
    let heads: Vec<(i32, String)> = sqlx::query_as(
        "SELECT number, head_sha FROM github_pull_requests
         WHERE repo_id = $1 AND (
             (ci_state = 'pending' AND gh_updated_at > $2)
             OR (state IN ('merged', 'closed') AND gh_updated_at > $3))
         ORDER BY number",
    )
    .bind(l.repo)
    .bind(now - Duration::days(CI_PENDING_DAYS))
    .bind(now - Duration::days(CI_CLOSED_DAYS))
    .fetch_all(&app.pool)
    .await?;
    for (num, sha) in heads {
        if !done.contains(&i64::from(num)) {
            sync_checks(app, api, l, &sha).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- retention (§19.9)

pub const DELIVERY_KEEP_DAYS: i64 = 7;
pub const UNLINKED_KEEP_DAYS: i64 = 30;

/// Deliveries older than 7 days (never one still waiting for the worker);
/// PRs and checks of repositories that lost their last link 30 days ago.
/// `(deliveries, repositories)` cleared.
pub async fn sweep_once(app: &App, now: DateTime<Utc>) -> Result<(u64, u64)> {
    let mut tx = app.pool.begin().await?;
    let old = now - Duration::days(DELIVERY_KEEP_DAYS);
    sqlx::query(
        "DELETE FROM github_event_jobs j USING github_deliveries d
         WHERE d.delivery_id = j.delivery_id AND j.payload IS NULL AND d.received_at < $1",
    )
    .bind(old)
    .execute(&mut *tx)
    .await?;
    let deliveries = sqlx::query(
        "DELETE FROM github_deliveries d WHERE d.received_at < $1
         AND NOT EXISTS (SELECT 1 FROM github_event_jobs j WHERE j.delivery_id = d.delivery_id)",
    )
    .bind(old)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let repos: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM github_repos r
         WHERE NOT EXISTS (SELECT 1 FROM project_repo_links l WHERE l.repo_id = r.id AND l.unlinked_at IS NULL)
           AND (SELECT max(l.unlinked_at) FROM project_repo_links l WHERE l.repo_id = r.id) < $1
           AND (EXISTS (SELECT 1 FROM github_pull_requests p WHERE p.repo_id = r.id)
                OR EXISTS (SELECT 1 FROM github_commits c WHERE c.repo_id = r.id AND c.redacted_at IS NULL)
                OR EXISTS (SELECT 1 FROM github_check_runs c WHERE c.repo_id = r.id)
                OR EXISTS (SELECT 1 FROM github_check_suites s WHERE s.repo_id = r.id)
                OR EXISTS (SELECT 1 FROM github_commit_statuses s WHERE s.repo_id = r.id))
         FOR UPDATE OF r",
    )
    .bind(now - Duration::days(UNLINKED_KEEP_DAYS))
    .fetch_all(&mut *tx)
    .await?;
    for t in [
        "github_pull_requests",
        "github_check_runs",
        "github_check_suites",
        "github_commit_statuses",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {t} WHERE repo_id = ANY($1)"
        )))
        .bind(&repos)
        .execute(&mut *tx)
        .await?;
    }
    // R-C8: commits of those repositories, and commits without a ref that
    // were not seen for 30 days.
    super::commits::sweep(&mut tx, now - Duration::days(UNLINKED_KEEP_DAYS), &repos).await?;
    tx.commit().await?;
    Ok((deliveries, repos.len() as u64))
}
