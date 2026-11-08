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
//! It also checks the MCP-facing `service::search` result.
//!
//! Needs PostgreSQL 18. Uses OXLEDGER_TEST_DATABASE_URL, or database
//! `oxledger_test` next to DATABASE_URL / the local docker default (created if
//! missing, see `common::test_pool`).

mod common;

use ox_core::IssueFacts;
use oxledger::db::{self, Caller, push_pred};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App, SearchParams};
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::BTreeSet;
use uuid::Uuid;

const WORLDS: u64 = 40;

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
    for seed in 1..=WORLDS {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
        let w = gen_world(&mut rng);
        load_world(&pool, &w).await;
        let index = SearchIndex::open(None).unwrap();
        index.sync(&pool).await.unwrap();
        let app = App {
            pool: pool.clone(),
            index,
        };

        let viewers: Vec<Option<&User>> = std::iter::once(None)
            .chain(w.users.iter().map(Some))
            .collect();
        for viewer in viewers {
            let caller = Caller(viewer.map(|u| u.id));
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
        }
    }
    // Guard against a vacuous pass (e.g. everything invisible).
    assert!(
        visible_total > checked,
        "worlds too sparse: {visible_total} visible over {checked} viewers"
    );
    eprintln!(
        "E4: {checked} viewer checks over {WORLDS} worlds, {visible_total} visible issue-viewer pairs"
    );
}
