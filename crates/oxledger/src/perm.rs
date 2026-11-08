// SPDX-License-Identifier: GPL-2.0-or-later
//! Permission evaluator (contract 02 §6.2).
//!
//! `perm(x)` is evaluated for one project and one tracker: each role's
//! `role_permission_trackers` rows count for *that* permission only, so a role
//! that sees every tracker but edits only Bugs cannot edit a Feature. Projects
//! must be `active` with the issue module for any write (Redmine: archived
//! allows nothing, closed allows reads only).
//!
//! Agents act with `EffectiveAuthority` = the delegator's permissions ∩ the
//! delegation (§3.5): the project must be delegated, and the op and field must
//! be in the delegation's `proposal_ops` / `writable_fields`. What an agent may
//! *read* is decided by `db::load_scope` and the visible-fields rule, not here.

use crate::auth::AuthContext;
use crate::db::{self, ISSUE_MODULE};
use anyhow::Result;
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const VIEW_ISSUES: &str = "view_issues";
pub const ADD_ISSUES: &str = "add_issues";
pub const EDIT_ISSUES: &str = "edit_issues";
pub const EDIT_OWN_ISSUES: &str = "edit_own_issues";
pub const ADD_ISSUE_NOTES: &str = "add_issue_notes";
pub const SET_NOTES_PRIVATE: &str = "set_notes_private";
pub const VIEW_PRIVATE_NOTES: &str = "view_private_notes";
pub const SET_ISSUES_PRIVATE: &str = "set_issues_private";
pub const SET_OWN_ISSUES_PRIVATE: &str = "set_own_issues_private";
pub const MANAGE_ISSUE_RELATIONS: &str = "manage_issue_relations";
pub const MANAGE_VERSIONS: &str = "manage_versions";

/// Every permission a role may hold; the `role_permissions_known` CHECK in
/// migration 0007 lists the same names (a test compares them).
pub const PERMISSIONS: &[&str] = &[
    VIEW_ISSUES,
    ADD_ISSUES,
    EDIT_ISSUES,
    EDIT_OWN_ISSUES,
    ADD_ISSUE_NOTES,
    SET_NOTES_PRIVATE,
    VIEW_PRIVATE_NOTES,
    SET_ISSUES_PRIVATE,
    SET_OWN_ISSUES_PRIVATE,
    MANAGE_ISSUE_RELATIONS,
    MANAGE_VERSIONS,
];

/// Proposal operations a delegation may allow (delegations.proposal_ops).
pub const OP_CREATE_ISSUE: &str = "create_issue";
pub const OP_UPDATE_FIELDS: &str = "update_fields";
pub const OP_TRANSITION: &str = "transition";
pub const OP_ADD_NOTE: &str = "add_note";
pub const OP_DUPLICATE_OF: &str = "duplicate_of";

/// One active delegation's scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationScope {
    pub proposal_ops: BTreeSet<String>,
    pub writable_fields: BTreeSet<String>,
    pub max_ops: i32,
}

/// Whose permissions apply. For an agent, the delegator's, plus the
/// delegation limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The user (or the agent's delegator); `None` = anonymous.
    pub user_id: Option<Uuid>,
    pub is_admin: bool,
    /// `user_id` and the groups the user belongs to.
    pub principal_ids: Vec<Uuid>,
    /// `Some` for agents: delegations by project. Only projects the agent may
    /// currently read (`AgentReach::projects`) are present.
    pub delegations: Option<BTreeMap<Uuid, DelegationScope>>,
}

impl Actor {
    pub fn is_agent(&self) -> bool {
        self.delegations.is_some()
    }

    /// How the actor reaches `project`.
    pub fn delegation(&self, project: Uuid) -> Reach<'_> {
        match &self.delegations {
            None => Reach::Person,
            Some(d) => d
                .get(&project)
                .map_or(Reach::NotDelegated, Reach::Delegated),
        }
    }

    /// Whether the delegation (if any) allows `op` on `field`.
    /// `field = None` checks the op only.
    pub fn delegation_allows(&self, project: Uuid, op: &str, field: Option<&str>) -> bool {
        match self.delegation(project) {
            Reach::NotDelegated => false,
            Reach::Person => true,
            Reach::Delegated(d) => {
                d.proposal_ops.contains(op) && field.is_none_or(|f| d.writable_fields.contains(f))
            }
        }
    }
}

