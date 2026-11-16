// SPDX-License-Identifier: GPL-2.0-or-later
//! What agents read of the wiki (design 06 §3.12 R-AG1, §4.1): MCP
//! `wiki_list` and `wiki_get`, the wiki part of `search`, and each
//! delegated project's wiki status for `describe`.
//!
//! An agent reads a project's wiki when the project is delegated to it
//! (a live delegation in its work context), its delegator reads that wiki
//! (R-W1) and W(P) ⊇ A(P): everyone who reads the project's non-private
//! issues also reads its wiki ([`crate::audience::wiki_delegable`]). Then
//! what the agent copies out of the wiki reaches no one who could not have
//! read it, so the 02 §3.8 proof needs no exception (R-AG2). Otherwise
//! every wiki tool for that project is `forbidden_op` with
//! `wiki_not_delegable`, the same whether or not the page exists. An older
//! version also needs `view_wiki_edits` for the delegator and H(P) ⊇ A(P)
//! (`wiki_history_not_delegable`). Deleted pages are never readable (B-55).

use super::{
    Access, PageRow, VIEW_WIKI_EDITS, WikiPerson, find_for_read, forbidden, links, live_children,
    not_found, page_by_id, person, project_id,
};
use crate::audience::{self, WikiDelegable};
use crate::auth::AuthContext;
use crate::error::{ApiError, ErrorCode};
use crate::perm;
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Pages per `wiki_list` call.
pub const LIST_PAGE: i64 = 200;

fn not_agent() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ForbiddenOp,
        "the agent wiki tools are for agents",
    )
    .with_details(serde_json::json!({"reason": "agent_only"}))
    .into()
}

pub(crate) fn not_delegable() -> anyhow::Error {
    forbidden(
        "wiki_not_delegable",
        "this project's wiki has fewer readers than its issues, so agents cannot read or write it",
    )
}

/// The delegated projects the agent `caller` reads the wiki of (R-AG1),
/// with the delegator's access; and the delegated projects whose wiki the
/// delegator reads but R-AG1 refuses.
pub(crate) struct AgentWikis {
    pub readable: BTreeMap<Uuid, (Access, WikiDelegable)>,
    pub refused: BTreeMap<Uuid, Access>,
}

/// [`AgentWikis`] for `caller`, an agent (`None` for anyone else).
pub(crate) async fn agent_wikis(
    conn: &mut PgConnection,
    caller: AuthContext,
) -> Result<Option<AgentWikis>> {
    let actor = perm::actor(&mut *conn, caller).await?;
    agent_wikis_of(&mut *conn, &actor).await
}

/// [`agent_wikis`] for an actor already loaded.
pub(crate) async fn agent_wikis_of(
    conn: &mut PgConnection,
    actor: &perm::Actor,
) -> Result<Option<AgentWikis>> {
    let Some(delegated) = &actor.delegations else {
        return Ok(None);
    };
    let mut out = AgentWikis {
        readable: BTreeMap::new(),
        refused: BTreeMap::new(),
    };
    let ids: Vec<Uuid> = delegated.keys().copied().collect();
    for p in ids {
        let Some(a) = super::access_with(&mut *conn, actor, p).await? else {
            continue;
        };
        let d = audience::wiki_delegable(&mut *conn, p).await?;
        if d.pages {
            out.readable.insert(p, (a, d));
        } else {
            out.refused.insert(p, a);
        }
    }
    Ok(Some(out))
}

/// R-AG1 for one project named by key or id: the delegator's access when
/// the agent may read it. `history`: an older version is asked for.
pub(crate) async fn agent_access(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: &str,
    history: bool,
) -> Result<Access> {
    let actor = perm::actor(&mut *conn, caller).await?;
    let Some(delegated) = &actor.delegations else {
        return Err(not_agent());
    };
    let Some(id) = project_id(&mut *conn, project).await? else {
        return Err(not_found());
    };
    if !delegated.contains_key(&id) {
        return Err(not_found());
    }
    let Some(a) = super::access_with(&mut *conn, &actor, id).await? else {
        return Err(not_found());
    };
    let d = audience::wiki_delegable(&mut *conn, id).await?;
    if !d.pages {
        return Err(not_delegable());
    }
    if history {
        if !a.has(VIEW_WIKI_EDITS) {
            return Err(forbidden(
                "permission",
                "older versions of wiki pages need view_wiki_edits",
            ));
        }
        if !d.history {
            return Err(forbidden(
                "wiki_history_not_delegable",
                "this project's wiki history has fewer readers than its issues, so agents cannot read older versions",
            ));
        }
    }
    Ok(a)
}

