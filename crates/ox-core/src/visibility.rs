// SPDX-License-Identifier: GPL-2.0-or-later
//! Issue visibility, following Redmine's `Issue.visible_condition` and
//! `Project.allowed_to_condition` (redmine/redmine app/models/issue.rb,
//! app/models/project.rb):
//!
//! * per project, the caller holds one or more roles; each role that grants
//!   `view_issues` contributes a clause `(mode AND tracker restriction)`;
//! * clauses are OR-ed within a project, projects are OR-ed together;
//! * `default` = not private, or author is me, or assignee is me or one of my groups;
//! * `own` = author is me, or assignee is me or one of my groups;
//! * anonymous callers only ever see non-private issues;
//! * an explicit, empty tracker restriction grants nothing.
//!
//! A [`Scope`] is computed per request (decision D10) and turned into a
//! [`Pred`] tree, which the server compiles to SQL and to a tantivy query.
//! [`Pred::eval`] is the in-memory form of the same tree.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    All,
    Default,
    Own,
}

impl Visibility {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Self::All),
            "default" => Some(Self::Default),
            "own" => Some(Self::Own),
            _ => None,
        }
    }
}

/// One role's contribution to `view_issues` in one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleGrant {
    pub visibility: Visibility,
    /// `None` = all trackers; `Some(vec![])` = no tracker (grants nothing).
    pub trackers: Option<Vec<Uuid>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGrant {
    pub project_id: Uuid,
    pub roles: Vec<RoleGrant>,
}

/// Who is asking, and what they may see. Built per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// `None` for anonymous callers.
    pub user_id: Option<Uuid>,
    /// The user's id plus the ids of every group the user belongs to.
    pub principal_ids: Vec<Uuid>,
    pub grants: Vec<ProjectGrant>,
}

/// The issue columns visibility depends on. These are also the search index
/// fast fields, so SQL, search and memory all filter on the same facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueFacts {
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    pub is_private: bool,
    pub author_id: Uuid,
    pub assigned_to_id: Option<Uuid>,
}

/// Boolean filter over [`IssueFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pred {
    True,
    False,
    And(Vec<Pred>),
    Or(Vec<Pred>),
    ProjectIs(Uuid),
    TrackerIn(Vec<Uuid>),
    NotPrivate,
    AuthorIs(Uuid),
    AssigneeIn(Vec<Uuid>),
}

impl Pred {
    pub fn eval(&self, f: &IssueFacts) -> bool {
        match self {
            Pred::True => true,
            Pred::False => false,
            Pred::And(ps) => ps.iter().all(|p| p.eval(f)),
            Pred::Or(ps) => ps.iter().any(|p| p.eval(f)),
            Pred::ProjectIs(id) => f.project_id == *id,
            Pred::TrackerIn(ids) => ids.contains(&f.tracker_id),
            Pred::NotPrivate => !f.is_private,
            Pred::AuthorIs(id) => f.author_id == *id,
            Pred::AssigneeIn(ids) => f.assigned_to_id.is_some_and(|a| ids.contains(&a)),
        }
    }