/// See [`Actor::delegation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach<'a> {
    /// Not an agent: no delegation limits.
    Person,
    /// An agent with an active delegation for the project.
    Delegated(&'a DelegationScope),
    /// An agent without one: nothing is allowed.
    NotDelegated,
}

/// Resolves the caller into an [`Actor`] (re-checking the credential, §3.7).
pub async fn actor(conn: &mut PgConnection, ctx: AuthContext) -> Result<Actor> {
    let me = db::effective(&mut *conn, ctx).await?;
    let (user_id, is_admin) = match &me.agent {
        None => (me.user_id, me.is_admin),
        Some(a) => (Some(a.delegator_id), a.delegator_is_admin),
    };
    let mut principal_ids = Vec::new();
    if let Some(u) = user_id {
        principal_ids.push(u);
        let groups: Vec<Uuid> = sqlx::query_scalar(
            "SELECT group_id FROM group_members WHERE user_id = $1 ORDER BY group_id",
        )
        .bind(u)
        .fetch_all(&mut *conn)
        .await?;
        principal_ids.extend(groups);
    }
    let delegations = match &me.agent {
        None => None,
        Some(a) => {
            let rows: Vec<(Uuid, Vec<String>, Vec<String>, i32)> = sqlx::query_as(
                "SELECT project_id, proposal_ops, writable_fields, max_ops FROM delegations
                 WHERE context_id = $1 AND revoked_at IS NULL AND expires_at > now()
                   AND project_id = ANY($2)",
            )
            .bind(a.context_id)
            .bind(&a.projects)
            .fetch_all(&mut *conn)
            .await?;
            Some(
                rows.into_iter()
                    .map(|(p, ops, fields, max_ops)| {
                        (
                            p,
                            DelegationScope {
                                proposal_ops: ops.into_iter().collect(),
                                writable_fields: fields.into_iter().collect(),
                                max_ops,
                            },
                        )
                    })
                    .collect(),
            )
        }
    };
    Ok(Actor {
        user_id,
        is_admin,
        principal_ids,
        delegations,
    })
}

/// One role's permissions: permission → trackers (`None` = all trackers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePerms {
    pub id: Uuid,
    pub perms: BTreeMap<String, Option<BTreeSet<Uuid>>>,
}

impl RolePerms {
    fn allows(&self, perm: &str, tracker: Option<Uuid>) -> bool {
        match self.perms.get(perm) {
            None => false,
            Some(None) => true,
            Some(Some(trackers)) => match tracker {
                Some(t) => trackers.contains(&t),
                None => !trackers.is_empty(),
            },
        }
    }
}

/// The actor's permissions in one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPerms {
    pub project_id: Uuid,
    /// `active`, `closed` or `archived`.
    pub status: String,
    pub issue_module: bool,
    pub admin: bool,
    /// The roles that apply: member roles (direct or through a group), else
    /// the built-in Non member / Anonymous role on a public project. Admins:
    /// every role (Redmine uses `Role.all` for workflow).
    pub roles: Vec<RolePerms>,
}

impl ProjectPerms {
    /// Writes need an active project with the issue module.
    pub fn writable(&self) -> bool {
        self.issue_module && self.status == "active"
    }

    /// `perm(x)` on `tracker` (§6.2), ignoring project status (callers that
    /// write also check [`Self::writable`]).
    pub fn allowed(&self, perm: &str, tracker: Uuid) -> bool {
        if !self.issue_module || self.status == "archived" {
            return false;
        }
        self.admin || self.roles.iter().any(|r| r.allows(perm, Some(tracker)))
    }