// ---------------------------------------------------------------- wiki_list

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiListParams {
    /// Project key (e.g. "AUTH") or id.
    pub project: String,
    /// Only the pages below this one (its title or page id), at any depth.
    #[serde(default)]
    pub under: Option<String>,
    /// `next_cursor` of the previous call.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct WikiListItem {
    pub page_id: Uuid,
    pub title: String,
    pub parent_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct WikiListResult {
    pub project: String,
    /// Live pages by title, at most 200 per call.
    pub items: Vec<WikiListItem>,
    /// More pages follow: call again with `next_cursor`.
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

fn list_cursor(c: Option<&str>) -> Result<i64> {
    match c {
        None => Ok(0),
        Some(c) => c
            .strip_prefix("w")
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| *n >= 0)
            .ok_or_else(|| crate::error::api(ErrorCode::InvalidCursor, "not a wiki_list cursor")),
    }
}

/// A page of the wiki by title (an old title follows its redirect) or id.
async fn page_ref(
    conn: &mut PgConnection,
    project: Uuid,
    page: &str,
) -> Result<(PageRow, Option<String>)> {
    if let Ok(id) = Uuid::parse_str(page.trim()) {
        let p = page_by_id(&mut *conn, project, id)
            .await?
            .filter(|p| p.deleted_at.is_none())
            .ok_or_else(not_found)?;
        return Ok((p, None));
    }
    find_for_read(&mut *conn, project, page).await
}

pub async fn list(app: &App, caller: AuthContext, p: WikiListParams) -> Result<WikiListResult> {
    let offset = list_cursor(p.cursor.as_deref())?;
    let mut tx = read_tx(app).await?;
    let a = agent_access(&mut tx, caller, &p.project, false).await?;
    let under = match &p.under {
        None => None,
        Some(u) => Some(page_ref(&mut tx, a.project, u).await?.0.id),
    };
    let rows: Vec<(Uuid, String, Option<Uuid>, DateTime<Utc>, i32)> = sqlx::query_as(
        "WITH RECURSIVE below AS (
             SELECT id FROM wiki_pages WHERE parent_id = $2 AND deleted_at IS NULL
             UNION
             SELECT w.id FROM wiki_pages w JOIN below b ON w.parent_id = b.id WHERE w.deleted_at IS NULL
         )
         SELECT id, title, parent_id, updated_at, current_version FROM wiki_pages
         WHERE project_id = $1 AND deleted_at IS NULL
           AND ($2::uuid IS NULL OR id IN (SELECT id FROM below))
         ORDER BY title, id OFFSET $3 LIMIT $4",
    )
    .bind(a.project)
    .bind(under)
    .bind(offset)
    .bind(LIST_PAGE + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let truncated = rows.len() as i64 > LIST_PAGE;
    let items: Vec<WikiListItem> = rows
        .into_iter()
        .take(LIST_PAGE as usize)
        .map(
            |(page_id, title, parent_id, updated_at, version)| WikiListItem {
                page_id,
                title,
                parent_id,
                updated_at,
                version,
            },
        )
        .collect();
    Ok(WikiListResult {
        project: a.key,
        next_cursor: truncated.then(|| format!("w{}", offset + LIST_PAGE)),
        truncated,
        items,
    })
}

// ---------------------------------------------------------------- wiki_get

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiGetParams {
    /// Project key (e.g. "AUTH") or id.
    pub project: String,
    /// The page's title (an old title follows its redirect) or page id.
    pub page: String,
    /// An older version (needs the history to be readable by agents).
    #[serde(default)]
    pub version: Option<i32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct AgentPageLink {
    pub page_id: Uuid,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct AgentWikiPage {
    pub page_id: Uuid,
    pub project: String,
    pub title: String,
    /// The Markdown source of `version`.
    pub body: String,
    /// Always `markdown`.
    pub format: String,
    pub version: i32,
    pub current_version: i32,
    /// Pass it as the precondition of a `wiki_put` (`{"wiki_page": page_id,
    /// "lock_version": …}`).
    pub lock_version: i64,
    /// Agents cannot write a protected page.
    pub protected: bool,
    pub parent: Option<AgentPageLink>,
    pub children: Vec<AgentPageLink>,
    /// Attachment metadata (none before M2.4).
    pub attachments: Vec<serde_json::Value>,
    /// When `version` was written.
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<WikiPerson>,
    /// The version's change comment.
    pub comment: String,
    /// The version was redacted: its body is a placeholder.
    pub redacted: bool,
    /// The title asked for, when it was an old title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirected_from: Option<String>,
}

pub async fn get(app: &App, caller: AuthContext, p: WikiGetParams) -> Result<AgentWikiPage> {
    let mut tx = read_tx(app).await?;
    let a = agent_access(&mut tx, caller, &p.project, p.version.is_some()).await?;
    let (page, redirected_from) = page_ref(&mut tx, a.project, &p.page).await?;
    let n = p.version.unwrap_or(page.current_version);
    let row: Option<(String, String, Uuid, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT body, comment, actor_id, created_at, redaction_id IS NOT NULL
         FROM wiki_page_versions WHERE page_id = $1 AND version = $2",
    )
    .bind(page.id)
    .bind(n)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((body, comment, actor, at, redacted)) = row else {
        return Err(crate::error::api(ErrorCode::NotFound, "no such version"));
    };
    let kids = live_children(&mut tx, page.id).await?;
    let mut ids = kids.clone();
    ids.extend(page.parent_id);
    let names = links(&mut tx, &ids).await?;
    let link = |id: &Uuid| AgentPageLink {
        page_id: *id,
        title: names.get(id).cloned().unwrap_or_default(),
    };
    let mut children: Vec<AgentPageLink> = kids.iter().map(link).collect();
    children.sort_by(|x, y| x.title.cmp(&y.title));
    let updated_by = person(&mut tx, actor).await?;
    tx.commit().await?;
    Ok(AgentWikiPage {
        page_id: page.id,
        project: a.key.clone(),
        title: page.title.clone(),
        body,
        format: "markdown".into(),
        version: n,
        current_version: page.current_version,
        lock_version: page.lock_version,
        protected: page.protected,
        parent: page.parent_id.as_ref().map(link),
        children,
        attachments: Vec::new(),
        updated_at: at,
        updated_by,
        comment,
        redacted,
        redirected_from,
    })
}

// ---------------------------------------------------------------- describe

/// `describe agent_context`: one delegated project's wiki for the agent.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ContextWiki {
    /// `readable`, `writable` or `unavailable`.
    pub state: String,
    /// Why not writable (or not readable): `project_archived`, `wiki_module_off`,
    /// `wiki_not_delegable`, `wiki_readers_differ` (the wiki has more
    /// readers than the issues), `protected_project` (closed project),
    /// `no_permission`, `not_delegated` (no wiki op in the delegation),
    /// `audience` (a source project has fewer readers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ContextWiki {
    fn new(state: &str, reason: Option<&str>) -> Self {
        ContextWiki {
            state: state.into(),
            reason: reason.map(String::from),
        }
    }
}

/// The wiki status of `project` for the agent `caller` whose context's
/// sources are `sources` (R-AG1, R-AG3). `ops`: the delegation's commands.
pub(crate) async fn context_wiki(
    conn: &mut PgConnection,
    wikis: &AgentWikis,
    project: Uuid,
    ops: &[String],
    allowed: &audience::Audience,
) -> Result<ContextWiki> {
    if wikis.refused.contains_key(&project) {
        return Ok(ContextWiki::new("unavailable", Some("wiki_not_delegable")));
    }
    let Some((a, d)) = wikis.readable.get(&project) else {
        let (archived, module): (bool, bool) = sqlx::query_as(
            "SELECT p.status = 'archived',
                    EXISTS (SELECT 1 FROM enabled_modules WHERE project_id = p.id AND module = 'wiki')
             FROM projects p WHERE p.id = $1",
        )
        .bind(project)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or((true, false));
        return Ok(ContextWiki::new(
            "unavailable",
            Some(if archived {
                "project_archived"
            } else if module {
                "no_permission"
            } else {
                "wiki_module_off"
            }),
        ));
    };
    let reason = if !ops.iter().any(|o| o == "wiki_put" || o == "wiki_move") {
        Some("not_delegated")
    } else if !a.writable() {
        Some("protected_project")
    } else if !a.has(super::EDIT_WIKI_PAGES) {
        Some("no_permission")
    } else if !d.equal {
        Some("wiki_readers_differ")
    } else if !audience::wiki_audience(&mut *conn, project, super::VIEW_WIKI_PAGES)
        .await?
        .is_subset(allowed)
    {
        Some("audience")
    } else {
        None
    };
    Ok(match reason {
        None => ContextWiki::new("writable", None),
        Some(r) => ContextWiki::new("readable", Some(r)),
    })
}
