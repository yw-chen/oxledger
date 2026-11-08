// SPDX-License-Identifier: GPL-2.0-or-later
//! Snapshots shared by webhook payloads and API responses, and the R-G8
//! write rules. Every PR and suite write locks the row first (`FOR
//! UPDATE`), so webhooks, backfill, reconciliation and refetches agree.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

pub const MAX_TITLE_CHARS: usize = 256;

fn s<'a>(v: &'a Json, k: &str) -> Result<&'a str> {
    v[k].as_str().with_context(|| format!("{k} missing"))
}

fn t(v: &Json, k: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s(v, k)?)?.with_timezone(&Utc))
}

fn opt_t(v: &Json, k: &str) -> Option<DateTime<Utc>> {
    v[k].as_str()
        .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
        .map(|x| x.with_timezone(&Utc))
}

fn digest(v: &Json) -> Vec<u8> {
    Sha256::digest([b"oxledger/github/v1\n".as_slice(), v.to_string().as_bytes()].concat()).to_vec()
}

/// Non-completed GitHub statuses (queued, in_progress, waiting, requested,
/// pending, anything unknown) are `pending`.
pub fn norm_status(gh: &str) -> &'static str {
    if gh == "completed" {
        "completed"
    } else {
        "pending"
    }
}

// ---------------------------------------------------------------- pull requests

#[derive(Debug, Clone, PartialEq)]
pub struct PrSnap {
    pub number: i32,
    pub title: String,
    pub head_ref: String,
    pub head_sha: String,
    pub state: String,
    pub html_url: String,
    pub updated_at: DateTime<Utc>,
    /// Users and teams still asked to review (R-G6 `pending`).
    pub requested: usize,
}

impl PrSnap {
    /// A `pull_request` object (webhook or REST, same shape).
    pub fn from_json(v: &Json) -> Result<PrSnap> {
        let state = if !v["merged_at"].is_null() {
            "merged"
        } else if s(v, "state")? == "closed" {
            "closed"
        } else if v["draft"].as_bool() == Some(true) {
            "draft"
        } else {
            "open"
        };
        let count = |k: &str| v[k].as_array().map_or(0, Vec::len);
        Ok(PrSnap {
            number: v["number"].as_i64().context("number missing")? as i32,
            title: s(v, "title")?.chars().take(MAX_TITLE_CHARS).collect(),
            head_ref: s(&v["head"], "ref")?.to_string(),
            head_sha: s(&v["head"], "sha")?.to_string(),
            state: state.into(),
            html_url: s(v, "html_url")?.to_string(),
            updated_at: t(v, "updated_at")?,
            requested: count("requested_reviewers") + count("requested_teams"),
        })
    }

    fn hash(&self) -> Vec<u8> {
        digest(&json!([
            self.title,
            self.head_ref,
            self.head_sha,
            self.state,
            self.html_url
        ]))
    }
}

/// What a snapshot write did (R-G8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// Taken (new row or later time); the row id.
    Applied(Uuid),
    /// Same time and content.
    Same(Uuid),
    /// Older than what is stored.
    Ignored(Uuid),
    /// Same time, different content: refetch through the API.
    Refetch(Uuid),
}

impl Write {
    pub fn id(self) -> Uuid {
        match self {
            Write::Applied(i) | Write::Same(i) | Write::Ignored(i) | Write::Refetch(i) => i,
        }
    }
}

