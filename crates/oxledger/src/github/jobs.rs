// SPDX-License-Identifier: GPL-2.0-or-later
//! The webhook job worker (§19.5): one event per transaction, `FOR UPDATE
//! SKIP LOCKED`, exponential backoff, at most 5 attempts. Refetches (R-G8)
//! run after the event's transaction, each in its own.

use super::client::GithubApi;
use super::model::{self, PrSnap, RunSnap, StatusSnap, SuiteSnap, Write};
use super::rollup::review_state;
use crate::service::App;
use anyhow::{Context, Result};
use serde_json::Value as Json;
use sqlx::PgConnection;
use uuid::Uuid;

pub const MAX_ATTEMPTS: i16 = 5;
pub const MAX_REFETCHES: usize = 3;

/// A row that needs `GET` from the API (R-G8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refetch {
    Pr {
        repo: Uuid,
        inst: i64,
        full_name: String,
        number: i32,
    },
    Suite {
        repo: Uuid,
        inst: i64,
        full_name: String,
        id: i64,
    },
    Run {
        repo: Uuid,
        inst: i64,
        full_name: String,
        id: i64,
    },
}

/// The outcome of one event.
pub struct Outcome {
    pub result: &'static str,
    pub refetch: Vec<Refetch>,
}

fn ok(refetch: Vec<Refetch>) -> Outcome {
    Outcome {
        result: "ok",
        refetch,
    }
}

fn ignored() -> Outcome {
    Outcome {
        result: "ignored",
        refetch: Vec::new(),
    }
}

