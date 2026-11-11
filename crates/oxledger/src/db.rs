// SPDX-License-Identifier: GPL-2.0-or-later
//! PostgreSQL access: per-request scope loading (D10) and predicate → SQL.

use crate::auth::{AuthContext, AuthMethod};
use crate::error::{ErrorCode, api};
use anyhow::{Context, Result, bail};
use ox_core::{Pred, ProjectGrant, RoleGrant, Scope, Visibility};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const ISSUE_MODULE: &str = "issue_tracking";
pub const VIEW_ISSUES: &str = "view_issues";

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(url)
        .await
        .context("connect to PostgreSQL")?;
    MIGRATOR.run(&pool).await.context("run migrations")?;
    Ok(pool)
}

/// An [`AuthContext`] checked against the database right now (contract 02
/// §2 P4, §3.7). Every authorization function starts here, so "this session,
/// key or token is still valid and the people behind it are active" is decided
/// in one place, on every request: a credential that was valid when a
/// connection started stops working as soon as it is revoked, also on a
/// long-lived stdio process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// Whose visibility applies: the user, or an agent's delegator. `None` = anonymous.
    pub user_id: Option<Uuid>,
    /// Admin powers. Always false for agents (§3.5: an admin's delegation
    /// gives the agent no private content and no restricted fields).
    pub is_admin: bool,
    /// Set for agent tokens.
    pub agent: Option<AgentReach>,
}

/// What an agent token reaches right now (§3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReach {
    pub agent_principal_id: Uuid,
    pub context_id: Uuid,
    pub delegator_id: Uuid,
    pub delegator_is_admin: bool,
    /// Projects with a delegation that is not revoked or expired. Reads are
    /// limited to these; empty means the agent reads nothing.
    pub projects: Vec<Uuid>,
}

fn unauthenticated() -> anyhow::Error {
    api(
        ErrorCode::Unauthenticated,
        "the credentials are no longer valid",
    )
}

