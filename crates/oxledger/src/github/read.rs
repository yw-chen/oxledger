// SPDX-License-Identifier: GPL-2.0-or-later
//! Reading development data (§19.4 R-G2, R-G3, R-G5, R-G9; §19.7). Every
//! query joins a live link (`unlinked_at IS NULL`) and leaves out PRs the
//! issue's project hid; issues are filtered by the caller's scope, so an
//! invisible issue leaves no trace and nothing is counted.

use super::links::{self, ProjectAccess};
use crate::auth::AuthContext;
use crate::error::{ApiError, ErrorCode};
use crate::service::{self, App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// R-G9: PRs on one issue card.
pub const ISSUE_PR_LIMIT: i64 = 20;
/// R-C6: commits on one issue card, newest first.
pub const ISSUE_COMMIT_LIMIT: i64 = 20;
/// R-C6: characters of a commit's first line that are shown.
pub const COMMIT_TITLE_CHARS: usize = 120;
/// Issues per page of the development tab.
pub const DEV_PAGE: usize = 50;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DevPr {
    pub pr_id: Uuid,
    /// `owner/name` as GitHub shows it now.
    pub repo: String,
    pub number: i32,
    /// GitHub's text as written (R-G3).
    pub title: String,
    pub url: String,
    /// open, draft, merged, closed
    pub state: String,
    /// none, pending, approved, changes_requested
    pub review_state: String,
    /// pending, success, failure; null = no checks on the current head
    pub ci_state: Option<String>,
    pub updated_at: DateTime<Utc>,
    /// false: the App can no longer read the repository; data is as of `synced_at`.
    pub accessible: bool,
    /// When the link's last full sync finished (null: backfill not done).
    pub synced_at: Option<DateTime<Utc>>,
    /// Only with `include_hidden` (managers).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// A commit whose message names the issue (design 06 R-C6). Collected from
/// GitHub's `push` notifications, so the list may be incomplete.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DevCommit {
    pub repo_id: Uuid,
    /// `owner/name` as GitHub shows it now.
    pub repo: String,
    pub sha: String,
    /// Built from the repository's name and the sha (never a URL GitHub sent).
    pub url: String,
    /// The first line of the message, cut to 120 characters; `[已遮蔽]`
    /// after a revoke.
    pub title: String,
    /// The author GitHub names for the commit: NOT verified (anyone can
    /// write any author into a commit).
    pub author_login: Option<String>,
    /// Who pushed it first (verified by GitHub).
    pub sender_login: Option<String>,
    /// The commit's time; when GitHub sent none, when it was first seen.
    pub at: DateTime<Utc>,
    pub on_default_branch: bool,
    /// The repository's default branch, when known.
    pub default_branch: Option<String>,
    /// Only for managers (a person's session).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IssueDev {
    pub key: String,
    pub prs: Vec<DevPr>,
    /// Newest first, at most 20.
    pub commits: Vec<DevCommit>,
    /// More than 20 commits name this issue.
    pub commits_truncated: bool,
    /// The caller may hide commits and PRs of this issue's project (and is
    /// shown hidden commits). Always false for agents.
    pub can_manage: bool,
    /// More than 20 PRs: the rest are on GitHub.
    pub truncated: bool,
    /// R-G5: a PR is merged while the issue is not closed (a hint; nothing
    /// changes by itself).
    pub merged_open: bool,
    /// The oldest `synced_at` of the PRs shown.
    pub synced_at: Option<DateTime<Utc>>,
    /// Fix keywords found for this issue in merged PRs and pushed commits
    /// (design 06 R-F6): what moved it, and what only hints.
    pub fix_links: Vec<super::fixes::DevFix>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DevIssue {
    pub key: String,
    pub subject: String,
    pub status: String,
    pub closed: bool,
    pub merged_open: bool,
    pub prs: Vec<DevPr>,
    pub truncated: bool,
    /// As in the issue's own card.
    pub fix_links: Vec<super::fixes::DevFix>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectDev {
    pub items: Vec<DevIssue>,
    pub next_cursor: Option<String>,
    /// Whether the caller may hide PRs here (and ask for `include_hidden`).
    pub can_manage: bool,
}

type PrRow = (
    Uuid,
    String,
    i32,
    String,
    String,
    String,
    String,
    Option<String>,
    DateTime<Utc>,
    String,
    Option<DateTime<Utc>>,
    bool,
);

/// PRs of one issue through live links of its own project.
async fn prs_of(
    conn: &mut PgConnection,
    issue: Uuid,
    include_hidden: bool,
    limit: i64,
) -> Result<Vec<DevPr>> {
    let rows: Vec<PrRow> = sqlx::query_as(
        "SELECT p.id, r.full_name, p.number, p.title, p.html_url, p.state, p.review_state, p.ci_state,
                p.gh_updated_at, r.state, l.synced_through,
                EXISTS (SELECT 1 FROM project_pr_hidden h WHERE h.project_id = f.project_id AND h.pr_id = p.id)
         FROM pr_issue_refs f
         JOIN project_repo_links l ON l.id = f.link_id AND l.unlinked_at IS NULL AND l.project_id = f.project_id
         JOIN github_pull_requests p ON p.id = f.pr_id
         JOIN github_repos r ON r.id = p.repo_id
         WHERE f.issue_id = $1
           AND ($2 OR NOT EXISTS (SELECT 1 FROM project_pr_hidden h
                                  WHERE h.project_id = f.project_id AND h.pr_id = p.id))
         ORDER BY CASE p.state WHEN 'open' THEN 0 WHEN 'draft' THEN 1 ELSE 2 END,
                  p.gh_updated_at DESC, p.id
         LIMIT $3",
    )
    .bind(issue)
    .bind(include_hidden)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DevPr {
            pr_id: r.0,
            repo: r.1,
            number: r.2,
            title: r.3,
            url: r.4,
            state: r.5,
            review_state: r.6,
            ci_state: r.7,
            updated_at: r.8,
            accessible: r.9 == "accessible",
            synced_at: r.10,
            hidden: r.11,
        })
        .collect())
}

type CommitRow = (
    Uuid,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
    bool,
    Option<String>,
    bool,
);

/// Commits of one issue through live links of its own project (R-G3).
async fn commits_of(
    conn: &mut PgConnection,
    issue: Uuid,
    include_hidden: bool,
    limit: i64,
) -> Result<Vec<DevCommit>> {
    let rows: Vec<CommitRow> = sqlx::query_as(
        "SELECT c.repo_id, r.full_name, c.sha, c.message, c.author_login, c.sender_login,
                coalesce(c.committed_at, c.first_seen) AS at, c.on_default_branch, r.default_branch,
                EXISTS (SELECT 1 FROM project_commit_hidden h
                        WHERE h.project_id = f.project_id AND h.repo_id = c.repo_id AND h.sha = c.sha)
         FROM commit_issue_refs f
         JOIN project_repo_links l ON l.id = f.link_id AND l.unlinked_at IS NULL AND l.project_id = f.project_id
         JOIN github_commits c ON c.repo_id = f.repo_id AND c.sha = f.sha
         JOIN github_repos r ON r.id = c.repo_id
         WHERE f.issue_id = $1
           AND ($2 OR NOT EXISTS (SELECT 1 FROM project_commit_hidden h
                                  WHERE h.project_id = f.project_id AND h.repo_id = c.repo_id AND h.sha = c.sha))
         ORDER BY c.first_seen DESC, at DESC, c.sha
         LIMIT $3",
    )
    .bind(issue)
    .bind(include_hidden)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DevCommit {
            url: format!("https://github.com/{}/commit/{}", r.1, r.2),
            title: r
                .3
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(COMMIT_TITLE_CHARS)
                .collect(),
            repo_id: r.0,
            repo: r.1,
            sha: r.2,
            author_login: r.4,
            sender_login: r.5,
            at: r.6,
            on_default_branch: r.7,
            default_branch: r.8,
            hidden: r.9,
        })
        .collect())
}