/// Takes and processes at most one due job; how many it took (0 or 1).
pub async fn run_jobs_once(app: &App, api: &dyn GithubApi) -> Result<usize> {
    let mut tx = app.pool.begin().await?;
    let job: Option<(Uuid, String, Json, i16)> = sqlx::query_as(
        "SELECT j.delivery_id, d.event, j.payload, j.attempts
         FROM github_event_jobs j JOIN github_deliveries d ON d.delivery_id = j.delivery_id
         WHERE j.payload IS NOT NULL AND j.next_at <= clock_timestamp()
         ORDER BY j.next_at FOR UPDATE OF j SKIP LOCKED LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((delivery, event, payload, attempts)) = job else {
        tx.commit().await?;
        return Ok(0);
    };
    sqlx::query("SAVEPOINT github_event")
        .execute(&mut *tx)
        .await?;
    match process(&mut tx, api, &event, &payload).await {
        Ok(out) => {
            sqlx::query("RELEASE SAVEPOINT github_event")
                .execute(&mut *tx)
                .await?;
            finish(&mut tx, delivery, out.result).await?;
            tx.commit().await?;
            let mut result = None;
            for r in out.refetch {
                match refetch(app, api, &r).await {
                    Ok(Some(r)) => result = Some(r),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(%delivery, "github refetch failed: {}", e.root_cause());
                        result = Some("refetch_failed");
                    }
                }
            }
            if let Some(r) = result {
                sqlx::query("UPDATE github_deliveries SET result = $2 WHERE delivery_id = $1")
                    .bind(delivery)
                    .bind(r)
                    .execute(&app.pool)
                    .await?;
            }
        }
        Err(e) => {
            sqlx::query("ROLLBACK TO SAVEPOINT github_event")
                .execute(&mut *tx)
                .await?;
            // The message never carries the payload.
            tracing::warn!(%delivery, %event, attempt = attempts + 1, "github event failed: {}", e.root_cause());
            let attempts = attempts + 1;
            if attempts >= MAX_ATTEMPTS {
                sqlx::query("UPDATE github_event_jobs SET attempts = $2 WHERE delivery_id = $1")
                    .bind(delivery)
                    .bind(attempts)
                    .execute(&mut *tx)
                    .await?;
                finish(&mut tx, delivery, "error").await?;
            } else {
                sqlx::query(
                    "UPDATE github_event_jobs SET attempts = $2,
                            next_at = clock_timestamp() + make_interval(secs => power(2, $2)::double precision)
                     WHERE delivery_id = $1",
                )
                .bind(delivery)
                .bind(attempts)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        }
    }
    Ok(1)
}

/// Runs jobs until none is due; how many were taken.
pub async fn drain(app: &App, api: &dyn GithubApi) -> Result<usize> {
    let mut n = 0;
    while run_jobs_once(app, api).await? == 1 {
        n += 1;
    }
    Ok(n)
}

async fn finish(conn: &mut PgConnection, delivery: Uuid, result: &str) -> Result<()> {
    sqlx::query("UPDATE github_event_jobs SET payload = NULL WHERE delivery_id = $1")
        .bind(delivery)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE github_deliveries SET processed_at = clock_timestamp(), result = $2 WHERE delivery_id = $1",
    )
    .bind(delivery)
    .bind(result)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- dispatch

pub async fn process(
    conn: &mut PgConnection,
    api: &dyn GithubApi,
    event: &str,
    p: &Json,
) -> Result<Outcome> {
    match event {
        "installation" => installation(conn, p).await,
        "installation_repositories" => installation_repositories(conn, p).await,
        "repository" => repository(conn, p).await,
        "push" => super::commits::push(conn, api, p).await,
        "pull_request" | "pull_request_review" | "check_run" | "check_suite" | "status" => {
            content(conn, api, event, p).await
        }
        _ => Ok(ignored()),
    }
}

pub(crate) async fn upsert_installation(
    conn: &mut PgConnection,
    inst: &Json,
    state: &str,
) -> Result<Uuid> {
    let gh = inst["id"].as_i64().context("installation id missing")?;
    let login = inst["account"]["login"].as_str().unwrap_or("");
    Ok(sqlx::query_scalar(
        "INSERT INTO github_installations (gh_installation_id, account_login, state)
         VALUES ($1, $2, $3)
         ON CONFLICT (gh_installation_id) DO UPDATE
             SET account_login = CASE WHEN $2 = '' THEN github_installations.account_login ELSE $2 END,
                 state = $3, updated_at = now()
         RETURNING id",
    )
    .bind(gh)
    .bind(login)
    .bind(state)
    .fetch_one(&mut *conn)
    .await?)
}

/// A repository object (installation payloads carry a short one).
pub async fn upsert_repo(
    conn: &mut PgConnection,
    repo: &Json,
    installation: Option<Uuid>,
    state: &str,
) -> Result<Uuid> {
    let gh = repo["id"].as_i64().context("repository id missing")?;
    let name = repo["full_name"].as_str().context("full_name missing")?;
    let private = repo["private"].as_bool().unwrap_or(true);
    // R-C2: installation payloads carry a short object without it.
    let default_branch = repo["default_branch"]
        .as_str()
        .filter(|b| !b.is_empty())
        .map(|b| {
            b.chars()
                .filter(|c| *c != '\0')
                .take(super::commits::MAX_REF_CHARS)
                .collect::<String>()
        });
    Ok(sqlx::query_scalar(
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, state, default_branch)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (gh_repo_id) DO UPDATE SET installation_id = $2, full_name = $3,
             is_private = $4, state = $5, updated_at = now(),
             default_branch = coalesce($6, github_repos.default_branch)
         RETURNING id",
    )
    .bind(gh)
    .bind(installation)
    .bind(name)
    .bind(private)
    .bind(state)
    .bind(default_branch)
    .fetch_one(&mut *conn)
    .await?)
}

pub(crate) async fn repos_of(
    conn: &mut PgConnection,
    inst: Uuid,
    state: &str,
    detach: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE github_repos SET state = $2, updated_at = now(),
                installation_id = CASE WHEN $3 THEN NULL ELSE installation_id END
         WHERE installation_id = $1",
    )
    .bind(inst)
    .bind(state)
    .bind(detach)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn installation(conn: &mut PgConnection, p: &Json) -> Result<Outcome> {
    let action = p["action"].as_str().unwrap_or("");
    let inst = &p["installation"];
    match action {
        "created" | "new_permissions_accepted" | "unsuspend" => {
            let id = upsert_installation(conn, inst, "active").await?;
            if action == "unsuspend" {
                repos_of(conn, id, "accessible", false).await?;
            }
            for r in p["repositories"].as_array().into_iter().flatten() {
                upsert_repo(conn, r, Some(id), "accessible").await?;
            }
        }
        "suspend" => {
            let id = upsert_installation(conn, inst, "suspended").await?;
            repos_of(conn, id, "inaccessible", false).await?;
        }
        "deleted" => {
            let id = upsert_installation(conn, inst, "deleted").await?;
            repos_of(conn, id, "inaccessible", true).await?;
        }
        _ => return Ok(ignored()),
    }
    Ok(ok(Vec::new()))
}