/// Resolves `ctx` to an [`Effective`] identity. Anything no longer valid is
/// `unauthenticated`, the same answer for every reason.
pub async fn effective(conn: &mut PgConnection, ctx: AuthContext) -> Result<Effective> {
    let user = |is_admin: Option<bool>| match is_admin {
        Some(is_admin) => Ok(Effective {
            user_id: ctx.principal_id(),
            is_admin,
            agent: None,
        }),
        None => Err(unauthenticated()),
    };
    match ctx.method() {
        AuthMethod::Anonymous => Ok(Effective {
            user_id: None,
            is_admin: false,
            agent: None,
        }),
        AuthMethod::Recipient => user(
            sqlx::query_scalar(
                "SELECT is_admin FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active'",
            )
            .bind(ctx.principal_id())
            .fetch_optional(&mut *conn)
            .await?,
        ),
        #[cfg(feature = "test-support")]
        AuthMethod::TestUser => user(
            sqlx::query_scalar(
                "SELECT is_admin FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active'",
            )
            .bind(ctx.principal_id())
            .fetch_optional(&mut *conn)
            .await?,
        ),
        AuthMethod::BrowserSession => {
            let sql = format!(
                "SELECT p.is_admin FROM browser_sessions s JOIN principals p ON p.id = s.user_id
                 JOIN user_passwords up ON up.user_id = s.user_id
                 WHERE s.id = $1 AND s.user_id = $2 AND {}",
                crate::auth::SESSION_VALID
            );
            user(
                sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                    .bind(ctx.session_id())
                    .bind(ctx.principal_id())
                    .fetch_optional(&mut *conn)
                    .await?,
            )
        }
        AuthMethod::LegacyApiKey => user(
            sqlx::query_scalar(
                "SELECT p.is_admin FROM api_keys k JOIN principals p ON p.id = k.principal_id
                 WHERE k.id = $1 AND k.principal_id = $2 AND k.revoked_at IS NULL
                   AND (k.expires_at IS NULL OR k.expires_at > now())
                   AND p.kind = 'user' AND p.status = 'active'",
            )
            .bind(ctx.credential_id())
            .bind(ctx.principal_id())
            .fetch_optional(&mut *conn)
            .await?,
        ),
        AuthMethod::AgentToken => {
            // Token, context, agent (the role the token is bound to, design
            // 05 §3.12.5), runtime, delegator and client, all valid now.
            let row: Option<(Uuid, Uuid, Uuid, bool)> = sqlx::query_as(
                "SELECT w.id, ap.agent_principal_id, ap.delegator_user_id, d.is_admin
                 FROM agent_credentials c
                 JOIN agent_work_contexts w ON w.id = c.context_id AND w.runtime_id = c.runtime_id
                 JOIN agent_principals ap ON ap.agent_principal_id = c.agent_principal_id
                 JOIN agent_runtimes rt ON rt.id = c.runtime_id
                 JOIN principals a ON a.id = ap.agent_principal_id
                 JOIN principals d ON d.id = ap.delegator_user_id
                 JOIN agent_clients cl ON cl.id = ap.agent_client_id
                 WHERE c.id = $1 AND c.context_id = $2 AND c.agent_principal_id = $3
                   AND c.revoked_at IS NULL AND c.expires_at > now()
                   AND w.state = 'active' AND rt.retired_at IS NULL AND ap.retired_at IS NULL
                   AND a.kind = 'agent' AND a.status = 'active'
                   AND d.kind = 'user' AND d.status = 'active'
                   AND cl.enabled",
            )
            .bind(ctx.credential_id())
            .bind(ctx.context_id())
            .bind(ctx.principal_id())
            .fetch_optional(&mut *conn)
            .await?;
            let Some((context_id, agent_id, delegator_id, delegator_is_admin)) = row else {
                let sql = format!(
                    "SELECT EXISTS (SELECT 1 FROM agent_credentials c
                                    JOIN agent_work_contexts w ON w.id = c.context_id
                                    WHERE c.id = $1 AND {})",
                    crate::agents::CLOSED_WITH_CONTEXT
                );
                let closed: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                    .bind(ctx.credential_id())
                    .fetch_one(&mut *conn)
                    .await?;
                return Err(if closed {
                    crate::agents::context_closed()
                } else {
                    unauthenticated()
                });
            };
            agent_reach(
                conn,
                context_id,
                agent_id,
                delegator_id,
                delegator_is_admin,
            )
            .await
        }
        AuthMethod::BuiltinRun => {
            // Design 05 §3.13.1 (appendix D V13): every tool call re-checks
            // that AI is enabled, the oxledger-ai client is enabled, the user
            // and the principal are active and not paused or retired, the run
            // is still running within its lease, and the conversation's
            // context is the runtime's active context.
            // In a write transaction the run row is held (FOR SHARE) until
            // the write commits: a cancel, the sweeper or the run's end
            // (which update the row) wait for it, so a write never lands
            // after the run was seen ended. Reads run READ ONLY, where
            // row locks are not allowed; they write nothing.
            let read_only: String =
                sqlx::query_scalar("SELECT current_setting('transaction_read_only')")
                    .fetch_one(&mut *conn)
                    .await?;
            let lock = if read_only == "on" {
                ""
            } else {
                " FOR SHARE OF r"
            };
            let row: Option<(Uuid, Uuid, Uuid, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT w.id, ap.agent_principal_id, ap.delegator_user_id, d.is_admin
                 FROM ai_runs r
                 JOIN ai_conversations cv ON cv.id = r.conversation_id AND cv.user_id = r.user_id
                 JOIN agent_work_contexts w ON w.id = cv.context_id AND w.runtime_id = cv.runtime_id
                 JOIN agent_principals ap ON ap.agent_principal_id = cv.agent_principal_id
                                         AND ap.runtime_id = cv.runtime_id
                 JOIN agent_runtimes rt ON rt.id = cv.runtime_id AND rt.kind = 'builtin'
                 JOIN principals a ON a.id = ap.agent_principal_id
                 JOIN principals d ON d.id = ap.delegator_user_id
                 JOIN agent_clients cl ON cl.id = ap.agent_client_id
                 JOIN ai_settings s ON s.id
                 WHERE r.id = $1 AND w.id = $2 AND ap.agent_principal_id = $3 AND d.id = $4
                   AND r.state = 'running' AND r.lease_until > clock_timestamp()
                   AND r.cancel_requested_at IS NULL
                   AND cv.state = 'open' AND cv.deleted_at IS NULL
                   AND w.state = 'active' AND rt.retired_at IS NULL
                   AND ap.retired_at IS NULL AND ap.paused_at IS NULL
                   AND a.kind = 'agent' AND a.status = 'active'
                   AND d.kind = 'user' AND d.status = 'active'
                   AND cl.enabled AND cl.builtin AND s.enabled{lock}",
            )))
            .bind(ctx.run_id())
            .bind(ctx.context_id())
            .bind(ctx.principal_id())
            .bind(ctx.on_behalf_of())
            .fetch_optional(&mut *conn)
            .await?;
            let Some((context_id, agent_id, delegator_id, delegator_is_admin)) = row else {
                return Err(unauthenticated());
            };
            agent_reach(
                conn,
                context_id,
                agent_id,
                delegator_id,
                delegator_is_admin,
            )
            .await
        }
    }
}

