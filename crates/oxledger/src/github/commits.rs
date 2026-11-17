// SPDX-License-Identifier: GPL-2.0-or-later
//! Commits from `push` events (design 06 §3.7, R-C1–R-C11): stored per
//! repository, linked to the issues their messages name (R-G1) through the
//! live links the worker holds `FOR SHARE`. Read-only towards GitHub.
//!
//! What a commit row keeps is fixed at its first sight: a later push never
//! rewrites `message`, `author_login` or `sender_login`, so a message that
//! was revoked (`redacted_at`) stays `[已遮蔽]` (R-C3, R-C7).

use super::client::GithubApi;
use super::jobs::{Outcome, Target};
use super::refs::{self, Source};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::PgConnection;
use uuid::Uuid;

/// R-C4: GitHub puts at most this many commits in one push payload.
pub const PAYLOAD_COMMIT_LIMIT: usize = 2048;
/// R-C4: commits fetched from the compare API for one push.
pub const COMPARE_LIMIT: usize = 1000;
pub const MAX_MESSAGE_CHARS: usize = 4096;
pub const MAX_LOGIN_CHARS: usize = 100;
pub const MAX_REF_CHARS: usize = 255;
/// What a revoked commit's message becomes (R-C7).
pub const REDACTED: &str = "[已遮蔽]";

/// One commit as a push payload or the compare API gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCommit {
    pub sha: String,
    pub message: String,
    /// `author.username`: not verified by GitHub.
    pub author_login: Option<String>,
    pub committed_at: Option<DateTime<Utc>>,
    /// First time on this repository (payload); compare commits: false.
    pub distinct: bool,
}

pub fn valid_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn cut(s: &str, max: usize) -> String {
    s.chars().filter(|c| *c != '\0').take(max).collect()
}

fn time(v: &Json) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn login(v: &Json) -> Option<String> {
    v.as_str()
        .map(|s| cut(s, MAX_LOGIN_CHARS))
        .filter(|s| !s.is_empty())
}

impl PushCommit {
    /// An element of a push payload's `commits[]`; `None` when it has no
    /// usable sha.
    pub fn from_push(v: &Json) -> Option<PushCommit> {
        let sha = v["id"].as_str().filter(|s| valid_sha(s))?;
        Some(PushCommit {
            sha: sha.to_string(),
            message: cut(v["message"].as_str().unwrap_or(""), MAX_MESSAGE_CHARS),
            author_login: login(&v["author"]["username"]),
            committed_at: time(&v["timestamp"]),
            distinct: v["distinct"].as_bool().unwrap_or(true),
        })
    }

    /// A commit of `GET /repos/{repo}/compare/{a}...{b}`.
    pub fn from_api(v: &Json) -> Option<PushCommit> {
        let sha = v["sha"].as_str().filter(|s| valid_sha(s))?;
        Some(PushCommit {
            sha: sha.to_string(),
            message: cut(
                v["commit"]["message"].as_str().unwrap_or(""),
                MAX_MESSAGE_CHARS,
            ),
            author_login: login(&v["author"]["login"]),
            committed_at: time(&v["commit"]["committer"]["date"])
                .or_else(|| time(&v["commit"]["author"]["date"])),
            distinct: false,
        })
    }
}

/// R-C2: the repository object of any event names the default branch.
pub async fn note_default_branch(conn: &mut PgConnection, repo: &Json) -> Result<()> {
    let (Some(gh), Some(branch)) = (repo["id"].as_i64(), repo["default_branch"].as_str()) else {
        return Ok(());
    };
    let branch = cut(branch, MAX_REF_CHARS);
    if branch.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE github_repos SET default_branch = $2, updated_at = now()
         WHERE gh_repo_id = $1 AND default_branch IS DISTINCT FROM $2",
    )
    .bind(gh)
    .bind(branch)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// §6.10: this installation sends `push` events.
