// SPDX-License-Identifier: GPL-2.0-or-later
//! Project ↔ repository links, hiding PRs, revoking GitHub content and the
//! administrator's settings (§19.6, §19.7, §19.9). People only: agents can
//! neither link nor hide (they have no human session).
//!
//! Lock order of a new link (§19.7): `operation_requests` → principal and
//! session `FOR SHARE` → project `FOR SHARE` → `project_tab_settings`
//! `FOR UPDATE` (the project settings lock, so two links of one project
//! are serial, T-G27) → the repository `FOR SHARE`. Unlinking takes the
//! link `FOR UPDATE`; the worker holds it `FOR SHARE` while it builds refs
//! (§19.5), so one waits for the other (T-G17).

use crate::auth::{self, AuthContext, SessionPolicy};
use crate::commands::{self, Executor};
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, MANAGE_PROJECT, VIEW_ISSUES};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use ox_core::visibility::Scope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// R-G9.
pub const MAX_LINKS: i64 = 20;
pub const MAX_REVOKE_PRS: usize = 100;

pub const ACTION_LINK: &str = "github_repo_link";
pub const ACTION_UNLINK: &str = "github_repo_unlink";
pub const ACTION_HIDE: &str = "github_pr_hide";
pub const ACTION_UNHIDE: &str = "github_pr_unhide";
pub const ACTION_REVOKE: &str = "github_content_revoke";
pub const ACTION_COMMIT_HIDE: &str = "github_commit_hide";
pub const ACTION_COMMIT_UNHIDE: &str = "github_commit_unhide";
pub const MAX_REVOKE_COMMITS: usize = 100;

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RepoLink {
    pub link_id: Uuid,
    pub repo_id: Uuid,
    pub full_name: String,
    pub is_private: bool,
    /// accessible / inaccessible (the App lost access; data is as of `synced_at`).
    pub repo_state: String,
    /// backfilling / ok / paused
    pub sync_state: String,
    /// The last full sync (the watermark); null until the backfill finished.
    pub synced_at: Option<DateTime<Utc>>,
    /// While paused by GitHub's rate limit: when it resumes.
    pub paused_until: Option<DateTime<Utc>>,
    pub linked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RepoLinks {
    pub items: Vec<RepoLink>,
    pub can_manage: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LinkableRepo {
    pub repo_id: Uuid,
    pub full_name: String,
    pub is_private: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LinkableRepos {
    pub items: Vec<LinkableRepo>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkInput {
    pub repo_id: Uuid,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Unlinked {
    pub link_id: Uuid,
    /// false: it was unlinked already (nothing changed).
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HideResult {
    pub pr_id: Uuid,
    pub hidden: bool,
    /// false: it was in that state already (no event).
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommitHideResult {
    pub repo_id: Uuid,
    pub sha: String,
    pub hidden: bool,
    /// false: it was in that state already (no event).
    pub changed: bool,
}

/// A commit of a repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitRef {
    pub repo_id: Uuid,
    /// The full 40-character sha.
    pub sha: String,
}

/// Exactly one of `link_id`, `pr_ids`, `commits`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeInput {
    /// Revoke a whole link (also for PRs on issues you cannot see).
    pub link_id: Option<Uuid>,
    /// Or these PRs (each must be on an issue of this project you can see).
    pub pr_ids: Option<Vec<Uuid>>,
    /// Or these commits (same rule): their links to this project's issues
    /// go and never come back, even when the same commit is pushed again;
    /// a commit no project links any more loses its message.
    pub commits: Option<Vec<CommitRef>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Revoked {
    pub security_epoch: i64,
    pub link_id: Option<Uuid>,
    pub pr_ids: Vec<Uuid>,
    pub commits: Vec<CommitRef>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AdminRepo {
    pub repo_id: Uuid,
    pub gh_repo_id: i64,
    pub full_name: String,
    pub is_private: bool,
    pub linkable: bool,
    pub state: String,
    /// Live links to projects.
    pub links: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AdminInstallation {
    pub id: Uuid,
    pub gh_installation_id: i64,
    pub account_login: String,
    /// active / suspended / deleted
    pub state: String,
    /// When this installation last sent a `push` event; null = never (the
    /// App is not subscribed to push: no commits, no fix keywords).
    pub last_push_at: Option<DateTime<Utc>>,
    pub repos: Vec<AdminRepo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AdminGithub {
    pub installations: Vec<AdminInstallation>,
    /// Repositories the App no longer reaches through any installation.
    pub detached_repos: Vec<AdminRepo>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminRepoSetting {
    pub repo_id: Uuid,
    pub linkable: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminGithubPut {
    pub repos: Vec<AdminRepoSetting>,
}

// ---------------------------------------------------------------- access

pub(crate) fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such project")
}

fn commit_not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such commit in this project")
}

fn pr_not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such pull request in this project")
}

pub(crate) fn forbidden(msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": "manage_project"}))
        .into()
}

fn not_linkable() -> anyhow::Error {
    ApiError::new(
        ErrorCode::RepoNotLinkable,
        "this repository cannot be linked: ask an administrator to open it for linking",
    )
    .into()
}

pub(crate) struct ProjectAccess {
    pub project: Uuid,
    /// Active, with `manage_project`: may link.
    pub can_manage: bool,
    /// Not archived, with `manage_project` or an administrator: may
    /// unlink, hide and revoke even when the repository is gone (T-G23).
    pub can_clean: bool,
    pub is_admin: bool,
    pub scope: Scope,
}

/// Visible with `view_issues` and not archived; otherwise `not_found`.
pub(crate) async fn project_access(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
) -> Result<ProjectAccess> {
    let project = describe::visible_project(&mut *conn, caller, project)
        .await
        .map_err(|_| not_found())?;
    access_of(conn, caller, project).await
}

pub(crate) async fn access_of(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<ProjectAccess> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(not_found)?;
    if pp.status == "archived" || !pp.allowed_any(VIEW_ISSUES) {
        return Err(not_found());
    }
    let me = db::effective(&mut *conn, caller).await?;
    let is_admin = me.is_admin && me.agent.is_none();
    let manages = pp.allowed_any(MANAGE_PROJECT);
    Ok(ProjectAccess {
        project,
        can_manage: pp.status == "active" && manages,
        can_clean: manages || is_admin,
        is_admin,
        scope,
    })
}

fn hash(v: &Json) -> [u8; 32] {
    Sha256::digest([b"oxledger/github/v1\n".as_slice(), v.to_string().as_bytes()].concat()).into()
}

/// The operation request, or the first result when this key ran before.
pub(super) async fn open(
    conn: &mut PgConnection,
    who: &Executor,
    operation_id: Uuid,
    action: &str,
    body: &Json,
) -> Result<std::result::Result<Uuid, Json>> {
    let fp = commands::fingerprint(action, &hash(body));
    match commands::open_request(&mut *conn, who.user_id, operation_id, action, &fp).await? {
        Some(id) => Ok(Ok(id)),
        None => Ok(Err(commands::stored_result(
            &mut *conn,
            who.user_id,
            operation_id,
            action,
            &fp,
        )
        .await?)),
    }
}

pub(super) async fn event(
    conn: &mut PgConnection,
    project: Uuid,
    kind: &str,
    who: Uuid,
    request: Uuid,
    payload: Json,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(project)
    .bind(kind)
    .bind(who)
    .bind(request)
    .bind(payload)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn link_name(conn: &mut PgConnection, link: Uuid) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT r.full_name FROM project_repo_links l JOIN github_repos r ON r.id = l.repo_id WHERE l.id = $1",
    )
    .bind(link)
    .fetch_one(&mut *conn)
    .await?)
}

// ---------------------------------------------------------------- reading links

type LinkRow = (
    Uuid,
    Uuid,
    String,
    bool,
    String,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

async fn link_rows(
    conn: &mut PgConnection,
    project: Uuid,
    only: Option<Uuid>,
) -> Result<Vec<RepoLink>> {
    let rows: Vec<LinkRow> = sqlx::query_as(
        "SELECT l.id, r.id, r.full_name, r.is_private, r.state, l.sync_state, l.synced_through,
                l.paused_until, l.linked_at
         FROM project_repo_links l JOIN github_repos r ON r.id = l.repo_id
         WHERE l.project_id = $1 AND l.unlinked_at IS NULL AND ($2::uuid IS NULL OR l.id = $2)
         ORDER BY r.full_name, l.id",
    )
    .bind(project)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RepoLink {
            link_id: r.0,
            repo_id: r.1,
            full_name: r.2,
            is_private: r.3,
            repo_state: r.4,
            sync_state: r.5,
            synced_at: r.6,
            paused_until: r.7,
            linked_at: r.8,
        })
        .collect())
}

/// `GET /projects/{id}/repos`.
pub async fn list(app: &App, caller: AuthContext, project: &str) -> Result<RepoLinks> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let a = project_access(&mut tx, caller, project).await?;
    let items = link_rows(&mut tx, a.project, None).await?;
    tx.commit().await?;
    Ok(RepoLinks {
        items,
        can_manage: a.can_manage,
    })
}

/// `GET /projects/{id}/repos/linkable`: opened by an administrator,
/// reachable by the App, not linked here yet.
pub async fn linkable(app: &App, caller: AuthContext, project: &str) -> Result<LinkableRepos> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let a = project_access(&mut tx, caller, project).await?;
    if !a.can_manage {
        return Err(forbidden(
            "linking repositories needs manage_project in an active project",
        ));
    }
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT r.id, r.full_name, r.is_private FROM github_repos r
         JOIN github_installations i ON i.id = r.installation_id AND i.state = 'active'
         WHERE r.linkable AND r.state = 'accessible'
           AND NOT EXISTS (SELECT 1 FROM project_repo_links l
                           WHERE l.repo_id = r.id AND l.project_id = $1 AND l.unlinked_at IS NULL)
         ORDER BY r.full_name",
    )
    .bind(a.project)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(LinkableRepos {
        items: rows
            .into_iter()
            .map(|(repo_id, full_name, is_private)| LinkableRepo {
                repo_id,
                full_name,
                is_private,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------- link / unlink

/// `POST /projects/{id}/repos` (Idempotency-Key).
pub async fn link(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &LinkInput,
) -> Result<RepoLink> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let body = json!({"project": project, "repo_id": input.repo_id});
    let request = match open(&mut tx, &who, operation_id, ACTION_LINK, &body).await? {
        Ok(r) => r,
        Err(first) => {
            let id = first["link_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok());
            let row = sqlx::query_as::<_, LinkRow>(
                "SELECT l.id, r.id, r.full_name, r.is_private, r.state, l.sync_state, l.synced_through,
                        l.paused_until, l.linked_at
                 FROM project_repo_links l JOIN github_repos r ON r.id = l.repo_id WHERE l.id = $1",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(RepoLink {
                link_id: row.0,
                repo_id: row.1,
                full_name: row.2,
                is_private: row.3,
                repo_state: row.4,
                sync_state: row.5,
                synced_at: row.6,
                paused_until: row.7,
                linked_at: row.8,
            });
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, project).await?;
    if !a.can_manage {
        return Err(forbidden(
            "linking repositories needs manage_project in an active project",
        ));
    }
    sqlx::query("SELECT 1 FROM project_tab_settings WHERE project_id = $1 FOR UPDATE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    super::fault("github.link.after_settings_lock").await?;
    let ok: Option<bool> = sqlx::query_scalar(
        "SELECT r.linkable AND r.state = 'accessible' AND coalesce(i.state = 'active', false)
         FROM github_repos r LEFT JOIN github_installations i ON i.id = r.installation_id
         WHERE r.id = $1 FOR SHARE OF r",
    )
    .bind(input.repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    if ok != Some(true) {
        return Err(not_linkable());
    }
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT repo_id FROM project_repo_links WHERE project_id = $1 AND unlinked_at IS NULL",
    )
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    if live.contains(&input.repo_id) {
        return Err(api(
            ErrorCode::RepoAlreadyLinked,
            "this project links the repository already",
        ));
    }
    if live.len() as i64 >= MAX_LINKS {
        return Err(ApiError::new(
            ErrorCode::RepoLinkLimit,
            format!("a project links at most {MAX_LINKS} repositories"),
        )
        .with_details(json!({"limit": MAX_LINKS}))
        .into());
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO project_repo_links (project_id, repo_id, linked_by) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(project)
    .bind(input.repo_id)
    .bind(who.user_id)
    .fetch_one(&mut *tx)
    .await?;
    let full_name = link_name(&mut tx, id).await?;
    event(
        &mut tx,
        project,
        "repo_linked",
        who.user_id,
        request,
        json!({"link_id": id, "repo_id": input.repo_id, "full_name": full_name}),
    )
    .await?;
    commands::complete_request(&mut tx, request, &json!({"link_id": id})).await?;
    let link = link_rows(&mut tx, project, Some(id)).await?.remove(0);
    tx.commit().await?;
    Ok(link)
}

/// Sets `unlinked_at` and drops that link's refs, under the link's
/// `FOR UPDATE`. `false` when it was unlinked already.
async fn unlink_locked(
    conn: &mut PgConnection,
    project: Uuid,
    link: Uuid,
    who: Uuid,
) -> Result<bool> {
    let row: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT unlinked_at FROM project_repo_links WHERE id = $1 AND project_id = $2 FOR UPDATE",
    )
    .bind(link)
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(unlinked) = row else {
        return Err(api(
            ErrorCode::NotFound,
            "no such repository link in this project",
        ));
    };
    super::fault("github.unlink.after_lock").await?;
    if unlinked.is_some() {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE project_repo_links SET unlinked_at = now(), unlinked_by = $2 WHERE id = $1",
    )
    .bind(link)
    .bind(who)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM pr_issue_refs WHERE link_id = $1")
        .bind(link)
        .execute(&mut *conn)
        .await?;
    super::commits::drop_link_refs(conn, link).await?;
    super::fixes::drop_link(conn, link).await?;
    Ok(true)
}

/// `DELETE /projects/{id}/repos/{link_id}` (Idempotency-Key). Works when
/// the repository is no longer reachable or linkable (T-G23).
pub async fn unlink(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    link: Uuid,
) -> Result<Unlinked> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let body = json!({"project": project, "link_id": link});
    let request = match open(&mut tx, &who, operation_id, ACTION_UNLINK, &body).await? {
        Ok(r) => r,
        Err(first) => {
            tx.commit().await?;
            return Ok(Unlinked {
                link_id: link,
                changed: first["changed"].as_bool().unwrap_or(false),
            });
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, project).await?;
    if !a.can_clean {
        return Err(forbidden("unlinking needs manage_project"));
    }
    let changed = unlink_locked(&mut tx, project, link, who.user_id).await?;
    if changed {
        let repo: Uuid = sqlx::query_scalar("SELECT repo_id FROM project_repo_links WHERE id = $1")
            .bind(link)
            .fetch_one(&mut *tx)
            .await?;
        let full_name = link_name(&mut tx, link).await?;
        event(
            &mut tx,
            project,
            "repo_unlinked",
            who.user_id,
            request,
            json!({"link_id": link, "repo_id": repo, "full_name": full_name}),
        )
        .await?;
    }
    commands::complete_request(
        &mut tx,
        request,
        &json!({"link_id": link, "changed": changed}),
    )
    .await?;
    tx.commit().await?;
    Ok(Unlinked {
        link_id: link,
        changed,
    })
}

// ---------------------------------------------------------------- hide / revoke

/// The PR has a ref, through a live link of `project`, to an issue of
/// `project` the caller can see (§19.7); otherwise it does not exist for
/// them (T-G29).
async fn visible_in(
    conn: &mut PgConnection,
    scope: &Scope,
    project: Uuid,
    pr: Uuid,
) -> Result<bool> {
    let rows: Vec<(Uuid, bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM (SELECT issue_id, link_id FROM pr_issue_refs WHERE pr_id = $1 AND project_id = $2
               UNION
               -- A fix keyword in the PR's body leaves a fix link and no ref.
               SELECT issue_id, link_id FROM issue_fix_links WHERE pr_id = $1 AND project_id = $2) f
         JOIN project_repo_links l ON l.id = f.link_id AND l.unlinked_at IS NULL AND l.project_id = $2
         JOIN issues i ON i.id = f.issue_id AND i.archived_at IS NULL",
    )
    .bind(pr)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().any(|r| {
        scope.can_see(&IssueFacts {
            project_id: project,
            tracker_id: r.0,
            is_private: r.1,
            author_id: r.2,
            assigned_to_id: r.3,
        })
    }))
}

/// Inserts or deletes the hidden row; whether anything changed.
async fn set_hidden(
    conn: &mut PgConnection,
    project: Uuid,
    pr: Uuid,
    hidden: bool,
    who: Uuid,
) -> Result<bool> {
    let n = if hidden {
        sqlx::query(
            "INSERT INTO project_pr_hidden (project_id, pr_id, hidden_by) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(project)
        .bind(pr)
        .bind(who)
        .execute(&mut *conn)
        .await?
    } else {
        sqlx::query("DELETE FROM project_pr_hidden WHERE project_id = $1 AND pr_id = $2")
            .bind(project)
            .bind(pr)
            .execute(&mut *conn)
            .await?
    };
    Ok(n.rows_affected() > 0)
}

/// `POST` / `DELETE /projects/{id}/dev/prs/{pr_id}/hide` (Idempotency-Key).
/// Hiding is not a D12 redaction (§19.9): agents that read it stay valid.
pub async fn hide(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    pr: Uuid,
    hidden: bool,
) -> Result<HideResult> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let action = if hidden { ACTION_HIDE } else { ACTION_UNHIDE };
    let body = json!({"project": project, "pr_id": pr});
    let request = match open(&mut tx, &who, operation_id, action, &body).await? {
        Ok(r) => r,
        Err(first) => {
            tx.commit().await?;
            return Ok(HideResult {
                pr_id: pr,
                hidden,
                changed: first["changed"].as_bool().unwrap_or(false),
            });
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, project).await?;
    if !a.can_clean {
        return Err(forbidden("hiding pull requests needs manage_project"));
    }
    if !visible_in(&mut tx, &a.scope, project, pr).await? {
        return Err(pr_not_found());
    }
    let changed = set_hidden(&mut tx, project, pr, hidden, who.user_id).await?;
    if changed {
        let kind = if hidden { "pr_hidden" } else { "pr_unhidden" };
        let (repo, number): (Uuid, i32) =
            sqlx::query_as("SELECT repo_id, number FROM github_pull_requests WHERE id = $1")
                .bind(pr)
                .fetch_one(&mut *tx)
                .await?;
        event(
            &mut tx,
            project,
            kind,
            who.user_id,
            request,
            json!({"pr_id": pr, "repo_id": repo, "number": number}),
        )
        .await?;
    }
    commands::complete_request(&mut tx, request, &json!({"pr_id": pr, "changed": changed})).await?;
    tx.commit().await?;
    Ok(HideResult {
        pr_id: pr,
        hidden,
        changed,
    })
}

/// The commit has a ref, through a live link of `project`, to an issue of
/// `project` the caller can see (`scope`; `None` = an administrator: any
/// ref of the project). Otherwise it does not exist for them (T-M2-66).
async fn commit_visible_in(
    conn: &mut PgConnection,
    scope: Option<&Scope>,
    project: Uuid,
    c: &CommitRef,
) -> Result<bool> {
    let rows: Vec<(Uuid, bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM commit_issue_refs f
         JOIN project_repo_links l ON l.id = f.link_id AND l.unlinked_at IS NULL AND l.project_id = $3
         JOIN issues i ON i.id = f.issue_id AND i.archived_at IS NULL
         WHERE f.repo_id = $1 AND f.sha = $2 AND f.project_id = $3",
    )
    .bind(c.repo_id)
    .bind(&c.sha)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(match scope {
        None => !rows.is_empty(),
        Some(scope) => rows.into_iter().any(|r| {
            scope.can_see(&IssueFacts {
                project_id: project,
                tracker_id: r.0,
                is_private: r.1,
                author_id: r.2,
                assigned_to_id: r.3,
            })
        }),
    })
}

/// `POST` / `DELETE /projects/{id}/dev/commits/{repo_id}/{sha}/hide`
/// (Idempotency-Key; design 06 R-C7). As hiding a PR: this project only,
/// not a redaction.
pub async fn hide_commit(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    c: &CommitRef,
    hidden: bool,
) -> Result<CommitHideResult> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let out = |changed| CommitHideResult {
        repo_id: c.repo_id,
        sha: c.sha.clone(),
        hidden,
        changed,
    };
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let action = if hidden {
        ACTION_COMMIT_HIDE
    } else {
        ACTION_COMMIT_UNHIDE
    };
    let body = json!({"project": project, "repo_id": c.repo_id, "sha": c.sha});
    let request = match open(&mut tx, &who, operation_id, action, &body).await? {
        Ok(r) => r,
        Err(first) => {
            tx.commit().await?;
            return Ok(out(first["changed"].as_bool().unwrap_or(false)));
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, project).await?;
    if !a.can_clean {
        return Err(forbidden("hiding commits needs manage_project"));
    }
    if !super::commits::valid_sha(&c.sha)
        || !commit_visible_in(&mut tx, Some(&a.scope), project, c).await?
    {
        return Err(commit_not_found());
    }
    let n = if hidden {
        sqlx::query(
            "INSERT INTO project_commit_hidden (project_id, repo_id, sha, hidden_by) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(project)
        .bind(c.repo_id)
        .bind(&c.sha)
        .bind(who.user_id)
        .execute(&mut *tx)
        .await?
    } else {
        sqlx::query(
            "DELETE FROM project_commit_hidden WHERE project_id = $1 AND repo_id = $2 AND sha = $3",
        )
        .bind(project)
        .bind(c.repo_id)
        .bind(&c.sha)
        .execute(&mut *tx)
        .await?
    };
    let changed = n.rows_affected() > 0;
    if changed {
        let kind = if hidden {
            "commit_hidden"
        } else {
            "commit_unhidden"
        };
        event(
            &mut tx,
            project,
            kind,
            who.user_id,
            request,
            json!({"repo_id": c.repo_id, "sha": c.sha}),
        )
        .await?;
    }
    commands::complete_request(&mut tx, request, &json!({"changed": changed})).await?;
    tx.commit().await?;
    Ok(out(changed))
}

/// R-C7, inside the revoke transaction (project `FOR UPDATE`, epoch
/// bumped): the tombstone, then this project's refs, then the message of a
/// commit nothing links any more.
async fn revoke_commit(
    conn: &mut PgConnection,
    project: Uuid,
    c: &CommitRef,
    who: Uuid,
) -> Result<()> {
    // The worker builds refs under the link's FOR SHARE (§19.5): with the
    // link FOR UPDATE here, a push of this sha either finished before (its
    // refs are deleted below) or starts after and sees the tombstone.
    sqlx::query(
        "SELECT 1 FROM project_repo_links
         WHERE project_id = $1 AND repo_id = $2 AND unlinked_at IS NULL FOR UPDATE",
    )
    .bind(project)
    .bind(c.repo_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO project_commit_revocations (project_id, repo_id, sha, revoked_by)
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(project)
    .bind(c.repo_id)
    .bind(&c.sha)
    .bind(who)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM commit_issue_refs WHERE project_id = $1 AND repo_id = $2 AND sha = $3",
    )
    .bind(project)
    .bind(c.repo_id)
    .bind(&c.sha)
    .execute(&mut *conn)
    .await?;
    super::fixes::drop_commit(conn, project, c.repo_id, &c.sha).await?;
    sqlx::query(
        "UPDATE github_commits c SET message = $3, redacted_at = now()
         WHERE c.repo_id = $1 AND c.sha = $2 AND c.redacted_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM commit_issue_refs f WHERE f.repo_id = c.repo_id AND f.sha = c.sha)",
    )
    .bind(c.repo_id)
    .bind(&c.sha)
    .bind(super::commits::REDACTED)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `POST /projects/{id}/dev/revoke` (Idempotency-Key, step-up): §19.9.
/// One transaction: project `FOR UPDATE` and `security_epoch + 1` (every
/// agent context that read this project is quarantined) → unlink or hide
/// → the derived copies of §7.5 (proposal contents, `execution_field_changes`;
/// S7, B14) → `github_content_revoked` (ids only, never titles).
pub async fn revoke(
    app: &App,
    caller: AuthContext,
    policy: &SessionPolicy,
    operation_id: Uuid,
    project: &str,
    input: &RevokeInput,
) -> Result<Revoked> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let (pr_ids, commits) = match (&input.link_id, &input.pr_ids, &input.commits) {
        (Some(_), None, None) => (Vec::new(), Vec::new()),
        (None, Some(p), None) if !p.is_empty() && p.len() <= MAX_REVOKE_PRS => {
            let mut p = p.clone();
            p.sort();
            p.dedup();
            (p, Vec::new())
        }
        (None, None, Some(c))
            if !c.is_empty()
                && c.len() <= MAX_REVOKE_COMMITS
                && c.iter().all(|x| super::commits::valid_sha(&x.sha)) =>
        {
            let mut c = c.clone();
            c.sort();
            c.dedup();
            (Vec::new(), c)
        }
        _ => {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "send one of link_id, 1–{MAX_REVOKE_PRS} pr_ids or 1–{MAX_REVOKE_COMMITS} commits (full shas)"
                ),
            )
            .with_details(json!({"kind": "revoke_target"}))
            .into());
        }
    };
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    // Requests without commits hash as they did before commits existed.
    let mut body = json!({"project": project, "link_id": input.link_id, "pr_ids": pr_ids});
    if !commits.is_empty() {
        body["commits"] = json!(commits);
    }
    let request = match open(&mut tx, &who, operation_id, ACTION_REVOKE, &body).await? {
        Ok(r) => r,
        Err(first) => {
            tx.commit().await?;
            return Ok(
                serde_json::from_value(first["revoked"].clone()).map(|r: RevokedRef| Revoked {
                    security_epoch: r.security_epoch,
                    link_id: r.link_id,
                    pr_ids: r.pr_ids,
                    commits: r.commits,
                })?,
            );
        }
    };
    commands::lock_executor(&mut tx, &who).await?;
    auth::require_step_up(&mut tx, policy, &caller).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access_of(&mut tx, caller, project).await?;
    if !a.can_clean {
        return Err(forbidden("revoking GitHub content needs manage_project"));
    }
    if !pr_ids.is_empty() && !a.is_admin {
        for pr in &pr_ids {
            if !visible_in(&mut tx, &a.scope, project, *pr).await? {
                return Err(pr_not_found());
            }
        }
    }
    if a.is_admin {
        for pr in &pr_ids {
            let any: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pr_issue_refs WHERE pr_id = $1 AND project_id = $2)
                     OR EXISTS (SELECT 1 FROM issue_fix_links WHERE pr_id = $1 AND project_id = $2)",
            )
            .bind(pr)
            .bind(project)
            .fetch_one(&mut *tx)
            .await?;
            if !any {
                return Err(pr_not_found());
            }
        }
    }
    for c in &commits {
        let scope = (!a.is_admin).then_some(&a.scope);
        if !commit_visible_in(&mut tx, scope, project, c).await? {
            return Err(commit_not_found());
        }
    }
    let epoch: i64 = sqlx::query_scalar(
        "UPDATE projects SET security_epoch = security_epoch + 1 WHERE id = $1 RETURNING security_epoch",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await?;
    // §19.9 / §7.5: the derived copies, as for a redacted event (B14).
    crate::redaction::derived(&mut tx, project, who.user_id).await?;
    let payload = match input.link_id {
        Some(link) => {
            unlink_locked(&mut tx, project, link, who.user_id).await?;
            json!({"link_id": link})
        }
        None if !commits.is_empty() => {
            for c in &commits {
                revoke_commit(&mut tx, project, c, who.user_id).await?;
            }
            // Ids only: never the message.
            json!({"commits": commits})
        }
        None => {
            for pr in &pr_ids {
                set_hidden(&mut tx, project, *pr, true, who.user_id).await?;
                // Revoked for good: so is what its keywords asked for.
                super::fixes::drop_pr(&mut tx, project, *pr).await?;
            }
            json!({"pr_ids": pr_ids})
        }
    };
    event(
        &mut tx,
        project,
        "github_content_revoked",
        who.user_id,
        request,
        payload,
    )
    .await?;
    let out = Revoked {
        security_epoch: epoch,
        link_id: input.link_id,
        pr_ids,
        commits,
    };
    commands::complete_request(
        &mut tx,
        request,
        &json!({"revoked": {"security_epoch": out.security_epoch, "link_id": out.link_id,
                            "pr_ids": out.pr_ids, "commits": out.commits}}),
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

#[derive(Deserialize)]
struct RevokedRef {
    security_epoch: i64,
    link_id: Option<Uuid>,
    pr_ids: Vec<Uuid>,
    /// Absent in results stored before commits could be revoked.
    #[serde(default)]
    commits: Vec<CommitRef>,
}

// ---------------------------------------------------------------- admin

async fn require_admin(conn: &mut PgConnection, caller: AuthContext) -> Result<()> {
    let me = db::effective(&mut *conn, caller).await?;
    if !me.is_admin || me.agent.is_some() {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "GitHub settings are for administrators",
        )
        .with_details(json!({"reason": "admin"}))
        .into());
    }
    Ok(())
}

async fn admin_view(conn: &mut PgConnection) -> Result<AdminGithub> {
    type Inst = (Uuid, i64, String, String, Option<DateTime<Utc>>);
    let inst: Vec<Inst> = sqlx::query_as(
        "SELECT id, gh_installation_id, account_login, state, last_push_at
         FROM github_installations ORDER BY account_login, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    type Row = (Uuid, Option<Uuid>, i64, String, bool, bool, String, i64);
    let repos: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.installation_id, r.gh_repo_id, r.full_name, r.is_private, r.linkable, r.state,
                (SELECT count(*) FROM project_repo_links l WHERE l.repo_id = r.id AND l.unlinked_at IS NULL)
         FROM github_repos r ORDER BY r.full_name, r.id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let to = |r: &Row| AdminRepo {
        repo_id: r.0,
        gh_repo_id: r.2,
        full_name: r.3.clone(),
        is_private: r.4,
        linkable: r.5,
        state: r.6.clone(),
        links: r.7,
    };
    Ok(AdminGithub {
        installations: inst
            .into_iter()
            .map(|(id, gh, login, state, last_push_at)| AdminInstallation {
                id,
                gh_installation_id: gh,
                account_login: login,
                state,
                last_push_at,
                repos: repos.iter().filter(|r| r.1 == Some(id)).map(to).collect(),
            })
            .collect(),
        detached_repos: repos.iter().filter(|r| r.1.is_none()).map(to).collect(),
    })
}

/// `GET /admin/github`.
pub async fn admin_get(app: &App, caller: AuthContext) -> Result<AdminGithub> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    require_admin(&mut tx, caller).await?;
    let v = admin_view(&mut tx).await?;
    tx.commit().await?;
    Ok(v)
}

/// `PUT /admin/github`: the `linkable` switch of repositories. Closing a
/// repository keeps existing links (they can still be cleaned up, T-G23);
/// only new links check it.
pub async fn admin_put(
    app: &App,
    caller: AuthContext,
    input: &AdminGithubPut,
) -> Result<AdminGithub> {
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    require_admin(&mut tx, caller).await?;
    for r in &input.repos {
        let n =
            sqlx::query("UPDATE github_repos SET linkable = $2, updated_at = now() WHERE id = $1")
                .bind(r.repo_id)
                .bind(r.linkable)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if n == 0 {
            return Err(ApiError::new(ErrorCode::NotFound, "no such repository")
                .with_details(json!({"repo_id": r.repo_id}))
                .into());
        }
    }
    let v = admin_view(&mut tx).await?;
    tx.commit().await?;
    Ok(v)
}