/// The rest of an agent's identity once its credential (token or run)
/// checked out: R's epochs, then the role's readable delegated projects.
async fn agent_reach(
    conn: &mut PgConnection,
    context_id: Uuid,
    agent_id: Uuid,
    delegator_id: Uuid,
    delegator_is_admin: bool,
) -> Result<Effective> {
    {
        {
            // §3.5: every request compares the epochs of R; any change
            // quarantines the whole context (reads too).
            let changed: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM agent_context_source_projects r
                                JOIN projects p ON p.id = r.project_id
                                WHERE r.context_id = $1 AND p.security_epoch <> r.security_epoch)",
            )
            .bind(context_id)
            .fetch_one(&mut *conn)
            .await?;
            if changed {
                // Design 05 §3.12.2 (T-M1-87): every role of the runtime
                // stops; say which others share the context.
                let shared_with: Vec<String> = sqlx::query_scalar(
                    "SELECT p.handle FROM agent_principals p
                     JOIN agent_work_contexts w ON w.runtime_id = p.runtime_id
                     WHERE w.id = $1 AND p.agent_principal_id <> $2 AND p.retired_at IS NULL
                     ORDER BY p.handle",
                )
                .bind(context_id)
                .bind(agent_id)
                .fetch_all(&mut *conn)
                .await?;
                return Err(crate::agents::quarantined_shared(shared_with));
            }
            // Reads follow this role's own delegations; R (above) is the
            // runtime's (design 05 §3.12.2, T-M1-88).
            let delegated: Vec<Uuid> = sqlx::query_scalar(
                "SELECT project_id FROM delegations
                 WHERE context_id = $1 AND agent_principal_id = $2 AND revoked_at IS NULL AND expires_at > now()
                 ORDER BY project_id",
            )
            .bind(context_id)
            .bind(agent_id)
            .fetch_all(&mut *conn)
            .await?;
            // A project that stopped being audience-consistent is not readable,
            // even before S6's epoch triggers make the change quarantine R.
            let mut projects = Vec::with_capacity(delegated.len());
            for p in delegated {
                if crate::agents::audience_consistent(&mut *conn, p).await? {
                    projects.push(p);
                }
            }
            Ok(Effective {
                user_id: Some(delegator_id),
                is_admin: false,
                agent: Some(AgentReach {
                    agent_principal_id: agent_id,
                    context_id,
                    delegator_id,
                    delegator_is_admin,
                    projects,
                }),
            })
        }
    }
}