async fn note_push(conn: &mut PgConnection, p: &Json) -> Result<()> {
    if let Some(inst) = p["installation"]["id"].as_i64() {
        sqlx::query(
            "UPDATE github_installations SET last_push_at = clock_timestamp() WHERE gh_installation_id = $1",
        )
        .bind(inst)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Stores one commit and, when it is new here (or `distinct`), its refs.
/// `links` are locked `FOR SHARE` by the caller (§19.5).
pub async fn store(
    conn: &mut PgConnection,
    t: &Target,
    c: &PushCommit,
    sender: Option<&str>,
    branch: &str,
    on_default: bool,
) -> Result<()> {
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_commits WHERE repo_id = $1 AND sha = $2)",
    )
    .bind(t.repo)
    .bind(&c.sha)
    .fetch_one(&mut *conn)
    .await?;
    // R-C3 (2): seen before on another branch: no new refs. (3): the
    // default-branch flag is set whatever `distinct` says.
    if known && !c.distinct {
        sqlx::query(
            "UPDATE github_commits SET last_seen = now(), on_default_branch = on_default_branch OR $3
             WHERE repo_id = $1 AND sha = $2",
        )
        .bind(t.repo)
        .bind(&c.sha)
        .bind(on_default)
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    // Never rewrites message, author_login or sender_login (a revoked
    // message stays revoked).
    sqlx::query(
        "INSERT INTO github_commits (repo_id, sha, message, author_login, sender_login, committed_at,
                                     on_default_branch, first_ref)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (repo_id, sha) DO UPDATE SET last_seen = now(),
             on_default_branch = github_commits.on_default_branch OR EXCLUDED.on_default_branch",
    )
    .bind(t.repo)
    .bind(&c.sha)
    .bind(&c.message)
    .bind(&c.author_login)
    .bind(sender.map(|s| cut(s, MAX_LOGIN_CHARS)))
    .bind(c.committed_at)
    .bind(on_default)
    .bind(cut(branch, MAX_REF_CHARS))
    .execute(&mut *conn)
    .await?;
    for key in refs::scan(&c.message, Source::Commit) {
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
        // Only projects that link this repository now.
        let Some((link, _)) = t.links.iter().find(|(_, p)| *p == project) else {
            continue;
        };
        // R-C7: a revoked (project, repo, sha) never gets a ref again.
        sqlx::query(
            "INSERT INTO commit_issue_refs (repo_id, sha, issue_id, project_id, link_id)
             SELECT $1, $2, $3, $4, $5
             WHERE NOT EXISTS (SELECT 1 FROM project_commit_revocations v
                               WHERE v.project_id = $4 AND v.repo_id = $1 AND v.sha = $2)
             ON CONFLICT (repo_id, sha, issue_id) DO UPDATE SET link_id = EXCLUDED.link_id,
                 project_id = EXCLUDED.project_id",
        )
        .bind(t.repo)
        .bind(&c.sha)
        .bind(issue)
        .bind(project)
        .bind(link)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The `push` event (R-C1, R-C3, R-C4).
pub async fn push(conn: &mut PgConnection, api: &dyn GithubApi, p: &Json) -> Result<Outcome> {
    let ignored = || Outcome {
        result: "ignored",
        refetch: Vec::new(),
    };
    note_push(conn, p).await?;
    let gh_repo = p["repository"]["id"]
        .as_i64()
        .context("repository id missing")?;
    note_default_branch(conn, &p["repository"]).await?;
    // Branches only; a deleted branch brings no commits.
    let Some(branch) = p["ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .filter(|b| !b.is_empty())
    else {
        return Ok(ignored());
    };
    if p["deleted"].as_bool() == Some(true) {
        return Ok(ignored());
    }
    // R-C1: installation active, repository accessible, a live link —
    // otherwise nothing is stored.
    let Some(t) = super::jobs::target(conn, gh_repo).await? else {
        return Ok(ignored());
    };
    let default: Option<String> =
        sqlx::query_scalar("SELECT default_branch FROM github_repos WHERE id = $1")
            .bind(t.repo)
            .fetch_one(&mut *conn)
            .await?;
    let on_default = default.as_deref() == Some(branch);
    let sender = p["sender"]["login"].as_str();
    let mut commits: Vec<PushCommit> = p["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(PushCommit::from_push)
        .collect();
    let mut result = "ok";
    // R-C4: a full payload was cut by GitHub: the rest comes from compare.
    if p["commits"].as_array().map_or(0, Vec::len) >= PAYLOAD_COMMIT_LIMIT {
        let (before, after) = (
            p["before"].as_str().unwrap_or(""),
            p["after"].as_str().unwrap_or(""),
        );
        // A new branch has no `before` to compare with.
        let comparable = valid_sha(before) && valid_sha(after) && before.bytes().any(|b| b != b'0');
        if comparable {
            let more = api
                .compare_commits(t.inst, &t.full_name, before, after)
                .await?;
            if more.len() > COMPARE_LIMIT {
                result = "commits_truncated";
            }
            for c in more
                .iter()
                .take(COMPARE_LIMIT)
                .filter_map(PushCommit::from_api)
            {
                if !commits.iter().any(|x| x.sha == c.sha) {
                    commits.push(c);
                }
            }
        } else {
            result = "commits_truncated";
        }
    }
    for c in &commits {
        store(conn, &t, c, sender, branch, on_default).await?;
    }
    // R-F1 (b): fix keywords of every commit pushed to the default branch,
    // whatever `distinct` says. Decision A3 (i): only a hint.
    if on_default {
        let head = p["after"].as_str().filter(|s| valid_sha(s));
        let mut d = super::fixes::Detector::load(conn, &t).await?;
        for c in &commits {
            let found = super::fixes::Found::Push {
                sha: &c.sha,
                head: head.unwrap_or(&c.sha),
            };
            d.record(conn, &t, found, &[&c.message]).await?;
        }
        if d.truncated && result == "ok" {
            result = "fix_links_truncated";
        }
    }
    Ok(Outcome {
        result,
        refetch: Vec::new(),
    })
}

/// Unlinking (§19.5): the link's commit refs go in the same transaction.
pub async fn drop_link_refs(conn: &mut PgConnection, link: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM commit_issue_refs WHERE link_id = $1")
        .bind(link)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// R-C8: commits without a ref, not seen for `days` days, and every commit
/// of `repos` (repositories without a link for 30 days). Revoked rows stay:
/// with the tombstone they keep the original text from coming back.
pub async fn sweep(conn: &mut PgConnection, before: DateTime<Utc>, repos: &[Uuid]) -> Result<u64> {
    let n = sqlx::query(
        "DELETE FROM github_commits c
         WHERE c.redacted_at IS NULL
           AND (c.repo_id = ANY($2)
                OR (c.last_seen < $1
                    AND NOT EXISTS (SELECT 1 FROM commit_issue_refs f
                                    WHERE f.repo_id = c.repo_id AND f.sha = c.sha)))",
    )
    .bind(before)
    .bind(repos)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n)
}