fn card(mut prs: Vec<DevPr>) -> (Vec<DevPr>, bool) {
    let truncated = prs.len() as i64 > ISSUE_PR_LIMIT;
    prs.truncate(ISSUE_PR_LIMIT as usize);
    (prs, truncated)
}

/// The development block of an issue the caller has already been shown
/// (`get` with `include: ["dev"]`, `GET /issues/{key}/dev`). `manager`:
/// the caller may hide things in the issue's project, so hidden commits
/// are listed (marked) — never for agents.
pub async fn issue_block(
    conn: &mut PgConnection,
    issue: Uuid,
    key: &str,
    closed: bool,
    manager: bool,
) -> Result<IssueDev> {
    let (prs, truncated) = card(prs_of(conn, issue, false, ISSUE_PR_LIMIT + 1).await?);
    let mut commits = commits_of(conn, issue, manager, ISSUE_COMMIT_LIMIT + 1).await?;
    let commits_truncated = commits.len() as i64 > ISSUE_COMMIT_LIMIT;
    commits.truncate(ISSUE_COMMIT_LIMIT as usize);
    let synced_at = if prs.iter().any(|p| p.synced_at.is_none()) {
        None
    } else {
        prs.iter().filter_map(|p| p.synced_at).min()
    };
    Ok(IssueDev {
        key: key.to_string(),
        merged_open: !closed && prs.iter().any(|p| p.state == "merged"),
        prs,
        commits,
        commits_truncated,
        can_manage: manager,
        truncated,
        synced_at,
        fix_links: super::fixes::fixes_of(conn, issue).await?,
    })
}