async fn write_pr(conn: &mut PgConnection, id: Uuid, p: &PrSnap) -> Result<()> {
    sqlx::query(
        "UPDATE github_pull_requests SET title = $2, head_ref = $3, head_sha = $4, state = $5,
                html_url = $6, gh_updated_at = $7, content_hash = $8, row_version = row_version + 1
         WHERE id = $1",
    )
    .bind(id)
    .bind(&p.title)
    .bind(&p.head_ref)
    .bind(&p.head_sha)
    .bind(&p.state)
    .bind(&p.html_url)
    .bind(p.updated_at)
    .bind(p.hash())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Locks (creating if needed) the PR row: `(id, gh_updated_at, hash, row_version)`.
async fn lock_pr(
    conn: &mut PgConnection,
    repo: Uuid,
    p: &PrSnap,
) -> Result<(Uuid, Option<(DateTime<Utc>, Vec<u8>, i64)>)> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO github_pull_requests
             (repo_id, number, title, head_ref, head_sha, state, review_state, html_url, gh_updated_at, content_hash)
         VALUES ($1, $2, $3, $4, $5, $6, 'none', $7, $8, $9)
         ON CONFLICT (repo_id, number) DO NOTHING RETURNING id",
    )
    .bind(repo)
    .bind(p.number)
    .bind(&p.title)
    .bind(&p.head_ref)
    .bind(&p.head_sha)
    .bind(&p.state)
    .bind(&p.html_url)
    .bind(p.updated_at)
    .bind(p.hash())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = inserted {
        return Ok((id, None));
    }
    let (id, at, h, rv): (Uuid, DateTime<Utc>, Vec<u8>, i64) = sqlx::query_as(
        "SELECT id, gh_updated_at, content_hash, row_version FROM github_pull_requests
         WHERE repo_id = $1 AND number = $2 FOR UPDATE",
    )
    .bind(repo)
    .bind(p.number)
    .fetch_one(&mut *conn)
    .await?;
    Ok((id, Some((at, h, rv))))
}

/// R-G8 for a snapshot from a webhook, backfill or reconciliation.
pub async fn upsert_pr(conn: &mut PgConnection, repo: Uuid, p: &PrSnap) -> Result<Write> {
    let (id, cur) = lock_pr(conn, repo, p).await?;
    let Some((at, h, _)) = cur else {
        return Ok(Write::Applied(id));
    };
    Ok(if p.updated_at > at {
        write_pr(conn, id, p).await?;
        Write::Applied(id)
    } else if p.updated_at < at {
        Write::Ignored(id)
    } else if h == p.hash() {
        Write::Same(id)
    } else {
        Write::Refetch(id)
    })
}

/// R-G8 for a refetched snapshot: `seen` is the row_version read before
/// the API call. Unchanged → GitHub's answer is the current state and is
/// taken; changed (another write meanwhile) → the time rule, `Refetch`
/// again when it still cannot tell.
pub async fn apply_refetched_pr(
    conn: &mut PgConnection,
    repo: Uuid,
    p: &PrSnap,
    seen: i64,
) -> Result<Write> {
    let (id, cur) = lock_pr(conn, repo, p).await?;
    let Some((at, h, rv)) = cur else {
        return Ok(Write::Applied(id));
    };
    if rv == seen {
        if at == p.updated_at && h == p.hash() {
            return Ok(Write::Same(id));
        }
        write_pr(conn, id, p).await?;
        return Ok(Write::Applied(id));
    }
    upsert_pr(conn, repo, p).await
}

pub async fn pr_row_version(
    conn: &mut PgConnection,
    repo: Uuid,
    number: i32,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT row_version FROM github_pull_requests WHERE repo_id = $1 AND number = $2",
    )
    .bind(repo)
    .bind(number)
    .fetch_optional(&mut *conn)
    .await?)
}

// ---------------------------------------------------------------- check suites