    /// Folds constants so callers emit smaller SQL / queries.
    pub fn simplify(self) -> Pred {
        match self {
            Pred::And(ps) => {
                let mut out = Vec::new();
                for p in ps.into_iter().map(Pred::simplify) {
                    match p {
                        Pred::True => {}
                        Pred::False => return Pred::False,
                        Pred::And(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => Pred::True,
                    1 => out.pop().unwrap(),
                    _ => Pred::And(out),
                }
            }
            Pred::Or(ps) => {
                let mut out = Vec::new();
                for p in ps.into_iter().map(Pred::simplify) {
                    match p {
                        Pred::False => {}
                        Pred::True => return Pred::True,
                        Pred::Or(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => Pred::False,
                    1 => out.pop().unwrap(),
                    _ => Pred::Or(out),
                }
            }
            Pred::TrackerIn(ids) if ids.is_empty() => Pred::False,
            Pred::AssigneeIn(ids) if ids.is_empty() => Pred::False,
            other => other,
        }
    }
}

impl Scope {
    pub fn anonymous(grants: Vec<ProjectGrant>) -> Self {
        Scope {
            user_id: None,
            principal_ids: Vec::new(),
            grants,
        }
    }

    fn role_clause(&self, role: &RoleGrant) -> Pred {
        let mode = match self.user_id {
            None => Pred::NotPrivate,
            Some(me) => {
                let mine = || {
                    vec![
                        Pred::AuthorIs(me),
                        Pred::AssigneeIn(self.principal_ids.clone()),
                    ]
                };
                match role.visibility {
                    Visibility::All => Pred::True,
                    Visibility::Default => {
                        let mut v = vec![Pred::NotPrivate];
                        v.extend(mine());
                        Pred::Or(v)
                    }
                    Visibility::Own => Pred::Or(mine()),
                }
            }
        };
        match &role.trackers {
            None => mode,
            Some(ids) => Pred::And(vec![mode, Pred::TrackerIn(ids.clone())]),
        }
    }

    /// The filter selecting exactly the issues this scope may see.
    pub fn predicate(&self) -> Pred {
        Pred::Or(
            self.grants
                .iter()
                .map(|g| {
                    Pred::And(vec![
                        Pred::ProjectIs(g.project_id),
                        Pred::Or(g.roles.iter().map(|r| self.role_clause(r)).collect()),
                    ])
                })
                .collect(),
        )
        .simplify()
    }

    pub fn can_see(&self, f: &IssueFacts) -> bool {
        self.predicate().eval(f)
    }

    /// Projects in which the caller can see at least something.
    pub fn project_ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.grants
            .iter()
            .filter(|g| !g.roles.is_empty())
            .map(|g| g.project_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn facts(private: bool, author: u128, assignee: Option<u128>, tracker: u128) -> IssueFacts {
        IssueFacts {
            project_id: id(100),
            tracker_id: id(tracker),
            is_private: private,
            author_id: id(author),
            assigned_to_id: assignee.map(id),
        }
    }

    fn scope(vis: Visibility, trackers: Option<Vec<Uuid>>) -> Scope {
        Scope {
            user_id: Some(id(1)),
            principal_ids: vec![id(1), id(50)], // user 1 belongs to group 50
            grants: vec![ProjectGrant {
                project_id: id(100),
                roles: vec![RoleGrant {
                    visibility: vis,
                    trackers,
                }],
            }],
        }
    }

    #[test]
    fn default_hides_others_private_but_shows_group_assigned() {
        let s = scope(Visibility::Default, None);
        assert!(s.can_see(&facts(false, 2, None, 7)));
        assert!(!s.can_see(&facts(true, 2, None, 7)));
        assert!(
            s.can_see(&facts(true, 2, Some(50), 7)),
            "assigned to my group"
        );
        assert!(s.can_see(&facts(true, 1, None, 7)), "I am the author");
    }

    #[test]
    fn own_ignores_public_issues_of_others() {
        let s = scope(Visibility::Own, None);
        assert!(!s.can_see(&facts(false, 2, None, 7)));
        assert!(s.can_see(&facts(false, 2, Some(1), 7)));
    }

    #[test]
    fn tracker_restriction_and_empty_restriction() {
        assert!(!scope(Visibility::All, Some(vec![id(8)])).can_see(&facts(false, 2, None, 7)));
        assert!(scope(Visibility::All, Some(vec![id(7)])).can_see(&facts(false, 2, None, 7)));
        assert_eq!(
            scope(Visibility::All, Some(vec![])).predicate(),
            Pred::False
        );
    }

    #[test]
    fn roles_are_ored() {
        let mut s = scope(Visibility::Own, None);
        s.grants[0].roles.push(RoleGrant {
            visibility: Visibility::All,
            trackers: Some(vec![id(7)]),
        });
        assert!(
            s.can_see(&facts(true, 2, None, 7)),
            "second role: all, tracker 7"
        );
        assert!(
            !s.can_see(&facts(false, 2, None, 8)),
            "neither role covers tracker 8"
        );
    }

    #[test]
    fn anonymous_only_sees_non_private() {
        let mut s = scope(Visibility::All, None);
        s.user_id = None;
        s.principal_ids.clear();
        assert!(s.can_see(&facts(false, 2, None, 7)));
        assert!(!s.can_see(&facts(true, 2, None, 7)));
    }

    #[test]
    fn other_projects_invisible() {
        let s = scope(Visibility::All, None);
        let mut f = facts(false, 2, None, 7);
        f.project_id = id(101);
        assert!(!s.can_see(&f));
    }
}
