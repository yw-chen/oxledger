// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.9: links, the development tab, hiding and revoking, the
//! administrator's switch, and `get(include: ["dev"])`: T-G5 (unlink),
//! T-G9, T-G10, T-G11, T-G12, T-G14, T-G17, T-G18, T-G23, T-G24 (epoch),
//! T-G27, T-G28 (DB level, B15), T-G29, T-PT17. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use chrono::{DateTime, Utc};
use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::error::ErrorCode;
use oxledger::github::client::{Fut, GithubApi, HookDelivery};
use oxledger::github::links::{self, AdminGithubPut, AdminRepoSetting, LinkInput, RevokeInput};
use oxledger::github::rollup::Review;
use oxledger::github::{jobs, read, webhook};
use oxledger::issue_view::{self, GetParams};
use oxledger::project_tabs;
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use world::*;

const INST: i64 = 61;
const REPO: i64 = 7001;

/// Commits and reviews are empty; refs come from titles.
struct Quiet;

impl GithubApi for Quiet {
    fn get_pull<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Option<Value>> {
        Box::pin(async { Ok(None) })
    }
    fn list_pulls<'a>(
        &'a self,
        _: i64,
        _: &'a str,
        _: &'a str,
        _: Option<DateTime<Utc>>,
    ) -> Fut<'a, Vec<Value>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn list_pull_commits<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Vec<String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn list_reviews<'a>(&'a self, _: i64, _: &'a str, _: i32) -> Fut<'a, Vec<Review>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn get_check_suite<'a>(&'a self, _: i64, _: &'a str, _: i64) -> Fut<'a, Option<Value>> {
        Box::pin(async { Ok(None) })
    }
    fn get_check_run<'a>(&'a self, _: i64, _: &'a str, _: i64) -> Fut<'a, Option<Value>> {
        Box::pin(async { Ok(None) })
    }
    fn checks_for_ref<'a>(
        &'a self,
        _: i64,
        _: &'a str,
        _: &'a str,
    ) -> Fut<'a, (Vec<Value>, Vec<Value>, Vec<Value>)> {
        Box::pin(async { Ok((vec![], vec![], vec![])) })
    }
    fn list_installations(&self) -> Fut<'_, Vec<(i64, String, bool)>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn list_installation_repos(&self, _: i64) -> Fut<'_, Vec<Value>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn list_hook_deliveries(&self, _: DateTime<Utc>) -> Fut<'_, Vec<HookDelivery>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn redeliver(&self, _: i64) -> Fut<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct Env {
    w: World,
    /// Manager of AUTH and WEB who sees everything (Dev + Mgr).
    alice: AuthContext,
    /// Manager of AUTH who does not see private issues.
    carol: AuthContext,
    /// System administrator, no membership.
    admin: AuthContext,
    bob: AuthContext,
    repo: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> Env {
    let w = World::build(pool).await;
    faults::clear();
    exec(
        pool,
        "TRUNCATE github_event_jobs, github_deliveries, pr_issue_refs, project_pr_hidden,
                  github_commit_statuses, github_check_suites, github_check_runs,
                  github_pull_requests, project_repo_links, github_repos, github_installations CASCADE",
    )
    .await;
    let mgr = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(pool, mgr, &["view_issues", "manage_project"]).await;
    let mgr2 = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr2','default') RETURNING id",
    )
    .await;
    grant(pool, mgr2, &["view_issues", "manage_project"]).await;
    // alice: manager in both; carol (WEB Dev) manages AUTH without private issues.
    let m: Uuid =
        sqlx::query_scalar("SELECT id FROM members WHERE project_id = $1 AND principal_id = $2")
            .bind(w.auth_p)
            .bind(w.alice)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(mgr)
        .execute(pool)
        .await
        .unwrap();
    let m: Uuid =
        sqlx::query_scalar("SELECT id FROM members WHERE project_id = $1 AND principal_id = $2")
            .bind(w.web)
            .bind(w.alice)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(mgr)
        .execute(pool)
        .await
        .unwrap();
    member(pool, w.auth_p, w.carol, mgr2).await;
    auth::create_user(pool, "dave", "dave", PW, true)
        .await
        .unwrap();
    let inst: Uuid = sqlx::query_scalar(
        "INSERT INTO github_installations (gh_installation_id, account_login, state)
         VALUES ($1, 'acme', 'active') RETURNING id",
    )
    .bind(INST)
    .fetch_one(pool)
    .await
    .unwrap();
    let repo: Uuid = sqlx::query_scalar(
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
         VALUES ($1, $2, 'acme/app', true, true, 'accessible') RETURNING id",
    )
    .bind(REPO)
    .bind(inst)
    .fetch_one(pool)
    .await
    .unwrap();
    Env {
        alice: sign_in(pool, &w.policy, "alice").await,
        carol: sign_in(pool, &w.policy, "carol").await,
        admin: sign_in(pool, &w.policy, "dave").await,
        bob: sign_in(pool, &w.policy, "bob").await,
        w,
        repo,
    }
}