#[derive(Debug, Clone, PartialEq)]
pub struct SuiteSnap {
    pub id: i64,
    pub head_sha: String,
    pub app_id: i64,
    pub status: String,
    pub conclusion: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl SuiteSnap {
    pub fn from_json(v: &Json) -> Result<SuiteSnap> {
        Ok(SuiteSnap {
            id: v["id"].as_i64().context("suite id missing")?,
            head_sha: s(v, "head_sha")?.to_string(),
            app_id: v["app"]["id"].as_i64().unwrap_or(0),
            status: norm_status(s(v, "status")?).into(),
            conclusion: v["conclusion"].as_str().map(String::from),
            updated_at: t(v, "updated_at")?,
        })
    }

    fn hash(&self) -> Vec<u8> {
        digest(&json!([self.head_sha, self.status, self.conclusion]))
    }
}

async fn write_suite(conn: &mut PgConnection, s: &SuiteSnap) -> Result<()> {
    sqlx::query(
        "UPDATE github_check_suites SET head_sha = $2, app_id = $3, status = $4, conclusion = $5,
                gh_updated_at = $6, content_hash = $7, row_version = row_version + 1
         WHERE gh_suite_id = $1",
    )
    .bind(s.id)
    .bind(&s.head_sha)
    .bind(s.app_id)
    .bind(&s.status)
    .bind(&s.conclusion)
    .bind(s.updated_at)
    .bind(s.hash())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn lock_suite(
    conn: &mut PgConnection,
    repo: Uuid,
    s: &SuiteSnap,
) -> Result<Option<(DateTime<Utc>, Vec<u8>, i64)>> {
    let inserted = sqlx::query(
        "INSERT INTO github_check_suites
             (gh_suite_id, repo_id, head_sha, app_id, status, conclusion, gh_updated_at, content_hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (gh_suite_id) DO NOTHING",
    )
    .bind(s.id)
    .bind(repo)
    .bind(&s.head_sha)
    .bind(s.app_id)
    .bind(&s.status)
    .bind(&s.conclusion)
    .bind(s.updated_at)
    .bind(s.hash())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(None);
    }
    Ok(Some(
        sqlx::query_as(
            "SELECT gh_updated_at, content_hash, row_version FROM github_check_suites
             WHERE gh_suite_id = $1 FOR UPDATE",
        )
        .bind(s.id)
        .fetch_one(&mut *conn)
        .await?,
    ))
}

/// R-G8 for suites: only time decides (a suite may go back to pending
/// when re-run); same time, different content → refetch.
pub async fn upsert_suite(conn: &mut PgConnection, repo: Uuid, s: &SuiteSnap) -> Result<Write> {
    let nil = Uuid::nil();
    let Some((at, h, _)) = lock_suite(conn, repo, s).await? else {
        return Ok(Write::Applied(nil));
    };
    Ok(if s.updated_at > at {
        write_suite(conn, s).await?;
        Write::Applied(nil)
    } else if s.updated_at < at {
        Write::Ignored(nil)
    } else if h == s.hash() {
        Write::Same(nil)
    } else {
        Write::Refetch(nil)
    })
}

pub async fn apply_refetched_suite(
    conn: &mut PgConnection,
    repo: Uuid,
    s: &SuiteSnap,
    seen: i64,
) -> Result<Write> {
    let nil = Uuid::nil();
    let Some((at, h, rv)) = lock_suite(conn, repo, s).await? else {
        return Ok(Write::Applied(nil));
    };
    if rv == seen {
        if at == s.updated_at && h == s.hash() {
            return Ok(Write::Same(nil));
        }
        write_suite(conn, s).await?;
        return Ok(Write::Applied(nil));
    }
    upsert_suite(conn, repo, s).await
}

pub async fn suite_row_version(conn: &mut PgConnection, id: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT row_version FROM github_check_suites WHERE gh_suite_id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

// ---------------------------------------------------------------- check runs

#[derive(Debug, Clone, PartialEq)]
pub struct RunSnap {
    pub id: i64,
    pub head_sha: String,
    pub app_id: i64,
    pub suite_id: Option<i64>,
    pub name: String,
    pub gh_status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl RunSnap {
    pub fn from_json(v: &Json) -> Result<RunSnap> {
        Ok(RunSnap {
            id: v["id"].as_i64().context("run id missing")?,
            head_sha: s(v, "head_sha")?.to_string(),
            app_id: v["app"]["id"].as_i64().unwrap_or(0),
            suite_id: v["check_suite"]["id"].as_i64(),
            name: s(v, "name")?.to_string(),
            gh_status: s(v, "status")?.to_string(),
            conclusion: v["conclusion"].as_str().map(String::from),
            started_at: opt_t(v, "started_at"),
            completed_at: opt_t(v, "completed_at"),
        })
    }

    fn status(&self) -> &'static str {
        norm_status(&self.gh_status)
    }
}

/// R-G8 for runs: a completed run is never set back to not completed; two
/// different completed conclusions → `Refetch` (the caller asks the API).
pub async fn upsert_run(conn: &mut PgConnection, repo: Uuid, r: &RunSnap) -> Result<Write> {
    let nil = Uuid::nil();
    // A new run: INSERT first. A concurrent insert of the same run makes this
    // wait for it and do nothing; the row is then locked and compared below,
    // so a completed run is never set back by a racing older event.
    if insert_run(conn, repo, r).await? {
        return Ok(Write::Applied(nil));
    }
    let cur: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, conclusion FROM github_check_runs WHERE gh_run_id = $1 FOR UPDATE",
    )
    .bind(r.id)
    .fetch_optional(&mut *conn)
    .await?;
    let status = r.status();
    match cur {
        Some((st, _)) if st == "completed" && status != "completed" => {
            return Ok(Write::Ignored(nil));
        }
        Some((st, c)) if st == "completed" && c != r.conclusion => return Ok(Write::Refetch(nil)),
        Some((st, c)) if st == "completed" && c == r.conclusion => return Ok(Write::Same(nil)),
        _ => {}
    }
    force_run(conn, repo, r).await?;
    Ok(Write::Applied(nil))
}

async fn insert_run(conn: &mut PgConnection, repo: Uuid, r: &RunSnap) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO github_check_runs (gh_run_id, repo_id, head_sha, app_id, check_suite_id, name,
                                        status, gh_status, conclusion, started_at, completed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (gh_run_id) DO NOTHING",
    )
    .bind(r.id)
    .bind(repo)
    .bind(&r.head_sha)
    .bind(r.app_id)
    .bind(r.suite_id)
    .bind(&r.name)
    .bind(r.status())
    .bind(&r.gh_status)
    .bind(&r.conclusion)
    .bind(r.started_at)
    .bind(r.completed_at)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1)
}