/// `GET /issues/{key}/dev`: `not_found` like the issue itself when it is
/// not visible (T-G9).
pub async fn issue_dev(app: &App, caller: AuthContext, key: &str) -> Result<IssueDev> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let (id, d) = service::get_in(&mut tx, caller, key).await?;
    let project: Uuid = sqlx::query_scalar("SELECT project_id FROM issues WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let manager = links::access_of(&mut tx, caller, project)
        .await
        .is_ok_and(|a| a.can_clean);
    let v = issue_block(&mut tx, id, &d.key, d.status_category == "done", manager).await?;
    tx.commit().await?;
    Ok(v)
}

/// `filter` of the development tab (B18).
fn filter_sql(filter: &str) -> Result<&'static str> {
    Ok(match filter {
        "all" => "true",
        "ci_failure" => "p.ci_state = 'failure' AND p.state IN ('open', 'draft')",
        "review" => {
            "p.review_state IN ('pending', 'changes_requested') AND p.state IN ('open', 'draft')"
        }
        "merged_open" => "p.state = 'merged' AND s.category <> 'done'",
        // Design 06 R-F9: a PR said `fixes` and nothing moved by itself.
        "fix_skipped" => {
            "EXISTS (SELECT 1 FROM issue_fix_links x
                     WHERE x.issue_id = i.id AND x.pr_id = p.id AND x.state IN ('suggested', 'skipped')
                       AND x.reason IS DISTINCT FROM 'already_in_target')"
        }
        _ => {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                "filter is all, ci_failure, review, merged_open or fix_skipped",
            )
            .with_details(json!({"path": "filter",
                                 "allowed": ["all", "ci_failure", "review", "merged_open", "fix_skipped"]}))
            .into());
        }
    })
}

fn bad_cursor() -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidCursor, "not a development-tab cursor").into()
}