#[derive(sqlx::FromRow)]
struct RoleRow {
    project_id: Uuid,
    issues_visibility: String,
    all_trackers: bool,
    trackers: Vec<Uuid>,
}

fn grant_from(vis: &str, all_trackers: bool, trackers: Vec<Uuid>) -> Result<RoleGrant> {
    let Some(visibility) = Visibility::parse(vis) else {
        bail!("bad issues_visibility {vis}")
    };
    Ok(RoleGrant {
        visibility,
        trackers: if all_trackers { None } else { Some(trackers) },
    })
}

/// Computes what `caller` may see right now. Mirrors Redmine's
/// `Project.allowed_to_condition(user, :view_issues)`:
/// archived projects and projects without the issue module are excluded;
/// admins get `all` everywhere; members get their member roles (direct or via
/// a group); public projects give non-members the built-in role.
///
/// Takes a connection so it can run inside the transaction that applies a
/// write (contract 02 §2 P3); read paths pass a pooled connection.
///
/// Agents (§3.5): the delegator's scope, cut down to the delegated projects,
/// to non-private issues, and without `own` roles (an `own` role sees the
/// delegator's issues, which is not an audience the agent may read from).
pub async fn load_scope(conn: &mut PgConnection, ctx: AuthContext) -> Result<Scope> {
    let me = effective(conn, ctx).await?;
    let Some(agent) = &me.agent else {
        return user_scope(conn, me.user_id, me.is_admin).await;
    };
    agent_scope(
        conn,
        agent.delegator_id,
        agent.delegator_is_admin,
        Some(&agent.projects),
    )
    .await
}

/// The visibility a write document is checked against before anything else
/// (看不到＝不存在, M1.11 gate). A person: [`load_scope`]. An agent: its
/// delegator's scope cut down to every project its context was ever
/// delegated, revoked or expired ones included: a target there is answered
/// by the delegation checks (`delegation_unavailable`, 02 T-X19), a target
/// anywhere else is `not_found` like a missing issue.
pub(crate) async fn write_scope(conn: &mut PgConnection, ctx: AuthContext) -> Result<Scope> {
    let me = effective(conn, ctx).await?;
    let Some(agent) = &me.agent else {
        return user_scope(conn, me.user_id, me.is_admin).await;
    };
    let projects: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT project_id FROM delegations WHERE context_id = $1")
            .bind(agent.context_id)
            .fetch_all(&mut *conn)
            .await?;
    agent_scope(
        conn,
        agent.delegator_id,
        agent.delegator_is_admin,
        Some(&projects),
    )
    .await
}

/// What an agent of `delegator` reads (§3.5): the delegator's scope cut down
/// to `projects` (`None`: every project the delegator reads that is
/// audience-consistent, for re-checking the built-in AI's stored replies
/// as the user reads them, design 05 §3.13.5), non-private issues, no
/// `own` roles.
pub(crate) async fn agent_scope(
    conn: &mut PgConnection,
    delegator: Uuid,
    delegator_is_admin: bool,
    projects: Option<&[Uuid]>,
) -> Result<Scope> {
    let full = user_scope(conn, Some(delegator), delegator_is_admin).await?;
    let mut allowed: Vec<Uuid> = Vec::new();
    match projects {
        Some(p) => allowed.extend_from_slice(p),
        None => {
            for g in &full.grants {
                if crate::agents::audience_consistent(&mut *conn, g.project_id).await? {
                    allowed.push(g.project_id);
                }
            }
        }
    }
    let grants = full
        .grants
        .into_iter()
        .filter(|g| allowed.contains(&g.project_id))
        .map(|g| ProjectGrant {
            project_id: g.project_id,
            roles: g
                .roles
                .into_iter()
                .filter(|r| r.visibility != Visibility::Own)
                .collect(),
        })
        .collect();
    // No user id: role clauses become "not private" (ox-core `role_clause`).
    Ok(Scope::anonymous(grants))
}