/// Writes the run as GitHub reports it now (after a refetch, or once
/// `upsert_run` holds the row).
pub async fn force_run(conn: &mut PgConnection, repo: Uuid, r: &RunSnap) -> Result<()> {
    sqlx::query(
        "INSERT INTO github_check_runs (gh_run_id, repo_id, head_sha, app_id, check_suite_id, name,
                                        status, gh_status, conclusion, started_at, completed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (gh_run_id) DO UPDATE SET head_sha = $3, app_id = $4, check_suite_id = $5,
             name = $6, status = $7, gh_status = $8, conclusion = $9, started_at = $10, completed_at = $11",
    )
    .bind(r.id)
    .bind(repo)
    .bind(&r.head_sha)
    .bind(r.app_id)
    .bind(r.suite_id)
    .bind(&r.name)
    .bind(r.status())
    .bind(&r.gh_status)
    .bind(&r.conclusion)
    .bind(r.started_at)
    .bind(r.completed_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- commit statuses

#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnap {
    pub id: i64,
    pub sha: String,
    pub context: String,
    pub state: String,
}

impl StatusSnap {
    pub fn from_json(v: &Json) -> Result<StatusSnap> {
        let state = s(v, "state")?;
        if !["pending", "success", "failure", "error"].contains(&state) {
            return Err(anyhow!("unknown status state"));
        }
        Ok(StatusSnap {
            id: v["id"].as_i64().context("status id missing")?,
            sha: s(v, "sha")?.to_string(),
            context: s(v, "context")?.to_string(),
            state: state.into(),
        })
    }
}

/// Per sha and context the larger `gh_status_id` wins.
pub async fn upsert_status(conn: &mut PgConnection, repo: Uuid, s: &StatusSnap) -> Result<()> {
    sqlx::query(
        "INSERT INTO github_commit_statuses (repo_id, head_sha, context, gh_status_id, state)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (repo_id, head_sha, context) DO UPDATE SET gh_status_id = $4, state = $5
         WHERE github_commit_statuses.gh_status_id < $4",
    )
    .bind(repo)
    .bind(&s.sha)
    .bind(&s.context)
    .bind(s.id)
    .bind(&s.state)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- roll-ups

/// R-G7 over the PR's current head: suites of its runs (a run whose suite
/// is not stored yet counts alone), runs without a suite, statuses.
/// Suites that no run of this head refers to are not units (GitHub
/// creates queued suites for apps that never run; they would stay pending).
pub async fn recompute_ci(conn: &mut PgConnection, pr: Uuid) -> Result<Option<&'static str>> {
    use super::rollup::{Unit, ci_state, status_unit};
    let (repo, sha): (Uuid, String) =
        sqlx::query_as("SELECT repo_id, head_sha FROM github_pull_requests WHERE id = $1")
            .bind(pr)
            .fetch_one(&mut *conn)
            .await?;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT coalesce(s.status, r.status), CASE WHEN s.gh_suite_id IS NULL THEN r.conclusion ELSE s.conclusion END
         FROM github_check_runs r
         LEFT JOIN github_check_suites s ON s.gh_suite_id = r.check_suite_id
         WHERE r.repo_id = $1 AND r.head_sha = $2 AND (r.check_suite_id IS NULL OR s.gh_suite_id IS NULL)
         UNION ALL
         SELECT s.status, s.conclusion FROM github_check_suites s
         WHERE s.repo_id = $1 AND s.head_sha = $2
           AND EXISTS (SELECT 1 FROM github_check_runs r WHERE r.check_suite_id = s.gh_suite_id)
         UNION ALL
         SELECT CASE WHEN state = 'pending' THEN 'pending' ELSE 'completed' END, state
         FROM github_commit_statuses WHERE repo_id = $1 AND head_sha = $2",
    )
    .bind(repo)
    .bind(&sha)
    .fetch_all(&mut *conn)
    .await?;
    let units: Vec<Unit> = rows
        .into_iter()
        .map(|(st, c)| {
            if st == "pending" {
                status_unit("pending")
            } else {
                Unit {
                    pending: false,
                    conclusion: c,
                }
            }
        })
        .collect();
    let ci = ci_state(&units);
    sqlx::query("UPDATE github_pull_requests SET ci_state = $2 WHERE id = $1")
        .bind(pr)
        .bind(ci)
        .execute(&mut *conn)
        .await?;
    Ok(ci)
}

