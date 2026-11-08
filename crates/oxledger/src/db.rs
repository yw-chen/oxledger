// SPDX-License-Identifier: GPL-2.0-or-later
//! PostgreSQL access: per-request scope loading (D10) and predicate → SQL.

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

/// The authenticated caller. `None` = anonymous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller(pub Option<Uuid>);

/// A [`Caller`] checked against the database right now (contract 02 §2 P4).
/// Every authorization function starts here, so "the principal exists, is a
/// user and is active" is decided in one place: a key or session that was
/// valid when the connection started stops working as soon as the user is
/// locked, also on a long-lived stdio process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// `None` = anonymous.
    pub user_id: Option<Uuid>,
    pub is_admin: bool,
}

/// Resolves `caller` to an [`Effective`] identity. An unknown, locked or
/// non-user principal is `unauthenticated` (the same answer for each, so the
/// error does not tell which).
pub async fn effective(conn: &mut PgConnection, caller: Caller) -> Result<Effective> {
    let Some(id) = caller.0 else {
        return Ok(Effective {
            user_id: None,
            is_admin: false,
        });
    };
    let is_admin: Option<bool> = sqlx::query_scalar(
        "SELECT is_admin FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active'",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    match is_admin {
        Some(is_admin) => Ok(Effective {
            user_id: Some(id),
            is_admin,
        }),
        None => Err(api(
            ErrorCode::Unauthenticated,
            "the credentials no longer belong to an active user",
        )),
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
pub async fn load_scope(conn: &mut PgConnection, caller: Caller) -> Result<Scope> {
    let me = effective(conn, caller).await?;
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
pub async fn role_ids_for(
    conn: &mut PgConnection,
    caller: Caller,
    project_id: Uuid,
) -> Result<Vec<Uuid>> {
    let me = effective(conn, caller).await?;
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

pub fn hash_api_key(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// Maps an API key to an active user.
pub async fn caller_for_key(pool: &PgPool, raw: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT p.id FROM api_keys k JOIN principals p ON p.id = k.principal_id
         WHERE k.token_sha256 = $1 AND p.kind = 'user' AND p.status = 'active'",
    )
    .bind(hash_api_key(raw))
    .fetch_optional(pool)
    .await?)
}

/// Whether the caller is an administrator (anonymous: false). Goes through
/// [`effective`], so a locked admin is `unauthenticated`, not "not admin".
pub async fn is_admin(conn: &mut PgConnection, caller: Caller) -> Result<bool> {
    Ok(effective(conn, caller).await?.is_admin)
}
