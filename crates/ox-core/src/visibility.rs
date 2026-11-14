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

    // ------------------------------------------------------------ properties (09b G13)
    //
    // Ids come from a small range so authors, assignees, groups, trackers
    // and projects collide often. Failures are kept in
    // crates/ox-core/proptest-regressions/visibility.txt (committed).

    use proptest::prelude::*;

    fn small_id() -> impl Strategy<Value = Uuid> {
        (1u128..=4).prop_map(id)
    }

    fn arb_facts() -> impl Strategy<Value = IssueFacts> {
        (
            small_id(),
            small_id(),
            any::<bool>(),
            small_id(),
            proptest::option::of(small_id()),
        )
            .prop_map(|(p, t, private, a, asg)| IssueFacts {
                project_id: p,
                tracker_id: t,
                is_private: private,
                author_id: a,
                assigned_to_id: asg,
            })
    }

    fn arb_pred() -> impl Strategy<Value = Pred> {
        let leaf = prop_oneof![
            Just(Pred::True),
            Just(Pred::False),
            Just(Pred::NotPrivate),
            small_id().prop_map(Pred::ProjectIs),
            small_id().prop_map(Pred::AuthorIs),
            proptest::collection::vec(small_id(), 0..3).prop_map(Pred::TrackerIn),
            proptest::collection::vec(small_id(), 0..3).prop_map(Pred::AssigneeIn),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Pred::And),
                proptest::collection::vec(inner, 0..4).prop_map(Pred::Or),
            ]
        })
    }

    fn arb_role() -> impl Strategy<Value = RoleGrant> {
        (
            prop_oneof![
                Just(Visibility::All),
                Just(Visibility::Default),
                Just(Visibility::Own)
            ],
            proptest::option::of(proptest::collection::vec(small_id(), 0..3)),
        )
            .prop_map(|(visibility, trackers)| RoleGrant {
                visibility,
                trackers,
            })
    }

    fn arb_scope() -> impl Strategy<Value = Scope> {
        (
            proptest::option::of(small_id()),
            proptest::collection::vec(small_id(), 0..3),
            proptest::collection::vec(
                (small_id(), proptest::collection::vec(arb_role(), 0..3))
                    .prop_map(|(project_id, roles)| ProjectGrant { project_id, roles }),
                0..3,
            ),
        )
            .prop_map(|(user, groups, grants)| match user {
                None => Scope::anonymous(grants),
                Some(me) => {
                    let mut principal_ids = vec![me];
                    principal_ids.extend(groups);
                    Scope {
                        user_id: Some(me),
                        principal_ids,
                        grants,
                    }
                }
            })
    }

    /// The rules of the module docs, written out directly (no `Pred`).
    fn reference_can_see(s: &Scope, f: &IssueFacts) -> bool {
        s.grants.iter().any(|g| {
            g.project_id == f.project_id
                && g.roles.iter().any(|r| {
                    let tracker_ok = r
                        .trackers
                        .as_ref()
                        .is_none_or(|t| t.contains(&f.tracker_id));
                    let mine = || {
                        s.user_id == Some(f.author_id)
                            || f.assigned_to_id
                                .is_some_and(|a| s.principal_ids.contains(&a))
                    };
                    let mode_ok = match (s.user_id, r.visibility) {
                        (None, _) => !f.is_private,
                        (Some(_), Visibility::All) => true,
                        (Some(_), Visibility::Default) => !f.is_private || mine(),
                        (Some(_), Visibility::Own) => mine(),
                    };
                    tracker_ok && mode_ok
                })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

        /// `simplify` never changes what a predicate selects.
        #[test]
        fn prop_simplify_keeps_meaning(p in arb_pred(), f in arb_facts()) {
            prop_assert_eq!(p.clone().simplify().eval(&f), p.eval(&f));
        }

        /// `simplify` is idempotent.
        #[test]
        fn prop_simplify_is_idempotent(p in arb_pred()) {
            let once = p.simplify();
            prop_assert_eq!(once.clone().simplify(), once);
        }

        /// `can_see` is exactly the Redmine rules of the module docs.
        #[test]
        fn prop_can_see_follows_the_rules(s in arb_scope(), f in arb_facts()) {
            prop_assert_eq!(s.can_see(&f), reference_can_see(&s, &f));
        }

        /// Anonymous callers never see a private issue; another role never
        /// hides an issue (roles only add).
        #[test]
        fn prop_anonymous_and_monotonic(s in arb_scope(), f in arb_facts(), extra in arb_role()) {
            if s.user_id.is_none() && f.is_private {
                prop_assert!(!s.can_see(&f));
            }
            let mut more = s.clone();
            match more.grants.iter_mut().find(|g| g.project_id == f.project_id) {
                Some(g) => g.roles.push(extra),
                None => more.grants.push(ProjectGrant { project_id: f.project_id, roles: vec![extra] }),
            }
            prop_assert!(!s.can_see(&f) || more.can_see(&f));
        }
    }
}
