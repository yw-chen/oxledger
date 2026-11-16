// SPDX-License-Identifier: GPL-2.0-or-later
//! E4 prototype: visibility property test.
//!
//! Generates random worlds (projects, users, groups, roles, memberships,
//! issues) and checks, for every viewer, that four answers agree:
//!   1. `reference_visible` — Redmine's rule written out directly from
//!      app/models/issue.rb `visible_condition` + project.rb `allowed_to_condition`,
//!      independent of ox-core;
//!   2. SQL: `load_scope` → `Pred` → WHERE clause;
//!   3. tantivy: the same `Pred` as an index filter;
//!   4. `Scope::can_see` in memory;
//!
//! It also checks the MCP-facing `service::search` result, and (design 05
//! M1.1) `service::search` with random Filter v2 `where` conditions over
//! the visibility inputs (project, tracker, is_private, author, assignee
//! with `me` and groups; eq/ne/in/not_in/is_null/not_null; and/or), with
//! and without `q`, against `reference_visible` AND a reference evaluator.
//!
//! Needs PostgreSQL 18; see `common::test_pool` (the test container, one
//! database per nextest slot, created if missing).

mod common;

use ox_core::IssueFacts;
use oxledger::auth::AuthContext;
use oxledger::db::{self, push_pred};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App, SearchParams};
use serde_json::{Value as Json, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::BTreeSet;
use uuid::Uuid;

const WORLDS: u64 = 40;
/// Random `where` filters per viewer and world.
const WHERES: usize = 4;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

#[derive(Clone)]
struct Role {
    id: Uuid,
    builtin: Option<&'static str>,
    vis: &'static str,
    view: bool,
    /// None = all trackers
    trackers: Option<Vec<Uuid>>,
}
struct Project {
    id: Uuid,
    public: bool,
    status: &'static str,
    module: bool,
}
struct User {
    id: Uuid,
    admin: bool,
}
struct Member {
    project: Uuid,
    principal: Uuid,
    roles: Vec<Uuid>,
}
struct World {
    trackers: Vec<Uuid>,
    users: Vec<User>,
    groups: Vec<(Uuid, Vec<Uuid>)>,
    roles: Vec<Role>,
    projects: Vec<Project>,
    members: Vec<Member>,
    issues: Vec<(Uuid, IssueFacts)>,
}

fn gen_world(rng: &mut Rng) -> World {
    let trackers: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    let users: Vec<User> = (0..5)
        .map(|_| User {
            id: Uuid::now_v7(),
            admin: rng.chance(0.1),
        })
        .collect();
    let groups = (0..2)
        .map(|_| {
            let members = users
                .iter()
                .filter(|_| rng.chance(0.4))
                .map(|u| u.id)
                .collect();
            (Uuid::now_v7(), members)
        })
        .collect::<Vec<_>>();
    let vis = ["all", "default", "own"];
    let role = |rng: &mut Rng, builtin| {
        let trackers = if rng.chance(0.3) {
            Some(
                trackers
                    .iter()
                    .copied()
                    .filter(|_| rng.chance(0.5))
                    .collect(),
            )
        } else {
            None
        };
        Role {
            id: Uuid::now_v7(),
            builtin,
            vis: rng.pick(&vis),
            view: rng.chance(0.85),
            trackers,
        }
    };
    let mut roles: Vec<Role> = (0..3).map(|_| role(rng, None)).collect();
    roles.push(role(rng, Some("non_member")));
    roles.push(role(rng, Some("anonymous")));
    let normal: Vec<Uuid> = roles
        .iter()
        .filter(|r| r.builtin.is_none())
        .map(|r| r.id)
        .collect();
    let statuses = ["active", "active", "closed", "archived"];
    let projects: Vec<Project> = (0..4)
        .map(|_| Project {
            id: Uuid::now_v7(),
            public: rng.chance(0.5),
            status: rng.pick(&statuses),
            module: rng.chance(0.85),
        })
        .collect();
    let principals: Vec<Uuid> = users
        .iter()
        .map(|u| u.id)
        .chain(groups.iter().map(|g| g.0))
        .collect();
    let mut members = Vec::new();
    for p in &projects {
        for pr in &principals {
            if rng.chance(0.35) {
                let mut rs: Vec<Uuid> = vec![*rng.pick(&normal)];
                if rng.chance(0.3) {
                    rs.push(*rng.pick(&normal));
                }
                rs.sort();
                rs.dedup();
                members.push(Member {
                    project: p.id,
                    principal: *pr,
                    roles: rs,
                });
            }
        }
    }
    let issues = (0..30)
        .map(|_| {
            let assignee = match rng.below(3) {
                0 => None,
                _ => Some(*rng.pick(&principals)),
            };
            (
                Uuid::now_v7(),
                IssueFacts {
                    project_id: rng.pick(&projects).id,
                    tracker_id: *rng.pick(&trackers),
                    is_private: rng.chance(0.35),
                    author_id: rng.pick(&users).id,
                    assigned_to_id: assignee,
                },
            )
        })
        .collect();
    World {
        trackers,
        users,
        groups,
        roles,
        projects,
        members,
        issues,
    }
}

/// Redmine's rule, written independently of ox-core.
fn reference_visible(w: &World, viewer: Option<&User>, f: &IssueFacts) -> bool {
    let p = w.projects.iter().find(|p| p.id == f.project_id).unwrap();
    if p.status == "archived" || !p.module {
        return false;
    }
    let tracker_ok = |r: &Role| {
        r.trackers
            .as_ref()
            .is_none_or(|t| t.contains(&f.tracker_id))
    };
    let Some(u) = viewer else {
        let r = w
            .roles
            .iter()
            .find(|r| r.builtin == Some("anonymous"))
            .unwrap();
        return p.public && r.view && tracker_ok(r) && !f.is_private;
    };
    if u.admin {
        return true;
    }
    let mut mine: Vec<Uuid> = vec![u.id];
    mine.extend(w.groups.iter().filter(|g| g.1.contains(&u.id)).map(|g| g.0));
    let memberships: Vec<&Member> = w
        .members
        .iter()
        .filter(|m| m.project == p.id && mine.contains(&m.principal))
        .collect();
    let roles: Vec<&Role> = if !memberships.is_empty() {
        memberships
            .iter()
            .flat_map(|m| m.roles.iter())
            .map(|id| w.roles.iter().find(|r| r.id == *id).unwrap())
            .collect()
    } else if p.public {
        vec![
            w.roles
                .iter()
                .find(|r| r.builtin == Some("non_member"))
                .unwrap(),
        ]
    } else {
        vec![]
    };
    let is_mine = f.author_id == u.id || f.assigned_to_id.is_some_and(|a| mine.contains(&a));
    roles.iter().any(|r| {
        r.view
            && tracker_ok(r)
            && match r.vis {
                "all" => true,
                "default" => !f.is_private || is_mine,
                "own" => is_mine,
                _ => unreachable!(),
            }
    })
}

use common::test_pool;

/// A random v2 `where` and its meaning, written independently of the
/// server: (json, uses `me`, predicate over the facts with "me" = the
/// viewer's id and groups).
type Check = Box<dyn Fn(&IssueFacts, &[Uuid]) -> bool>;

/// One `where` condition over a visibility input.
fn gen_cond(
    rng: &mut Rng,
    w: &World,
    visible_projects: &[Uuid],
    viewer: Option<Uuid>,
) -> (Json, bool, Check) {
    let principals: Vec<Uuid> = w
        .users
        .iter()
        .map(|u| u.id)
        .chain(w.groups.iter().map(|g| g.0))
        .collect();
    let users: Vec<Uuid> = w.users.iter().map(|u| u.id).collect();
    // A list of 1–3 ids from `from`, as JSON and as a set.
    let some = |rng: &mut Rng, from: &[Uuid]| -> Vec<Uuid> {
        let mut v: Vec<Uuid> = (0..1 + rng.below(3)).map(|_| *rng.pick(from)).collect();
        v.sort();
        v.dedup();
        v
    };
    let ids = |v: &[Uuid]| json!(v.iter().map(|u| u.to_string()).collect::<Vec<_>>());
    let kind = loop {
        let k = rng.below(5);
        if k != 0 || !visible_projects.is_empty() {
            break k;
        }
    };
    match kind {
        // project / tracker: eq, ne, in, not_in (tracker: is_null too).
        0 | 1 => {
            let (field, from): (&str, Vec<Uuid>) = if kind == 0 {
                ("project", visible_projects.to_vec())
            } else {
                ("tracker", w.trackers.clone())
            };
            let set = some(rng, &from);
            let get = move |f: &IssueFacts| {
                if kind == 0 {
                    f.project_id
                } else {
                    f.tracker_id
                }
            };
            match rng.below(5) {
                0 => {
                    let x = set[0];
                    (
                        json!({"field": field, "op": "eq", "value": x.to_string()}),
                        false,
                        Box::new(move |f, _| get(f) == x),
                    )
                }
                1 => {
                    let x = set[0];
                    (
                        json!({"field": field, "op": "ne", "value": x.to_string()}),
                        false,
                        Box::new(move |f, _| get(f) != x),
                    )
                }
                2 => (
                    json!({"field": field, "op": "in", "value": ids(&set)}),
                    false,
                    Box::new(move |f, _| set.contains(&get(f))),
                ),
                3 => (
                    json!({"field": field, "op": "not_in", "value": ids(&set)}),
                    false,
                    Box::new(move |f, _| !set.contains(&get(f))),
                ),
                _ if kind == 1 => (
                    json!({"field": field, "op": "is_null"}),
                    false,
                    Box::new(|_, _| false),
                ),
                _ => (
                    json!({"field": field, "op": "in", "value": ids(&set)}),
                    false,
                    Box::new(move |f, _| set.contains(&get(f))),
                ),
            }
        }
        2 => {
            let b = rng.chance(0.5);
            (
                json!({"field": "is_private", "op": "eq", "value": b}),
                false,
                Box::new(move |f, _| f.is_private == b),
            )
        }
        // author: ids or `me` (alone: me is also created_on_behalf_of, null here).
        3 => {
            let me = rng.chance(0.4);
            let set = some(rng, &users);
            let pos = rng.chance(0.5);
            let op = match (pos, rng.chance(0.5)) {
                (true, true) => "eq",
                (true, false) => "in",
                (false, true) => "ne",
                (false, false) => "not_in",
            };
            let value = match (me, op) {
                (true, "eq" | "ne") => json!({"me": true}),
                (true, _) => json!([{"me": true}]),
                (false, "eq" | "ne") => json!(set[0].to_string()),
                (false, _) => ids(&set),
            };
            let one = matches!(op, "eq" | "ne");

            (
                json!({"field": "author", "op": op, "value": value}),
                me,
                Box::new(move |f, _| {
                    let hit = if me {
                        Some(f.author_id) == viewer
                    } else if one {
                        f.author_id == set[0]
                    } else {
                        set.contains(&f.author_id)
                    };
                    hit == pos
                }),
            )
        }
        // assignee: ids and/or `me` (me + my groups), is_null / not_null.
        _ => {
            let me = rng.chance(0.4);
            let set = some(rng, &principals);
            match rng.below(6) {
                4 => (
                    json!({"field": "assignee", "op": "is_null"}),
                    false,
                    Box::new(|f, _| f.assigned_to_id.is_none()),
                ),
                5 => (
                    json!({"field": "assignee", "op": "not_null"}),
                    false,
                    Box::new(|f, _| f.assigned_to_id.is_some()),
                ),
                k => {
                    let op = ["eq", "ne", "in", "not_in"][k];
                    let one = k < 2;
                    let pos = k % 2 == 0;
                    let value = match (me, one) {
                        (true, true) => json!({"me": true}),
                        (false, true) => json!(set[0].to_string()),
                        (true, false) => {
                            let mut v = vec![json!({"me": true})];
                            v.extend(set.iter().map(|u| json!(u.to_string())));
                            json!(v)
                        }
                        (false, false) => ids(&set),
                    };
                    (
                        json!({"field": "assignee", "op": op, "value": value}),
                        me,
                        Box::new(move |f, mine| {
                            let Some(a) = f.assigned_to_id else {
                                return !pos;
                            };
                            let hit = (me && mine.contains(&a))
                                || (!(me && one)
                                    && if one { a == set[0] } else { set.contains(&a) });
                            hit == pos
                        }),
                    )
                }
            }
        }
    }
}

/// A random `where`: a condition, or and/or of 2–3 parts, two levels deep at most.
fn gen_where(
    rng: &mut Rng,
    w: &World,
    vp: &[Uuid],
    viewer: Option<Uuid>,
    depth: usize,
) -> (Json, bool, Check) {
    if depth >= 2 || rng.chance(0.4) {
        return gen_cond(rng, w, vp, viewer);
    }
    let and = rng.chance(0.5);
    let parts: Vec<(Json, bool, Check)> = (0..2 + rng.below(2))
        .map(|_| gen_where(rng, w, vp, viewer, depth + 1))
        .collect();
    let me = parts.iter().any(|p| p.1);
    let json = json!({ (if and { "and" } else { "or" }): parts.iter().map(|p| p.0.clone()).collect::<Vec<_>>() });
    let checks: Vec<Check> = parts.into_iter().map(|p| p.2).collect();
    (
        json,
        me,
        Box::new(move |f, mine| {
            if and {
                checks.iter().all(|c| c(f, mine))
            } else {
                checks.iter().any(|c| c(f, mine))
            }
        }),
    )
}

async fn load_world(pool: &PgPool, w: &World) {
    sqlx::query(
        "TRUNCATE outbox, outbox_cursors, issue_keys, issues, workflow_transitions, project_trackers,
                  member_roles, members, role_permission_trackers, role_permissions, roles, enabled_modules,
                  projects, api_keys, group_members, aliases, principals, trackers, issue_statuses CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    let status: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    for (i, t) in w.trackers.iter().enumerate() {
        sqlx::query("INSERT INTO trackers (id, name) VALUES ($1, $2)")
            .bind(t)
            .bind(format!("T{i}"))
            .execute(pool)
            .await
            .unwrap();
    }
    for (i, u) in w.users.iter().enumerate() {
        sqlx::query(
            "INSERT INTO principals (id, kind, login, name, is_admin) VALUES ($1,'user',$2,$2,$3)",
        )
        .bind(u.id)
        .bind(format!("u{i}"))
        .bind(u.admin)
        .execute(pool)
        .await
        .unwrap();
    }
    for (i, (g, members)) in w.groups.iter().enumerate() {
        sqlx::query("INSERT INTO principals (id, kind, name) VALUES ($1,'group',$2)")
            .bind(g)
            .bind(format!("g{i}"))
            .execute(pool)
            .await
            .unwrap();
        for m in members {
            sqlx::query("INSERT INTO group_members VALUES ($1,$2)")
                .bind(g)
                .bind(m)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    for (i, r) in w.roles.iter().enumerate() {
        sqlx::query(
            "INSERT INTO roles (id, name, builtin, issues_visibility) VALUES ($1,$2,$3,$4)",
        )
        .bind(r.id)
        .bind(format!("r{i}"))
        .bind(r.builtin)
        .bind(r.vis)
        .execute(pool)
        .await
        .unwrap();
        if r.view {
            sqlx::query("INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ($1,'view_issues',$2)")
                .bind(r.id).bind(r.trackers.is_none()).execute(pool).await.unwrap();
            for t in r.trackers.iter().flatten() {
                sqlx::query("INSERT INTO role_permission_trackers VALUES ($1,'view_issues',$2)")
                    .bind(r.id)
                    .bind(t)
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
    }
    for (i, p) in w.projects.iter().enumerate() {
        sqlx::query(
            "INSERT INTO projects (id, key, name, is_public, status) VALUES ($1,$2,$2,$3,$4)",
        )
        .bind(p.id)
        .bind(format!("P{i}X"))
        .bind(p.public)
        .bind(p.status)
        .execute(pool)
        .await
        .unwrap();
        if p.module {
            sqlx::query("INSERT INTO enabled_modules VALUES ($1, 'issue_tracking')")
                .bind(p.id)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    for m in &w.members {
        let mid: Uuid = sqlx::query_scalar(
            "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(m.project)
        .bind(m.principal)
        .fetch_one(pool)
        .await
        .unwrap();
        for r in &m.roles {
            sqlx::query("INSERT INTO member_roles VALUES ($1,$2)")
                .bind(mid)
                .bind(r)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    for (n, (id, f)) in w.issues.iter().enumerate() {
        sqlx::query(
            "INSERT INTO issues (id, project_id, tracker_id, status_id, subject, author_id, assigned_to_id, is_private)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(id).bind(f.project_id).bind(f.tracker_id).bind(status).bind(format!("issue {n}"))
        .bind(f.author_id).bind(f.assigned_to_id).bind(f.is_private)
        .execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ($1,$2,$3,$4)",
        )
        .bind(format!("K-{n}"))
        .bind(id)
        .bind(f.project_id)
        .bind(n as i32)
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn e4_visibility_sql_index_memory_agree_with_reference() {
    let pool = test_pool().await;
    let mut checked = 0usize;
    let mut visible_total = 0usize;
    let (mut where_checked, mut where_hits) = (0usize, 0usize);
    for seed in 1..=WORLDS {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
        let w = gen_world(&mut rng);
        load_world(&pool, &w).await;
        let index = SearchIndex::open(None).unwrap();
        index.sync_all(&pool).await.unwrap();
        let app = App {
            pool: pool.clone(),
            index,
            wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
        };

        let viewers: Vec<Option<&User>> = std::iter::once(None)
            .chain(w.users.iter().map(Some))
            .collect();
        for viewer in viewers {
            let caller = AuthContext::test_viewer(viewer.map(|u| u.id));
            let expected: BTreeSet<Uuid> = w
                .issues
                .iter()
                .filter(|(_, f)| reference_visible(&w, viewer, f))
                .map(|(id, _)| *id)
                .collect();

            let scope = db::load_scope(&mut app.pool.acquire().await.unwrap(), caller)
                .await
                .unwrap();
            let pred = scope.predicate();

            let mut qb: QueryBuilder<Postgres> =
                QueryBuilder::new("SELECT i.id FROM issues i WHERE ");
            push_pred(&mut qb, "i", &pred);
            let sql: BTreeSet<Uuid> = qb
                .build_query_scalar()
                .fetch_all(&app.pool)
                .await
                .unwrap()
                .into_iter()
                .collect();

            let idx: BTreeSet<Uuid> = app
                .index
                .search("", &pred, 10_000)
                .unwrap()
                .into_iter()
                .map(|h| h.issue_id)
                .collect();
            let mem: BTreeSet<Uuid> = w
                .issues
                .iter()
                .filter(|(_, f)| scope.can_see(f))
                .map(|(id, _)| *id)
                .collect();
            let svc = service::search(
                &app,
                caller,
                SearchParams {
                    limit: Some(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let svc: BTreeSet<Uuid> = svc.items.iter().map(|i| i.id).collect();

            let who = viewer
                .map(|u| format!("user {} admin={}", u.id, u.admin))
                .unwrap_or("anonymous".into());
            assert_eq!(
                sql, expected,
                "seed {seed}, {who}: SQL disagrees with reference\npred = {pred:?}"
            );
            assert_eq!(
                idx, expected,
                "seed {seed}, {who}: index disagrees with reference\npred = {pred:?}"
            );
            assert_eq!(
                mem, expected,
                "seed {seed}, {who}: memory disagrees with reference"
            );
            assert_eq!(
                svc, expected,
                "seed {seed}, {who}: service::search disagrees with reference"
            );
            checked += 1;
            visible_total += expected.len();

            // Filter v2: visible AND where, with and without q.
            let visible_projects: Vec<Uuid> = scope.project_ids().collect();
            let mine: Vec<Uuid> = viewer
                .map(|u| {
                    std::iter::once(u.id)
                        .chain(w.groups.iter().filter(|g| g.1.contains(&u.id)).map(|g| g.0))
                        .collect()
                })
                .unwrap_or_default();
            for _ in 0..WHERES {
                let (wj, uses_me, check) =
                    gen_where(&mut rng, &w, &visible_projects, viewer.map(|u| u.id), 0);
                let want: BTreeSet<Uuid> = w
                    .issues
                    .iter()
                    .filter(|(id, f)| expected.contains(id) && check(f, &mine))
                    .map(|(id, _)| *id)
                    .collect();
                for q in [None, Some("issue")] {
                    let r = service::search(
                        &app,
                        caller,
                        SearchParams {
                            q: q.map(String::from),
                            where_: Some(wj.clone()),
                            limit: Some(100),
                            ..Default::default()
                        },
                    )
                    .await;
                    if uses_me && viewer.is_none() {
                        let e = r.expect_err("anonymous has no me");
                        let a = oxledger::error::classify(&e).unwrap();
                        assert_eq!(
                            a.details.unwrap()["reason"],
                            "me_unavailable",
                            "seed {seed}, {who}: {wj}"
                        );
                        continue;
                    }
                    let got: BTreeSet<Uuid> = r
                        .unwrap_or_else(|e| panic!("seed {seed}, {who}, q={q:?}: {wj}: {e:#}"))
                        .items
                        .iter()
                        .map(|i| i.id)
                        .collect();
                    assert_eq!(got, want, "seed {seed}, {who}, q={q:?}: where {wj}");
                    where_checked += 1;
                    where_hits += want.len();
                }
            }
        }
    }
    assert!(
        where_hits > where_checked,
        "where checks too sparse: {where_hits} hits over {where_checked} searches"
    );
    // Guard against a vacuous pass (e.g. everything invisible).
    assert!(
        visible_total > checked,
        "worlds too sparse: {visible_total} visible over {checked} viewers"
    );
    eprintln!(
        "E4: {checked} viewer checks over {WORLDS} worlds, {visible_total} visible issue-viewer pairs; \
         {where_checked} v2 where searches, {where_hits} hits"
    );
}