/// The scope of a user (or anonymous), as Redmine computes it.
async fn user_scope(
    conn: &mut PgConnection,
    user_id: Option<Uuid>,
    is_admin: bool,
) -> Result<Scope> {
    let me = Effective {
        user_id,
        is_admin,
        agent: None,
    };
    let eligible: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT p.id, p.is_public FROM projects p
         JOIN enabled_modules m ON m.project_id = p.id AND m.module = $1
         WHERE p.status <> 'archived'",
    )
    .bind(ISSUE_MODULE)
    .fetch_all(&mut *conn)
    .await?;
    let eligible_ids: BTreeSet<Uuid> = eligible.iter().map(|(id, _)| *id).collect();

    let builtin = if me.user_id.is_some() {
        "non_member"
    } else {
        "anonymous"
    };
    let builtin_role: Option<(String, bool, Vec<Uuid>)> = sqlx::query_as(
        "SELECT r.issues_visibility, rp.all_trackers,
                ARRAY(SELECT tracker_id FROM role_permission_trackers t
                      WHERE t.role_id = r.id AND t.permission = rp.permission)
         FROM roles r JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = $2
         WHERE r.builtin = $1",
    )
    .bind(builtin)
    .bind(VIEW_ISSUES)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(user_id) = me.user_id else {
        let grants = match builtin_role {
            None => Vec::new(),
            Some((vis, all, trackers)) => {
                let g = grant_from(&vis, all, trackers)?;
                eligible
                    .iter()
                    .filter(|(_, public)| *public)
                    .map(|(id, _)| ProjectGrant {
                        project_id: *id,
                        roles: vec![g.clone()],
                    })
                    .collect()
            }
        };
        return Ok(Scope::anonymous(grants));
    };

    let groups: Vec<Uuid> = sqlx::query_scalar(
        "SELECT group_id FROM group_members WHERE user_id = $1 ORDER BY group_id",
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut principal_ids = vec![user_id];
    principal_ids.extend(groups);

    if me.is_admin {
        let all = RoleGrant {
            visibility: Visibility::All,
            trackers: None,
        };
        let grants = eligible_ids
            .iter()
            .map(|id| ProjectGrant {
                project_id: *id,
                roles: vec![all.clone()],
            })
            .collect();
        return Ok(Scope {
            user_id: Some(user_id),
            principal_ids,
            grants,
        });
    }

    let member_projects: BTreeSet<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT project_id FROM members WHERE principal_id = ANY($1)")
            .bind(&principal_ids)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();

    let role_rows: Vec<RoleRow> = sqlx::query_as(
        "SELECT DISTINCT m.project_id, r.issues_visibility, rp.all_trackers,
                ARRAY(SELECT tracker_id FROM role_permission_trackers t
                      WHERE t.role_id = r.id AND t.permission = rp.permission ORDER BY tracker_id) AS trackers
         FROM members m
         JOIN member_roles mr ON mr.member_id = m.id
         JOIN roles r ON r.id = mr.role_id
         JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = $2
         WHERE m.principal_id = ANY($1)",
    )
    .bind(&principal_ids)
    .bind(VIEW_ISSUES)
    .fetch_all(&mut *conn)
    .await?;

    let mut by_project: BTreeMap<Uuid, Vec<RoleGrant>> = BTreeMap::new();
    for r in role_rows {
        if eligible_ids.contains(&r.project_id) {
            by_project.entry(r.project_id).or_default().push(grant_from(
                &r.issues_visibility,
                r.all_trackers,
                r.trackers,
            )?);
        }
    }
    if let Some((vis, all, trackers)) = builtin_role {
        let g = grant_from(&vis, all, trackers)?;
        for (id, public) in &eligible {
            if *public && !member_projects.contains(id) {
                by_project.entry(*id).or_default().push(g.clone());
            }
        }
    }
    let grants = by_project
        .into_iter()
        .map(|(project_id, roles)| ProjectGrant { project_id, roles })
        .collect();
    Ok(Scope {
        user_id: Some(user_id),
        principal_ids,
        grants,
    })
}