/// `GET /projects/{id}/dev`: issues of the project that have PRs matching
/// `filter`, newest PR activity first, only issues the caller can see,
/// never a total (Q-S6). Hidden PRs only for managers who ask (B23).
pub async fn project_dev(
    app: &App,
    caller: AuthContext,
    project: &str,
    filter: &str,
    include_hidden: bool,
    cursor: Option<&str>,
) -> Result<ProjectDev> {
    caller.require_human_session()?;
    let cond = filter_sql(filter)?;
    let after: Option<(DateTime<Utc>, Uuid)> = cursor
        .map(|c| {
            let b = crate::auth::decode_bytes(c).ok_or_else(bad_cursor)?;
            let v: serde_json::Value = serde_json::from_slice(&b).map_err(|_| bad_cursor())?;
            let at = v["at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .ok_or_else(bad_cursor)?
                .with_timezone(&Utc);
            let id = v["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(bad_cursor)?;
            Ok::<_, anyhow::Error>((at, id))
        })
        .transpose()?;
    let mut tx = read_tx(app).await?;
    let a: ProjectAccess = links::project_access(&mut tx, caller, project).await?;
    if include_hidden && !a.can_clean {
        return Err(links::forbidden(
            "hidden PRs are shown to project managers only",
        ));
    }
    type Row = (
        Uuid,
        String,
        String,
        String,
        bool,
        DateTime<Utc>,
        Uuid,
        bool,
        Uuid,
        Option<Uuid>,
    );
    let mut items = Vec::new();
    let mut pos = after;
    let mut more = false;
    'outer: loop {
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT i.id, k.key, i.subject, s.name, s.category = 'done', max(p.gh_updated_at) AS latest,
                    i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
             FROM pr_issue_refs f
             JOIN project_repo_links l ON l.id = f.link_id AND l.unlinked_at IS NULL AND l.project_id = f.project_id
             JOIN github_pull_requests p ON p.id = f.pr_id
             JOIN issues i ON i.id = f.issue_id AND i.archived_at IS NULL
             JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
             JOIN issue_statuses s ON s.id = i.status_id
             WHERE f.project_id = $1 AND ({cond})
               AND ($2 OR NOT EXISTS (SELECT 1 FROM project_pr_hidden h
                                      WHERE h.project_id = f.project_id AND h.pr_id = p.id))
             GROUP BY i.id, k.key, s.name, s.category
             HAVING $3::timestamptz IS NULL OR (max(p.gh_updated_at), i.id) < ($3, $4)
             ORDER BY latest DESC, i.id DESC
             LIMIT $5"
        )))
        .bind(a.project)
        .bind(include_hidden)
        .bind(pos.map(|p| p.0))
        .bind(pos.map(|p| p.1))
        .bind((DEV_PAGE * 2) as i64)
        .fetch_all(&mut *tx)
        .await?;
        let exhausted = rows.len() < DEV_PAGE * 2;
        for r in rows {
            pos = Some((r.5, r.0));
            let facts = IssueFacts {
                project_id: a.project,
                tracker_id: r.6,
                is_private: r.7,
                author_id: r.8,
                assigned_to_id: r.9,
            };
            if !a.scope.can_see(&facts) {
                continue;
            }
            if items.len() == DEV_PAGE {
                more = true;
                break 'outer;
            }
            let (prs, truncated) =
                card(prs_of(&mut tx, r.0, include_hidden, ISSUE_PR_LIMIT + 1).await?);
            items.push((
                (r.5, r.0),
                DevIssue {
                    key: r.1,
                    subject: r.2,
                    status: r.3,
                    closed: r.4,
                    merged_open: !r.4 && prs.iter().any(|p| p.state == "merged"),
                    prs,
                    truncated,
                    fix_links: super::fixes::fixes_of(&mut tx, r.0).await?,
                },
            ));
        }
        if exhausted {
            break;
        }
    }
    tx.commit().await?;
    let next_cursor = more.then(|| {
        let (at, id) = items.last().unwrap().0;
        crate::auth::encode(
            json!({"at": at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true), "id": id})
                .to_string()
                .as_bytes(),
        )
    });
    Ok(ProjectDev {
        items: items.into_iter().map(|x| x.1).collect(),
        next_cursor,
        can_manage: a.can_clean,
    })
}