async fn issue(e: &Env, project: Uuid, private: bool) -> (Uuid, String) {
    create_issue(
        &e.w.pool,
        NewIssue {
            project_id: project,
            tracker_id: e.w.bug,
            status_id: e.w.new,
            subject: "work",
            description: "",
            author_id: e.w.bob,
            assigned_to_id: None,
            is_private: private,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

fn key_of(e: &Env, p: Uuid) -> &'static str {
    if p == e.w.auth_p { "AUTH" } else { "WEB" }
}

async fn link_to(e: &Env, project: Uuid, repo: Uuid) -> Uuid {
    links::link(
        &e.w.app,
        e.alice,
        Uuid::now_v7(),
        key_of(e, project),
        &LinkInput { repo_id: repo },
    )
    .await
    .unwrap()
    .link_id
}

fn pr_payload(n: i32, title: &str, state: &str, merged: bool) -> Value {
    json!({"action": "edited", "installation": {"id": INST},
           "repository": {"id": REPO, "full_name": "acme/app", "private": true},
           "pull_request": {"number": n, "title": title, "head": {"ref": "b", "sha": format!("s{n}")},
             "state": state, "draft": false,
             "merged_at": if merged { json!("2026-11-02T00:00:00Z") } else { Value::Null },
             "html_url": format!("https://github.com/acme/app/pull/{n}"),
             "updated_at": format!("2026-11-0{}T00:00:00Z", if merged { 2 } else { 1 }),
             "requested_reviewers": [], "requested_teams": []}})
}

async fn send_pr(app: &App, n: i32, title: &str, state: &str, merged: bool) {
    webhook::enqueue(
        app,
        Uuid::now_v7(),
        "pull_request",
        &pr_payload(n, title, state, merged),
    )
    .await
    .unwrap();
    jobs::drain(app, &Quiet).await.unwrap();
}

async fn pr_id(e: &Env, n: i32) -> Uuid {
    sqlx::query_scalar("SELECT id FROM github_pull_requests WHERE number = $1")
        .bind(n)
        .fetch_one(&e.w.pool)
        .await
        .unwrap()
}

async fn dev_of(e: &Env, who: AuthContext, key: &str) -> Vec<i32> {
    read::issue_dev(&e.w.app, who, key)
        .await
        .unwrap()
        .prs
        .iter()
        .map(|p| p.number)
        .collect()
}

async fn tab_keys(e: &Env, who: AuthContext, project: &str, filter: &str) -> Vec<String> {
    read::project_dev(&e.w.app, who, project, filter, false, None)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.key)
        .collect()
}

fn get(key: &str) -> GetParams {
    GetParams {
        key: key.into(),
        fields: None,
        citations: None,
        include: Some(vec!["dev".into()]),
        note_limit: None,
        event_limit: None,
    }
}

// ---------------------------------------------------------------- linking

#[tokio::test]
async fn linking_rules() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;

    // T-G10: an unknown id and a private repository not opened for linking.
    let closed: Uuid = sqlx::query_scalar(
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
         SELECT 7002, installation_id, 'acme/secret', true, false, 'accessible' FROM github_repos WHERE id = $1
         RETURNING id",
    )
    .bind(e.repo)
    .fetch_one(&pool)
    .await
    .unwrap();
    // ... and one the App can no longer reach.
    let gone: Uuid = sqlx::query_scalar(
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
         SELECT 7003, installation_id, 'acme/gone', true, true, 'inaccessible' FROM github_repos WHERE id = $1
         RETURNING id",
    )
    .bind(e.repo)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut shapes = Vec::new();
    for r in [Uuid::now_v7(), closed, gone] {
        let err = links::link(
            app,
            e.alice,
            Uuid::now_v7(),
            "AUTH",
            &LinkInput { repo_id: r },
        )
        .await
        .unwrap_err();
        assert_eq!(code(&err), ErrorCode::RepoNotLinkable);
        shapes.push(format!("{err:#}"));
    }
    assert!(shapes.iter().all(|s| *s == shapes[0]), "T-G10: one shape");
    assert_eq!(
        links::linkable(app, e.alice, "AUTH")
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    // Not a manager → forbidden; bob sees the list but cannot link.
    let err = links::link(
        app,
        e.bob,
        Uuid::now_v7(),
        "AUTH",
        &LinkInput { repo_id: e.repo },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);

    // Link; idempotent replay; already linked.
    let op = Uuid::now_v7();
    let l = links::link(app, e.alice, op, "AUTH", &LinkInput { repo_id: e.repo })
        .await
        .unwrap();
    assert_eq!(l.sync_state, "backfilling");
    let again = links::link(app, e.alice, op, "AUTH", &LinkInput { repo_id: e.repo })
        .await
        .unwrap();
    assert_eq!(again.link_id, l.link_id, "replay");
    let err = links::link(
        app,
        e.alice,
        Uuid::now_v7(),
        "AUTH",
        &LinkInput { repo_id: e.repo },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::RepoAlreadyLinked);
    let listed = links::list(app, e.bob, "AUTH").await.unwrap();
    assert_eq!((listed.items.len(), listed.can_manage), (1, false));
    assert!(
        links::linkable(app, e.alice, "AUTH")
            .await
            .unwrap()
            .items
            .is_empty()
    );

    // T-G27: 19 links, two managers at once.
    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
                 SELECT $2, installation_id, 'acme/r' || $2, false, true, 'accessible' FROM github_repos WHERE id = $1
                 RETURNING id",
            )
            .bind(e.repo)
            .bind(8000 + n as i64)
            .fetch_one(&pool)
            .await
            .unwrap(),
        );
    }
    for r in &ids[..18] {
        link_to(&e, e.w.auth_p, *r).await;
    }
    assert_eq!(
        links::list(app, e.alice, "AUTH").await.unwrap().items.len(),
        19
    );
    let g = Arc::new(Gate::default());
    faults::arm("github.link.after_settings_lock", Action::Block(g.clone()));
    let (a1, a2) = (app.clone(), app.clone());
    let (alice, carol) = (e.alice, e.carol);
    let (r1, r2) = (ids[18], ids[19]);
    let first = tokio::spawn(async move {
        links::link(
            &a1,
            alice,
            Uuid::now_v7(),
            "AUTH",
            &LinkInput { repo_id: r1 },
        )
        .await
    });
    g.reached.notified().await;
    let second = tokio::spawn(async move {
        links::link(
            &a2,
            carol,
            Uuid::now_v7(),
            "AUTH",
            &LinkInput { repo_id: r2 },
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    g.release.notify_one();
    let (x, y) = (first.await.unwrap(), second.await.unwrap());
    assert!(x.is_ok(), "the first one holds the settings lock");
    assert_eq!(code(&y.unwrap_err()), ErrorCode::RepoLinkLimit, "T-G27");
    assert_eq!(
        links::list(app, e.alice, "AUTH").await.unwrap().items.len(),
        20
    );

    // Admin switch; T-G23: closed for linking and unreachable, cleanup still works.
    let v = links::admin_get(app, e.admin).await.unwrap();
    assert_eq!(v.installations.len(), 1);
    assert_eq!(
        code(&links::admin_get(app, e.alice).await.unwrap_err()),
        ErrorCode::ForbiddenOp
    );
    links::admin_put(
        app,
        e.admin,
        &AdminGithubPut {
            repos: vec![AdminRepoSetting {
                repo_id: e.repo,
                linkable: false,
            }],
        },
    )
    .await
    .unwrap();
    exec(&pool, "UPDATE github_repos SET state = 'inaccessible'").await;
    // T-G11: link kept, shown as unreachable.
    let shown = links::list(app, e.alice, "AUTH").await.unwrap();
    assert!(shown.items.iter().all(|l| l.repo_state == "inaccessible"));
    let u = links::unlink(app, e.alice, Uuid::now_v7(), "AUTH", l.link_id)
        .await
        .unwrap();
    assert!(u.changed, "T-G23: unlink works");
    let u2 = links::unlink(app, e.alice, Uuid::now_v7(), "AUTH", l.link_id)
        .await
        .unwrap();
    assert!(!u2.changed);
    // T-G28 (B15): refs go with the issue at the database level.
    let del: String = sqlx::query_scalar(
        "SELECT confdeltype::text FROM pg_constraint
         WHERE conrelid = 'pr_issue_refs'::regclass AND confrelid = 'issues'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(del, "c", "T-G28: ON DELETE CASCADE");
}

// ---------------------------------------------------------------- reading

#[tokio::test]
async fn visibility_and_hiding() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let (a1, k1) = issue(&e, e.w.auth_p, false).await;
    let (_, k50) = issue(&e, e.w.auth_p, true).await;
    let (_, w2) = issue(&e, e.w.web, false).await;
    let ctx = e.w.new_context().await;
    e.w.delegate(e.w.auth_p).await.unwrap();
    let (_t, agent) = e.w.token(ctx).await;
    let auth_link = link_to(&e, e.w.auth_p, e.repo).await;
    let web_link = link_to(&e, e.w.web, e.repo).await;

    send_pr(app, 1, &format!("{k1} and {w2}"), "open", false).await;
    send_pr(app, 2, &format!("only {k50}"), "open", false).await;
    send_pr(app, 3, &format!("{k50} with {w2}"), "open", false).await;
    let (p1, p2, p3) = (pr_id(&e, 1).await, pr_id(&e, 2).await, pr_id(&e, 3).await);

    // T-G9: carol (AUTH manager, no private issues) sees no trace of AUTH-50.
    assert_eq!(dev_of(&e, e.alice, &k50).await, vec![2, 3]);
    let err = read::issue_dev(app, e.carol, &k50).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    assert_eq!(tab_keys(&e, e.carol, "AUTH", "all").await, vec![k1.clone()]);
    let mut all = tab_keys(&e, e.alice, "AUTH", "all").await;
    all.sort();
    let mut want = vec![k1.clone(), k50.clone()];
    want.sort();
    assert_eq!(all, want);

    // T-G29: carol hides/revokes a PR only on AUTH-50 → not_found, same as none.
    let none = links::hide(app, e.carol, Uuid::now_v7(), "AUTH", Uuid::now_v7(), true)
        .await
        .unwrap_err();
    for pr in [p2, p3] {
        let err = links::hide(app, e.carol, Uuid::now_v7(), "AUTH", pr, true)
            .await
            .unwrap_err();
        assert_eq!(code(&err), ErrorCode::NotFound);
        assert_eq!(format!("{err:#}"), format!("{none:#}"), "T-G29");
        let err = links::revoke(
            app,
            e.carol,
            &e.w.policy,
            Uuid::now_v7(),
            "AUTH",
            &RevokeInput {
                link_id: None,
                pr_ids: Some(vec![pr]),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code(&err), ErrorCode::NotFound);
    }

    // T-G18: AUTH hides PR 1; AUTH-1 loses it, WEB-2 keeps it; get(include dev) too.
    let h = links::hide(app, e.alice, Uuid::now_v7(), "AUTH", p1, true)
        .await
        .unwrap();
    assert!(h.changed);
    let h = links::hide(app, e.alice, Uuid::now_v7(), "AUTH", p1, true)
        .await
        .unwrap();
    assert!(!h.changed, "already hidden: no event");
    assert!(dev_of(&e, e.alice, &k1).await.is_empty());
    assert_eq!(dev_of(&e, e.alice, &w2).await, vec![1, 3]);
    let v = issue_view::get(app, agent, get(&k1)).await.unwrap();
    assert!(v.dev.unwrap().prs.is_empty(), "T-G18 MCP get");
    let v = issue_view::get(app, e.alice, get(&w2)).await.unwrap();
    assert_eq!(v.dev.unwrap().prs.len(), 2);
    // Managers can list hidden ones; others may not ask.
    let hidden = read::project_dev(app, e.alice, "AUTH", "all", true, None)
        .await
        .unwrap();
    assert!(
        hidden
            .items
            .iter()
            .any(|i| i.key == k1 && i.prs.iter().any(|p| p.hidden))
    );
    assert_eq!(
        code(
            &read::project_dev(app, e.bob, "AUTH", "all", true, None)
                .await
                .unwrap_err()
        ),
        ErrorCode::ForbiddenOp
    );
    // T-PT17: the hide event is listed for the administrator only.
    let kinds =
        |v: project_tabs::SettingEvents| v.items.into_iter().map(|i| i.kind).collect::<Vec<_>>();
    let mine = kinds(
        project_tabs::events(app, e.alice, "AUTH", None)
            .await
            .unwrap(),
    );
    assert!(!mine.contains(&"pr_hidden".to_string()) && mine.contains(&"repo_linked".to_string()));
    let adm = kinds(
        project_tabs::events(app, e.admin, "AUTH", None)
            .await
            .unwrap(),
    );
    assert!(adm.contains(&"pr_hidden".to_string()), "T-PT17");
    links::hide(app, e.alice, Uuid::now_v7(), "AUTH", p1, false)
        .await
        .unwrap();
    assert_eq!(dev_of(&e, e.alice, &k1).await, vec![1]);

    // T-G14: the agent's get on an issue outside the delegation → not_found, no dev.
    let err = issue_view::get(app, agent, get(&w2)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    let v = issue_view::get(app, agent, get(&k1)).await.unwrap();
    assert_eq!(v.dev.unwrap().prs.len(), 1);

    // T-G12: merged while the issue is in progress: a hint, nothing written.
    exec(
        &pool,
        &format!(
            "UPDATE issues SET status_id = '{}' WHERE id = '{a1}'",
            e.w.doing
        ),
    )
    .await;
    let before: (i32, i64) = sqlx::query_as(
        "SELECT lock_version, (SELECT count(*) FROM issue_events WHERE issue_id = $1) FROM issues WHERE id = $1",
    )
    .bind(a1)
    .fetch_one(&pool)
    .await
    .unwrap();
    send_pr(app, 1, &format!("{k1} and {w2}"), "closed", true).await;
    let d = read::issue_dev(app, e.alice, &k1).await.unwrap();
    assert!(d.merged_open, "T-G12 hint");
    assert_eq!(
        tab_keys(&e, e.alice, "AUTH", "merged_open").await,
        vec![k1.clone()]
    );
    let after: (i32, i64) = sqlx::query_as(
        "SELECT lock_version, (SELECT count(*) FROM issue_events WHERE issue_id = $1) FROM issues WHERE id = $1",
    )
    .bind(a1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after, "R-G4: the issue is untouched");

    // T-G5 (unlink): WEB's unlink removes only WEB refs.
    links::unlink(app, e.alice, Uuid::now_v7(), "WEB", web_link)
        .await
        .unwrap();
    assert!(dev_of(&e, e.alice, &w2).await.is_empty());
    assert_eq!(dev_of(&e, e.alice, &k1).await, vec![1]);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM pr_issue_refs WHERE link_id = '{web_link}'")
        )
        .await,
        0
    );

    // T-G24: revoking a PR bumps AUTH's epoch (the agent's context is
    // quarantined) and redacts AUTH's proposal contents (§7.5, S7).
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(a1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let p = oxledger::changesets::propose(
        &e.w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(a1, lv, "from the PR title"),
        ORIGIN,
    )
    .await
    .unwrap();
    let before = epoch(&pool, e.w.auth_p).await;
    let r = links::revoke(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &RevokeInput {
            link_id: None,
            pr_ids: Some(vec![p1]),
        },
    )
    .await
    .unwrap();
    assert_eq!(r.security_epoch, before + 1);
    let err = issue_view::get(app, agent, get(&k1)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::ContextUnavailable, "T-G24");
    let state: String = sqlx::query_scalar(
        "SELECT pc.state FROM changeset_versions v JOIN proposal_contents pc ON pc.id = v.content_id WHERE v.changeset_id = $1",
    )
    .bind(p.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "redacted", "T-G24 derived copies");
    assert!(
        dev_of(&e, e.alice, &k1).await.is_empty(),
        "revoked PR is hidden"
    );
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM project_setting_events WHERE kind = 'github_content_revoked'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !payload.to_string().contains("and"),
        "no title in the event: {payload}"
    );
    // Admin revokes the whole link (also PRs on issues alice could not see).
    let r = links::revoke(
        app,
        e.admin,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &RevokeInput {
            link_id: Some(auth_link),
            pr_ids: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(r.security_epoch, before + 2);
    assert_eq!(count(&pool, "SELECT count(*) FROM pr_issue_refs").await, 0);
    // Exactly one target.
    let err = links::revoke(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &RevokeInput {
            link_id: None,
            pr_ids: Some(vec![]),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidRequest);
}

// ---------------------------------------------------------------- T-G17

#[tokio::test]
async fn unlink_races_the_worker() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = e.w.app.clone();
    let (_, k1) = issue(&e, e.w.auth_p, false).await;
    let refs = |l: Uuid| {
        let pool = pool.clone();
        async move {
            count(
                &pool,
                &format!("SELECT count(*) FROM pr_issue_refs WHERE link_id = '{l}'"),
            )
            .await
        }
    };

    // The worker holds the link FOR SHARE and builds refs; unlink waits, then removes them.
    let l = link_to(&e, e.w.auth_p, e.repo).await;
    let g = Arc::new(Gate::default());
    faults::arm("github.after_links", Action::Block(g.clone()));
    webhook::enqueue(
        &app,
        Uuid::now_v7(),
        "pull_request",
        &pr_payload(1, &k1, "open", false),
    )
    .await
    .unwrap();
    let a = app.clone();
    let worker = tokio::spawn(async move { jobs::drain(&a, &Quiet).await.unwrap() });
    g.reached.notified().await;
    let (a, alice) = (app.clone(), e.alice);
    let unlink =
        tokio::spawn(async move { links::unlink(&a, alice, Uuid::now_v7(), "AUTH", l).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !unlink.is_finished(),
        "unlink waits for the worker's FOR SHARE"
    );
    g.release.notify_one();
    worker.await.unwrap();
    assert!(unlink.await.unwrap().unwrap().changed);
    assert_eq!(refs(l).await, 0, "worker first");
    assert!(dev_of(&e, e.alice, &k1).await.is_empty());

    // The other way: unlink holds FOR UPDATE; the worker waits and then sees no live link.
    let l2 = link_to(&e, e.w.auth_p, e.repo).await;
    let g = Arc::new(Gate::default());
    faults::arm("github.unlink.after_lock", Action::Block(g.clone()));
    let (a, alice) = (app.clone(), e.alice);
    let unlink =
        tokio::spawn(async move { links::unlink(&a, alice, Uuid::now_v7(), "AUTH", l2).await });
    g.reached.notified().await;
    webhook::enqueue(
        &app,
        Uuid::now_v7(),
        "pull_request",
        &pr_payload(2, &k1, "open", false),
    )
    .await
    .unwrap();
    let a = app.clone();
    let worker = tokio::spawn(async move { jobs::drain(&a, &Quiet).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!worker.is_finished(), "the worker waits for the unlink");
    g.release.notify_one();
    unlink.await.unwrap().unwrap();
    worker.await.unwrap();
    assert_eq!(refs(l2).await, 0, "unlink first");
    let result: String = sqlx::query_scalar(
        "SELECT result FROM github_deliveries ORDER BY received_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(result, "ignored");
}

// ---------------------------------------------------------------- cleanup rights, refs

fn pr_at(n: i32, title: &str, branch: &str, updated: &str) -> Value {
    json!({"action": "edited", "installation": {"id": INST},
           "repository": {"id": REPO, "full_name": "acme/app", "private": true},
           "pull_request": {"number": n, "title": title, "head": {"ref": branch, "sha": format!("s{n}")},
             "state": "open", "draft": false, "merged_at": null,
             "html_url": format!("https://github.com/acme/app/pull/{n}"),
             "updated_at": updated, "requested_reviewers": [], "requested_teams": []}})
}

async fn send(app: &App, v: Value) {
    webhook::enqueue(app, Uuid::now_v7(), "pull_request", &v)
        .await
        .unwrap();
    jobs::drain(app, &Quiet).await.unwrap();
}

async fn payload(pool: &sqlx::PgPool, kind: &str) -> Value {
    sqlx::query_scalar(
        "SELECT payload FROM project_setting_events WHERE kind = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn cleanup_rights_and_refs() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let (_, k1) = issue(&e, e.w.auth_p, false).await;
    let (_, k2) = issue(&e, e.w.auth_p, false).await;
    let l = link_to(&e, e.w.auth_p, e.repo).await;

    // §18.2: event payloads name the repository and the PR.
    let p = payload(&pool, "repo_linked").await;
    assert_eq!(p["full_name"], "acme/app", "{p}");
    assert_eq!(p["link_id"], json!(l));

    // R-G1: a ref follows the title and the branch; old refs go.
    send(app, pr_at(1, &k1, "b", "2026-11-01T00:00:00Z")).await;
    assert_eq!(dev_of(&e, e.alice, &k1).await, vec![1]);
    send(app, pr_at(1, &k2, "b", "2026-11-01T01:00:00Z")).await;
    assert!(dev_of(&e, e.alice, &k1).await.is_empty(), "R-G1 title");
    assert_eq!(dev_of(&e, e.alice, &k2).await, vec![1]);
    send(
        app,
        pr_at(1, "no key", &format!("fix/{k1}"), "2026-11-01T02:00:00Z"),
    )
    .await;
    assert!(dev_of(&e, e.alice, &k2).await.is_empty(), "R-G1 branch");
    assert_eq!(dev_of(&e, e.alice, &k1).await, vec![1]);
    let p1 = pr_id(&e, 1).await;

    // R-G9: at most 20 PRs per card, then `truncated`.
    for n in 2..=21 {
        send(app, pr_at(n, &k1, "b", "2026-11-01T00:00:00Z")).await;
    }
    let d = read::issue_dev(app, e.alice, &k1).await.unwrap();
    assert_eq!((d.prs.len(), d.truncated), (20, true), "R-G9");
    let tab = read::project_dev(app, e.alice, "AUTH", "all", false, None)
        .await
        .unwrap();
    let card = tab.items.iter().find(|i| i.key == k1).unwrap();
    assert_eq!((card.prs.len(), card.truncated), (20, true));

    // An administrator without manage_project hides; the payload names the PR.
    let h = links::hide(app, e.admin, Uuid::now_v7(), "AUTH", p1, true)
        .await
        .unwrap();
    assert!(h.changed, "admin may hide");
    let p = payload(&pool, "pr_hidden").await;
    assert_eq!(
        (p["pr_id"].clone(), p["number"].clone()),
        (json!(p1), json!(1)),
        "{p}"
    );
    assert!(p["repo_id"].is_string());
    // Already hidden: success, no second event.
    let before = count(&pool, "SELECT count(*) FROM project_setting_events").await;
    let h = links::hide(app, e.alice, Uuid::now_v7(), "AUTH", p1, true)
        .await
        .unwrap();
    assert!(!h.changed);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_setting_events").await,
        before,
        "no event when nothing changed"
    );

    // Revoke needs a recent step-up.
    let (_, session) = e.alice.require_human_session().unwrap();
    sqlx::query(
        "UPDATE browser_sessions SET reauthenticated_at = now() - interval '16 minutes' WHERE id = $1",
    )
    .bind(session)
    .execute(&pool)
    .await
    .unwrap();
    let err = links::revoke(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &RevokeInput {
            link_id: None,
            pr_ids: Some(vec![p1]),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::ReauthenticationRequired);

    // A closed (not archived) project can still be cleaned up.
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            e.w.auth_p
        ),
    )
    .await;
    let h = links::hide(app, e.alice, Uuid::now_v7(), "AUTH", p1, false)
        .await
        .unwrap();
    assert!(h.changed, "closed project: hide works");

    // R-G2: the read side drops refs of a link that is no longer live.
    sqlx::query(
        "UPDATE project_repo_links SET unlinked_at = now(), unlinked_by = $2 WHERE id = $1",
    )
    .bind(l)
    .bind(e.w.alice)
    .execute(&pool)
    .await
    .unwrap();
    assert!(count(&pool, "SELECT count(*) FROM pr_issue_refs").await > 0);
    assert!(dev_of(&e, e.alice, &k1).await.is_empty(), "R-G2 card");
    assert!(
        tab_keys(&e, e.alice, "AUTH", "all").await.is_empty(),
        "R-G2 tab"
    );
    let v = issue_view::get(app, e.alice, get(&k1)).await.unwrap();
    assert!(v.dev.unwrap().prs.is_empty(), "R-G2 get");
    exec(
        &pool,
        &format!(
            "UPDATE project_repo_links SET unlinked_at = NULL, unlinked_by = NULL WHERE id = '{l}'"
        ),
    )
    .await;

    // An administrator without manage_project unlinks a closed project's link.
    let u = links::unlink(app, e.admin, Uuid::now_v7(), "AUTH", l)
        .await
        .unwrap();
    assert!(u.changed, "admin may unlink");
    let p = payload(&pool, "repo_unlinked").await;
    assert_eq!(p["full_name"], "acme/app", "{p}");
}