/// Roles the caller holds in a project, for workflow checks. Admins use every
/// role (Redmine `new_statuses_allowed_to`); non-members of a public project
/// get the built-in role.
///
/// For an agent these are the delegator's roles (workflow follows the
/// delegator's permissions); callers deciding what an agent may *read* must
/// not use them (see `Effective::agent`).
pub async fn role_ids_for(
    conn: &mut PgConnection,
    ctx: AuthContext,
    project_id: Uuid,
) -> Result<Vec<Uuid>> {
    let me = effective(conn, ctx).await?;
    let me = match &me.agent {
        None => me,
        Some(a) => Effective {
            user_id: Some(a.delegator_id),
            is_admin: a.delegator_is_admin,
            agent: None,
        },
    };
    let Some(user_id) = me.user_id else {
        return Ok(sqlx::query_scalar(
            "SELECT r.id FROM roles r JOIN projects p ON p.id = $1 AND p.is_public WHERE r.builtin = 'anonymous'",
        )
        .bind(project_id)
        .fetch_all(&mut *conn)
        .await?);
    };
    if me.is_admin {
        return Ok(sqlx::query_scalar("SELECT id FROM roles")
            .fetch_all(&mut *conn)
            .await?);
    }
    let member_roles: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT mr.role_id FROM members m JOIN member_roles mr ON mr.member_id = m.id
         WHERE m.project_id = $1
           AND m.principal_id IN (SELECT $2 UNION SELECT group_id FROM group_members WHERE user_id = $2)",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    let is_member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM members WHERE project_id = $1
           AND principal_id IN (SELECT $2 UNION SELECT group_id FROM group_members WHERE user_id = $2))",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await?;
    if is_member {
        return Ok(member_roles);
    }
    Ok(sqlx::query_scalar(
        "SELECT r.id FROM roles r JOIN projects p ON p.id = $1 AND p.is_public WHERE r.builtin = 'non_member'",
    )
    .bind(project_id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Appends `pred` as a SQL boolean expression over issue alias `a`.
pub fn push_pred(qb: &mut QueryBuilder<Postgres>, a: &str, pred: &Pred) {
    match pred {
        Pred::True => {
            qb.push("TRUE");
        }
        Pred::False => {
            qb.push("FALSE");
        }
        Pred::And(ps) | Pred::Or(ps) => {
            let op = if matches!(pred, Pred::And(_)) {
                " AND "
            } else {
                " OR "
            };
            qb.push("(");
            for (i, p) in ps.iter().enumerate() {
                if i > 0 {
                    qb.push(op);
                }
                push_pred(qb, a, p);
            }
            qb.push(")");
        }
        Pred::ProjectIs(id) => {
            qb.push(format!("{a}.project_id = ")).push_bind(*id);
        }
        Pred::TrackerIn(ids) => {
            qb.push(format!("{a}.tracker_id = ANY("))
                .push_bind(ids.clone())
                .push(")");
        }
        Pred::NotPrivate => {
            qb.push(format!("NOT {a}.is_private"));
        }
        Pred::AuthorIs(id) => {
            qb.push(format!("{a}.author_id = ")).push_bind(*id);
        }
        Pred::AssigneeIn(ids) => {
            qb.push(format!("{a}.assigned_to_id = ANY("))
                .push_bind(ids.clone())
                .push(")");
        }
    }
}

pub use crate::auth::hash_api_key;

/// Whether the caller is an administrator (anonymous: false). Goes through
/// [`effective`], so a locked admin is `unauthenticated`, not "not admin".
pub async fn is_admin(conn: &mut PgConnection, ctx: AuthContext) -> Result<bool> {
    Ok(effective(conn, ctx).await?.is_admin)
}