async fn installation_repositories(conn: &mut PgConnection, p: &Json) -> Result<Outcome> {
    let gh = p["installation"]["id"]
        .as_i64()
        .context("installation id missing")?;
    let id: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, state FROM github_installations WHERE gh_installation_id = $1 FOR UPDATE",
    )
    .bind(gh)
    .fetch_optional(&mut *conn)
    .await?;
    let (id, state) = match id {
        Some(x) => x,
        None => (
            upsert_installation(conn, &p["installation"], "active").await?,
            "active".into(),
        ),
    };
    let live = if state == "active" {
        "accessible"
    } else {
        "inaccessible"
    };
    for r in p["repositories_added"].as_array().into_iter().flatten() {
        upsert_repo(conn, r, Some(id), live).await?;
    }
    for r in p["repositories_removed"].as_array().into_iter().flatten() {
        if let Some(gh) = r["id"].as_i64() {
            sqlx::query(
                "UPDATE github_repos SET state = 'inaccessible', installation_id = NULL, updated_at = now()
                 WHERE gh_repo_id = $1 AND installation_id = $2",
            )
            .bind(gh)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(ok(Vec::new()))
}

async fn repository(conn: &mut PgConnection, p: &Json) -> Result<Outcome> {
    let r = &p["repository"];
    let gh = r["id"].as_i64().context("repository id missing")?;
    let action = p["action"].as_str().unwrap_or("");
    super::commits::note_default_branch(conn, r).await?;
    let n = sqlx::query(
        "UPDATE github_repos SET full_name = coalesce($2, full_name), is_private = coalesce($3, is_private),
                state = CASE WHEN $4 THEN 'inaccessible' ELSE state END, updated_at = now()
         WHERE gh_repo_id = $1",
    )
    .bind(gh)
    .bind(r["full_name"].as_str())
    .bind(r["private"].as_bool())
    .bind(action == "deleted")
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(if n == 0 { ignored() } else { ok(Vec::new()) })
}

/// The repository and its live links, or `None` (→ `ignored`).
pub struct Target {
    pub repo: Uuid,
    pub inst: i64,
    pub full_name: String,
    /// `(link_id, project_id)`, locked `FOR SHARE` in id order.
    pub links: Vec<(Uuid, Uuid)>,
}

/// §19.5: installation active, repository accessible, live links locked.
pub async fn target(conn: &mut PgConnection, gh_repo: i64) -> Result<Option<Target>> {
    let r: Option<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT r.id, r.full_name, i.gh_installation_id FROM github_repos r
         JOIN github_installations i ON i.id = r.installation_id
         WHERE r.gh_repo_id = $1 AND r.state = 'accessible' AND i.state = 'active'",
    )
    .bind(gh_repo)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((repo, full_name, inst)) = r else {
        return Ok(None);
    };
    target_of(conn, repo, inst, full_name).await
}

pub async fn target_of(
    conn: &mut PgConnection,
    repo: Uuid,
    inst: i64,
    full_name: String,
) -> Result<Option<Target>> {
    // Refetches and API passes come here directly: the App must still
    // reach the repository (§19.5, revoked access stops updates).
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_repos r JOIN github_installations i ON i.id = r.installation_id
                        WHERE r.id = $1 AND r.state = 'accessible' AND i.state = 'active')",
    )
    .bind(repo)
    .fetch_one(&mut *conn)
    .await?;
    if !live {
        return Ok(None);
    }
    let links: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, project_id FROM project_repo_links
         WHERE repo_id = $1 AND unlinked_at IS NULL ORDER BY id FOR SHARE",
    )
    .bind(repo)
    .fetch_all(&mut *conn)
    .await?;
    crate::github::fault("github.after_links").await?;
    if links.is_empty() {
        return Ok(None);
    }
    Ok(Some(Target {
        repo,
        inst,
        full_name,
        links,
    }))
}