    /// `perm(x)` on at least one tracker.
    pub fn allowed_any(&self, perm: &str) -> bool {
        if !self.issue_module || self.status == "archived" {
            return false;
        }
        self.admin || self.roles.iter().any(|r| r.allows(perm, None))
    }

    /// Redmine `attributes_editable?`: `edit_issues`, or `edit_own_issues`
    /// when the actor is the author; and the project accepts writes.
    pub fn attributes_editable(&self, tracker: Uuid, author: Uuid, me: Option<Uuid>) -> bool {
        self.writable()
            && (self.allowed(EDIT_ISSUES, tracker)
                || (self.allowed(EDIT_OWN_ISSUES, tracker) && me == Some(author)))
    }

    /// Roles that *consider workflow* (`add_issues` or `edit_issues`,
    /// Redmine `Role#consider_workflow?`); for admins, every such role.
    pub fn workflow_roles(&self) -> Vec<Uuid> {
        self.roles
            .iter()
            .filter(|r| r.perms.contains_key(ADD_ISSUES) || r.perms.contains_key(EDIT_ISSUES))
            .map(|r| r.id)
            .collect()
    }

    /// Role ids that decide custom field readability (`field_roles`).
    pub fn role_ids(&self) -> Vec<Uuid> {
        self.roles.iter().map(|r| r.id).collect()
    }
}