/// Recomputes CI for every PR of `repo` whose head is `sha`.
pub async fn recompute_ci_for_sha(conn: &mut PgConnection, repo: Uuid, sha: &str) -> Result<()> {
    let prs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM github_pull_requests WHERE repo_id = $1 AND head_sha = $2 ORDER BY id FOR UPDATE",
    )
    .bind(repo)
    .bind(sha)
    .fetch_all(&mut *conn)
    .await?;
    for p in prs {
        recompute_ci(&mut *conn, p).await?;
    }
    Ok(())
}

pub async fn set_review_state(conn: &mut PgConnection, pr: Uuid, state: &str) -> Result<()> {
    sqlx::query("UPDATE github_pull_requests SET review_state = $2 WHERE id = $1")
        .bind(pr)
        .bind(state)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// R-G1, R-G2: rebuilds the PR's refs for the locked links only.
pub async fn recompute_refs(
    conn: &mut PgConnection,
    pr: Uuid,
    links: &[(Uuid, Uuid)],
    title: &str,
    branch: &str,
    commits: &[String],
) -> Result<()> {
    let link_ids: Vec<Uuid> = links.iter().map(|l| l.0).collect();
    sqlx::query("DELETE FROM pr_issue_refs WHERE pr_id = $1 AND link_id = ANY($2)")
        .bind(pr)
        .bind(&link_ids)
        .execute(&mut *conn)
        .await?;
    for (key, sources) in super::refs::collect(title, branch, commits) {
        let hit: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT i.id, i.project_id FROM issue_keys k JOIN issues i ON i.id = k.issue_id
             WHERE k.key = $1 LIMIT 1",
        )
        .bind(&key)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((issue, project)) = hit else {
            continue;
        };
        let Some((link, _)) = links.iter().find(|(_, p)| *p == project) else {
            continue;
        };
        sqlx::query(
            "INSERT INTO pr_issue_refs (pr_id, issue_id, project_id, link_id, sources)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (pr_id, issue_id) DO UPDATE SET link_id = $4, sources = $5",
        )
        .bind(pr)
        .bind(issue)
        .bind(project)
        .bind(link)
        .bind(sources.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