/// After a PR snapshot was taken: commits → refs, reviews, CI.
pub async fn pr_details(
    conn: &mut PgConnection,
    api: &dyn GithubApi,
    t: &Target,
    pr: Uuid,
    snap: &PrSnap,
) -> Result<Vec<String>> {
    let commits = api
        .list_pull_commits(t.inst, &t.full_name, snap.number)
        .await?;
    model::recompute_refs(conn, pr, &t.links, &snap.title, &snap.head_ref, &commits).await?;
    pr_reviews(conn, api, t, pr, snap).await?;
    model::recompute_ci(conn, pr).await?;
    Ok(commits)
}

async fn pr_reviews(
    conn: &mut PgConnection,
    api: &dyn GithubApi,
    t: &Target,
    pr: Uuid,
    snap: &PrSnap,
) -> Result<()> {
    let reviews = api.list_reviews(t.inst, &t.full_name, snap.number).await?;
    model::set_review_state(conn, pr, review_state(&reviews, snap.requested)).await
}

async fn content(
    conn: &mut PgConnection,
    api: &dyn GithubApi,
    event: &str,
    p: &Json,
) -> Result<Outcome> {
    let gh_repo = p["repository"]["id"]
        .as_i64()
        .context("repository id missing")?;
    super::commits::note_default_branch(conn, &p["repository"]).await?;
    let Some(t) = target(conn, gh_repo).await? else {
        return Ok(ignored());
    };
    let mut refetch = Vec::new();
    let mut result = "ok";
    match event {
        "pull_request" | "pull_request_review" => {
            let snap = PrSnap::from_json(&p["pull_request"])?;
            let w = model::upsert_pr(conn, t.repo, &snap).await?;
            let mut commits = None;
            match w {
                Write::Applied(id) => commits = Some(pr_details(conn, api, &t, id, &snap).await?),
                Write::Same(id) | Write::Ignored(id) if event == "pull_request_review" => {
                    pr_reviews(conn, api, &t, id, &snap).await?
                }
                Write::Refetch(_) => refetch.push(Refetch::Pr {
                    repo: t.repo,
                    inst: t.inst,
                    full_name: t.full_name.clone(),
                    number: snap.number,
                }),
                _ => {}
            }
            // Design 06 R-F1 (a): fix keywords of a PR merged into the
            // default branch (whatever the snapshot's fate: each (issue,
            // PR) is recorded once).
            if event == "pull_request"
                && p["action"].as_str() == Some("closed")
                && p["pull_request"]["merged"].as_bool() == Some(true)
            {
                let commits = match commits {
                    Some(c) => c,
                    None => {
                        api.list_pull_commits(t.inst, &t.full_name, snap.number)
                            .await?
                    }
                };
                if super::fixes::pr_merged(conn, &t, w.id(), p, &commits).await? {
                    result = "fix_links_truncated";
                }
            }
        }
        "check_suite" if p["check_suite"]["updated_at"].as_str().is_none() => {
            // B29: no time to compare → ask the API.
            refetch.push(Refetch::Suite {
                repo: t.repo,
                inst: t.inst,
                full_name: t.full_name.clone(),
                id: p["check_suite"]["id"]
                    .as_i64()
                    .context("suite id missing")?,
            });
        }
        "check_suite" => {
            let s = SuiteSnap::from_json(&p["check_suite"])?;
            match model::upsert_suite(conn, t.repo, &s).await? {
                Write::Applied(_) => model::recompute_ci_for_sha(conn, t.repo, &s.head_sha).await?,
                Write::Refetch(_) => refetch.push(Refetch::Suite {
                    repo: t.repo,
                    inst: t.inst,
                    full_name: t.full_name.clone(),
                    id: s.id,
                }),
                _ => {}
            }
        }
        "check_run" => {
            let r = RunSnap::from_json(&p["check_run"])?;
            match model::upsert_run(conn, t.repo, &r).await? {
                Write::Applied(_) => model::recompute_ci_for_sha(conn, t.repo, &r.head_sha).await?,
                Write::Refetch(_) => refetch.push(Refetch::Run {
                    repo: t.repo,
                    inst: t.inst,
                    full_name: t.full_name.clone(),
                    id: r.id,
                }),
                _ => {}
            }
        }
        _ => {
            let s = StatusSnap::from_json(p)?;
            model::upsert_status(conn, t.repo, &s).await?;
            model::recompute_ci_for_sha(conn, t.repo, &s.sha).await?;
        }
    }
    Ok(Outcome { result, refetch })
}