/// Loads the actor's permissions in `project_id`. `None` if the project does
/// not exist.
pub async fn project_perms(
    conn: &mut PgConnection,
    actor: &Actor,
    project_id: Uuid,
) -> Result<Option<ProjectPerms>> {
    let row: Option<(String, bool, bool)> = sqlx::query_as(
        "SELECT p.status, p.is_public,
                EXISTS (SELECT 1 FROM enabled_modules m WHERE m.project_id = p.id AND m.module = $2)
         FROM projects p WHERE p.id = $1",
    )
    .bind(project_id)
    .bind(ISSUE_MODULE)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((status, is_public, issue_module)) = row else {
        return Ok(None);
    };
    let role_ids: Vec<Uuid> = if actor.is_admin {
        sqlx::query_scalar("SELECT id FROM roles ORDER BY id")
            .fetch_all(&mut *conn)
            .await?
    } else {
        let member: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT mr.role_id FROM members m JOIN member_roles mr ON mr.member_id = m.id
             WHERE m.project_id = $1 AND m.principal_id = ANY($2) ORDER BY mr.role_id",
        )
        .bind(project_id)
        .bind(&actor.principal_ids)
        .fetch_all(&mut *conn)
        .await?;
        let is_member: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM members WHERE project_id = $1 AND principal_id = ANY($2))",
        )
        .bind(project_id)
        .bind(&actor.principal_ids)
        .fetch_one(&mut *conn)
        .await?;
        if is_member {
            member
        } else if is_public {
            let builtin = if actor.user_id.is_some() {
                "non_member"
            } else {
                "anonymous"
            };
            sqlx::query_scalar("SELECT id FROM roles WHERE builtin = $1")
                .bind(builtin)
                .fetch_all(&mut *conn)
                .await?
        } else {
            Vec::new()
        }
    };
    let rows: Vec<(Uuid, String, bool, Vec<Uuid>)> = sqlx::query_as(
        "SELECT rp.role_id, rp.permission, rp.all_trackers,
                ARRAY(SELECT t.tracker_id FROM role_permission_trackers t
                      WHERE t.role_id = rp.role_id AND t.permission = rp.permission)
         FROM role_permissions rp WHERE rp.role_id = ANY($1)",
    )
    .bind(&role_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut roles: BTreeMap<Uuid, RolePerms> = role_ids
        .iter()
        .map(|id| {
            (
                *id,
                RolePerms {
                    id: *id,
                    perms: BTreeMap::new(),
                },
            )
        })
        .collect();
    for (role, perm, all, trackers) in rows {
        if let Some(r) = roles.get_mut(&role) {
            r.perms
                .insert(perm, (!all).then(|| trackers.into_iter().collect()));
        }
    }
    Ok(Some(ProjectPerms {
        project_id,
        status,
        issue_module,
        admin: actor.is_admin,
        roles: roles.into_values().collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(roles: Vec<RolePerms>) -> ProjectPerms {
        ProjectPerms {
            project_id: Uuid::nil(),
            status: "active".into(),
            issue_module: true,
            admin: false,
            roles,
        }
    }

    fn role(id: u128, perms: &[(&str, Option<&[u128]>)]) -> RolePerms {
        RolePerms {
            id: Uuid::from_u128(id),
            perms: perms
                .iter()
                .map(|(p, t)| {
                    (
                        p.to_string(),
                        t.map(|t| t.iter().map(|n| Uuid::from_u128(*n)).collect()),
                    )
                })
                .collect(),
        }
    }

    /// Tracker sets are per permission: viewing all trackers does not make
    /// editing apply to all of them.
    #[test]
    fn per_permission_trackers() {
        let (bug, feature) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let p = pp(vec![role(
            9,
            &[(VIEW_ISSUES, None), (EDIT_ISSUES, Some(&[1]))],
        )]);
        assert!(p.allowed(VIEW_ISSUES, feature));
        assert!(p.allowed(EDIT_ISSUES, bug));
        assert!(!p.allowed(EDIT_ISSUES, feature));
        assert!(p.allowed_any(EDIT_ISSUES));
        assert!(!p.allowed_any(ADD_ISSUES));
        // A tracker-limited permission with no trackers grants nothing.
        let empty = pp(vec![role(9, &[(EDIT_ISSUES, Some(&[]))])]);
        assert!(!empty.allowed_any(EDIT_ISSUES));
    }

    #[test]
    fn edit_own_needs_author_and_project_status_matters() {
        let (t, me, other) = (Uuid::from_u128(1), Uuid::from_u128(5), Uuid::from_u128(6));
        let mut p = pp(vec![role(9, &[(EDIT_OWN_ISSUES, None)])]);
        assert!(p.attributes_editable(t, me, Some(me)));
        assert!(!p.attributes_editable(t, other, Some(me)));
        assert!(!p.attributes_editable(t, me, None));
        p.status = "closed".into();
        assert!(!p.attributes_editable(t, me, Some(me)));
        assert!(p.allowed(EDIT_OWN_ISSUES, t), "closed still reads");
        p.status = "archived".into();
        assert!(!p.allowed(EDIT_OWN_ISSUES, t));
        p.status = "active".into();
        p.issue_module = false;
        assert!(!p.allowed(EDIT_OWN_ISSUES, t));
    }

    #[test]
    fn workflow_roles_consider_add_or_edit() {
        let p = pp(vec![
            role(1, &[(VIEW_ISSUES, None)]),
            role(2, &[(ADD_ISSUES, Some(&[7]))]),
            role(3, &[(EDIT_ISSUES, None)]),
            role(4, &[(EDIT_OWN_ISSUES, None)]),
        ]);
        assert_eq!(
            p.workflow_roles(),
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
    }

    #[test]
    fn delegation_limits() {
        let p = Uuid::from_u128(1);
        let human = Actor {
            user_id: Some(Uuid::from_u128(5)),
            is_admin: false,
            principal_ids: vec![],
            delegations: None,
        };
        assert!(human.delegation_allows(p, OP_ADD_NOTE, Some("anything")));
        let mut d = BTreeMap::new();
        d.insert(
            p,
            DelegationScope {
                proposal_ops: [OP_UPDATE_FIELDS.to_string()].into(),
                writable_fields: ["subject".to_string()].into(),
                max_ops: 10,
            },
        );
        let agent = Actor {
            delegations: Some(d),
            ..human
        };
        assert!(agent.delegation_allows(p, OP_UPDATE_FIELDS, Some("subject")));
        assert!(!agent.delegation_allows(p, OP_UPDATE_FIELDS, Some("description")));
        assert!(!agent.delegation_allows(p, OP_ADD_NOTE, None));
        assert!(!agent.delegation_allows(Uuid::from_u128(2), OP_UPDATE_FIELDS, None));
    }
}