// ---------------------------------------------------------------- refetch (R-G8)

/// `Some(result)` when the delivery's result should change.
pub async fn refetch(app: &App, api: &dyn GithubApi, r: &Refetch) -> Result<Option<&'static str>> {
    match r {
        Refetch::Pr {
            repo,
            inst,
            full_name,
            number,
        } => {
            for _ in 0..MAX_REFETCHES {
                let mut c = app.pool.acquire().await?;
                let seen = model::pr_row_version(&mut c, *repo, *number)
                    .await?
                    .unwrap_or(-1);
                drop(c);
                let Some(v) = api.get_pull(*inst, full_name, *number).await? else {
                    return Ok(None);
                };
                let snap = PrSnap::from_json(&v)?;
                let mut tx = app.pool.begin().await?;
                let Some(t) = target_of(&mut tx, *repo, *inst, full_name.clone()).await? else {
                    return Ok(None);
                };
                match model::apply_refetched_pr(&mut tx, *repo, &snap, seen).await? {
                    Write::Applied(id) => {
                        pr_details(&mut tx, api, &t, id, &snap).await?;
                    }
                    Write::Refetch(_) => {
                        tx.commit().await?;
                        continue;
                    }
                    _ => {}
                }
                tx.commit().await?;
                return Ok(None);
            }
            Ok(Some("refetch_gave_up"))
        }
        Refetch::Suite {
            repo,
            inst,
            full_name,
            id,
        } => {
            for _ in 0..MAX_REFETCHES {
                let mut c = app.pool.acquire().await?;
                let seen = model::suite_row_version(&mut c, *id).await?.unwrap_or(-1);
                drop(c);
                let Some(v) = api.get_check_suite(*inst, full_name, *id).await? else {
                    return Ok(None);
                };
                let s = SuiteSnap::from_json(&v)?;
                let mut tx = app.pool.begin().await?;
                if target_of(&mut tx, *repo, *inst, full_name.clone())
                    .await?
                    .is_none()
                {
                    return Ok(None);
                }
                match model::apply_refetched_suite(&mut tx, *repo, &s, seen).await? {
                    Write::Applied(_) => {
                        model::recompute_ci_for_sha(&mut tx, *repo, &s.head_sha).await?
                    }
                    Write::Refetch(_) => {
                        tx.commit().await?;
                        continue;
                    }
                    _ => {}
                }
                tx.commit().await?;
                return Ok(None);
            }
            Ok(Some("refetch_gave_up"))
        }
        Refetch::Run {
            repo,
            inst,
            full_name,
            id,
        } => {
            let Some(v) = api.get_check_run(*inst, full_name, *id).await? else {
                return Ok(None);
            };
            let r = RunSnap::from_json(&v)?;
            let mut tx = app.pool.begin().await?;
            if target_of(&mut tx, *repo, *inst, full_name.clone())
                .await?
                .is_none()
            {
                return Ok(None);
            }
            model::force_run(&mut tx, *repo, &r).await?;
            model::recompute_ci_for_sha(&mut tx, *repo, &r.head_sha).await?;
            tx.commit().await?;
            Ok(None)
        }
    }
}
