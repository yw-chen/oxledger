// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.5 (§3.7, R-C1–R-C11): commits from GitHub `push` events,
//! without any network (a fake `GithubApi`): T-M2-60, 61, 62, 63, 64, 65,
//! 65a, 65b, 66 and 78. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::test_pool;
use oxledger::api::{self, ApiConfig};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::error::ErrorCode;
use oxledger::github::client::{Fut, GithubApi, HookDelivery};
use oxledger::github::commits::{COMPARE_LIMIT, PAYLOAD_COMMIT_LIMIT, REDACTED};
use oxledger::github::links::{self, CommitRef, LinkInput, RevokeInput};
use oxledger::github::rollup::Review;
use oxledger::github::{GithubConfig, jobs, read, sync, webhook};
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

const INST: i64 = 61;
const REPO: i64 = 7001;
const SECRET: &[u8] = b"test-webhook-secret";

// ---------------------------------------------------------------- fake GitHub

/// Only the compare API answers with anything.
#[derive(Default)]
struct Fake {
    compare: Mutex<Vec<Value>>,
    compare_calls: Mutex<usize>,
}

impl GithubApi for Fake {
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
    fn compare_commits<'a>(
        &'a self,
        _: i64,
        repo: &'a str,
        before: &'a str,
        after: &'a str,
    ) -> Fut<'a, Vec<Value>> {
        Box::pin(async move {
            assert_eq!(repo, "acme/app");
            assert_eq!((before, after), (sha(0xb), sha(0xa)));
            *self.compare_calls.lock().unwrap() += 1;
            Ok(self.compare.lock().unwrap().clone())
        })
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

// ---------------------------------------------------------------- world

struct Env {
    w: World,
    /// Manager of AUTH and WEB who sees everything.
    alice: AuthContext,
    /// Manager of AUTH who does not see private issues.
    carol: AuthContext,
    /// System administrator, no membership.
    admin: AuthContext,
    /// A member who manages nothing.
    bob: AuthContext,
    repo: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> Env {
    let w = World::build(pool).await;
    faults::clear();
    exec(
        pool,
        "TRUNCATE github_event_jobs, github_deliveries, pr_issue_refs, project_pr_hidden,
                  commit_issue_refs, project_commit_hidden, project_commit_revocations, github_commits,
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
    for project in [w.auth_p, w.web] {
        let m: Uuid = sqlx::query_scalar(
            "SELECT id FROM members WHERE project_id = $1 AND principal_id = $2",
        )
        .bind(project)
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
    }
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

async fn link_to(e: &Env, project: &str) -> Uuid {
    links::link(
        &e.w.app,
        e.alice,
        Uuid::now_v7(),
        project,
        &LinkInput { repo_id: e.repo },
    )
    .await
    .unwrap()
    .link_id
}

/// A 40-character sha from a number.
fn sha(n: u64) -> &'static str {
    Box::leak(format!("{n:040x}").into_boxed_str())
}

fn commit(n: u64, message: &str, distinct: bool) -> Value {
    json!({"id": sha(n), "message": message, "timestamp": "2026-11-01T08:00:00+08:00",
           "url": "https://evil.example/not-used",
           "author": {"name": "M", "email": "m@example.test", "username": "mallory"},
           "distinct": distinct})
}

fn push_to(branch: &str, commits: Vec<Value>) -> Value {
    json!({"ref": format!("refs/heads/{branch}"), "before": sha(0xb), "after": sha(0xa),
           "deleted": false, "installation": {"id": INST},
           "repository": {"id": REPO, "full_name": "acme/app", "private": true, "default_branch": "main"},
           "sender": {"login": "bob"}, "commits": commits})
}

/// Enqueues a `push`, runs the worker, and gives the delivery's result.
async fn deliver(e: &Env, api: &Fake, payload: &Value) -> String {
    let d = Uuid::now_v7();
    assert!(
        webhook::enqueue(&e.w.app, d, "push", payload)
            .await
            .unwrap()
    );
    jobs::drain(&e.w.app, api).await.unwrap();
    sqlx::query_scalar("SELECT result FROM github_deliveries WHERE delivery_id = $1")
        .bind(d)
        .fetch_one(&e.w.pool)
        .await
        .unwrap()
}

/// Keys of the issues a commit links to, in order.
async fn refs_of(e: &Env, n: u64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT k.key FROM commit_issue_refs f JOIN issue_keys k ON k.issue_id = f.issue_id AND k.is_current
         WHERE f.sha = $1 ORDER BY k.key",
    )
    .bind(sha(n))
    .fetch_all(&e.w.pool)
    .await
    .unwrap()
}

type CommitRow = (
    String,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    bool,
);

async fn row(e: &Env, n: u64) -> Option<CommitRow> {
    sqlx::query_as(
        "SELECT message, author_login, sender_login, on_default_branch, first_ref, redacted_at IS NOT NULL
         FROM github_commits WHERE sha = $1",
    )
    .bind(sha(n))
    .fetch_optional(&e.w.pool)
    .await
    .unwrap()
}

/// Shas on the issue's card, as `who` sees it.
async fn card(e: &Env, who: AuthContext, key: &str) -> Vec<String> {
    read::issue_dev(&e.w.app, who, key)
        .await
        .unwrap()
        .commits
        .into_iter()
        .map(|c| c.sha)
        .collect()
}

async fn commits_total(e: &Env) -> i64 {
    count(&e.w.pool, "SELECT count(*) FROM github_commits").await
}

fn one(n: u64) -> CommitRef {
    CommitRef {
        repo_id: Uuid::nil(),
        sha: sha(n).to_string(),
    }
}

fn of(e: &Env, n: u64) -> CommitRef {
    CommitRef {
        repo_id: e.repo,
        ..one(n)
    }
}

async fn revoke(
    e: &Env,
    who: AuthContext,
    project: &str,
    commits: Vec<CommitRef>,
) -> anyhow::Result<links::Revoked> {
    links::revoke(
        &e.w.app,
        who,
        &e.w.policy,
        Uuid::now_v7(),
        project,
        &RevokeInput {
            link_id: None,
            pr_ids: None,
            commits: Some(commits),
        },
    )
    .await
}

// ---------------------------------------------------------------- T-M2-60, 61, 62, 65b

#[tokio::test]
async fn push_stores_commits_and_refs() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let api = Fake::default();
    link_to(&e, "AUTH").await;
    let (_, a1) = issue(&e, e.w.auth_p, false).await;
    let (_, w1) = issue(&e, e.w.web, false).await;

    // T-M2-60: a feature branch; WEB does not link the repository.
    let long = format!("fix login {a1}, see {w1}\n\nbody {}", "x".repeat(5000));
    assert_eq!(
        deliver(
            &e,
            &api,
            &push_to("feat/login", vec![commit(1, &long, true)])
        )
        .await,
        "ok"
    );
    let r = row(&e, 1).await.unwrap();
    assert_eq!(r.0.chars().count(), 4096, "the message is cut");
    // T-M2-65b: the author is what the commit says; the sender is who pushed.
    assert_eq!(
        (r.1.as_deref(), r.2.as_deref()),
        (Some("mallory"), Some("bob"))
    );
    assert!(!r.3, "not on the default branch");
    assert_eq!(r.4.as_deref(), Some("feat/login"));
    assert_eq!(refs_of(&e, 1).await, vec![a1.clone()]);
    // R-C2 and §6.10.
    let (branch, pushed): (Option<String>, bool) = sqlx::query_as(
        "SELECT r.default_branch, i.last_push_at IS NOT NULL FROM github_repos r
         JOIN github_installations i ON i.id = r.installation_id WHERE r.id = $1",
    )
    .bind(e.repo)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((branch.as_deref(), pushed), (Some("main"), true));

    // The card: first line, cut to 120; a URL built here, not GitHub's.
    let dev = read::issue_dev(&e.w.app, e.bob, &a1).await.unwrap();
    assert!(!dev.can_manage && !dev.commits_truncated);
    let c = &dev.commits[0];
    assert_eq!(c.title, format!("fix login {a1}, see {w1}"));
    assert_eq!(
        c.url,
        format!("https://github.com/acme/app/commit/{}", sha(1))
    );
    assert_eq!(
        (c.author_login.as_deref(), c.sender_login.as_deref()),
        (Some("mallory"), Some("bob"))
    );
    assert_eq!(c.at.to_rfc3339(), "2026-11-01T00:00:00+00:00");
    assert!(!c.on_default_branch);
    assert_eq!(c.default_branch.as_deref(), Some("main"));
    assert!(card(&e, e.bob, &w1).await.is_empty(), "WEB has no link");
    assert!(
        read::issue_dev(&e.w.app, e.alice, &a1)
            .await
            .unwrap()
            .can_manage
    );

    // T-M2-61: the same commit reaches main by fast-forward (`distinct`
    // false): flagged, no second ref. A commit that was missed before comes
    // in whole.
    assert_eq!(
        deliver(
            &e,
            &api,
            &push_to(
                "main",
                vec![
                    commit(1, "REWRITTEN", false),
                    commit(2, &format!("more {a1}"), false)
                ]
            )
        )
        .await,
        "ok"
    );
    let r = row(&e, 1).await.unwrap();
    assert!(r.3, "now on the default branch");
    assert!(
        r.0.starts_with("fix login"),
        "the message is never rewritten"
    );
    assert_eq!(r.4.as_deref(), Some("feat/login"));
    assert_eq!(refs_of(&e, 1).await, vec![a1.clone()]);
    assert_eq!(refs_of(&e, 2).await, vec![a1.clone()]);
    assert!(row(&e, 2).await.unwrap().3);
    assert_eq!(card(&e, e.bob, &a1).await.len(), 2);
    assert!(read::issue_dev(&e.w.app, e.bob, &a1).await.unwrap().commits[0].on_default_branch);
    // A redelivered body (a new delivery id) changes nothing.
    deliver(
        &e,
        &api,
        &push_to("feat/login", vec![commit(1, &long, true)]),
    )
    .await;
    assert_eq!(refs_of(&e, 1).await, vec![a1.clone()]);
    assert_eq!(commits_total(&e).await, 2);

    // T-M2-62: a tag, a deleted branch, a repository nothing links, one the
    // App does not know: ignored, nothing stored.
    let mut tag = push_to("x", vec![commit(3, &a1, true)]);
    tag["ref"] = json!("refs/tags/v1");
    let mut gone = push_to("old", vec![commit(4, &a1, true)]);
    gone["deleted"] = json!(true);
    let mut unknown = push_to("main", vec![commit(5, &a1, true)]);
    unknown["repository"]["id"] = json!(424242);
    sqlx::query(
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state)
         SELECT 7002, installation_id, 'acme/other', true, true, 'accessible' FROM github_repos WHERE id = $1",
    )
    .bind(e.repo)
    .execute(&pool)
    .await
    .unwrap();
    let mut unlinked = push_to("main", vec![commit(6, &a1, true)]);
    unlinked["repository"]["id"] = json!(7002);
    for p in [&tag, &gone, &unknown, &unlinked] {
        assert_eq!(deliver(&e, &api, p).await, "ignored");
    }
    assert_eq!(commits_total(&e).await, 2);
    // A sha that is not one is skipped, the rest of the push is kept.
    let mut odd = commit(7, &a1, true);
    odd["id"] = json!("not-a-sha");
    assert_eq!(
        deliver(&e, &api, &push_to("main", vec![odd, commit(8, &a1, true)])).await,
        "ok"
    );
    assert_eq!(commits_total(&e).await, 3);

    // R-C6: twenty on the card, newest first; the rest is announced.
    let many: Vec<Value> = (100..120u64)
        .map(|n| {
            let mut c = commit(n, &format!("step {a1}"), true);
            c["timestamp"] = json!(format!("2026-11-02T00:00:{:02}Z", n - 100));
            c
        })
        .collect();
    deliver(&e, &api, &push_to("feat/many", many)).await;
    let dev = read::issue_dev(&e.w.app, e.bob, &a1).await.unwrap();
    assert_eq!(dev.commits.len(), 20);
    assert!(dev.commits_truncated);
    assert_eq!(dev.commits[0].sha, sha(119));
    // `get` with `dev` (the issue page) carries the commits; who manages
    // nothing gets no hide control.
    let v = oxledger::issue_view::get(
        &e.w.app,
        e.bob,
        oxledger::issue_view::GetParams {
            key: a1.clone(),
            fields: None,
            citations: None,
            include: Some(vec!["dev".into()]),
            note_limit: None,
            event_limit: None,
        },
    )
    .await
    .unwrap();
    let dev = v.dev.unwrap();
    assert_eq!(dev.commits.len(), 20);
    assert!(!dev.can_manage);
}

// ---------------------------------------------------------------- T-M2-63

#[tokio::test]
async fn a_full_payload_is_completed_from_compare() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let api = Fake::default();
    link_to(&e, "AUTH").await;
    let (_, a1) = issue(&e, e.w.auth_p, false).await;
    let full = |from: u64| -> Vec<Value> {
        (0..PAYLOAD_COMMIT_LIMIT as u64)
            .map(|i| commit(from + i, "chore", true))
            .collect()
    };
    let from_api = |n: u64, message: &str| {
        json!({"sha": sha(n), "commit": {"message": message,
               "committer": {"date": "2026-11-03T00:00:00Z"}}, "author": {"login": "alice-gh"}})
    };

    // One short of the limit: the payload is whole, no API call.
    let mut short = full(0x10000);
    short.pop();
    assert_eq!(deliver(&e, &api, &push_to("main", short)).await, "ok");
    assert_eq!(*api.compare_calls.lock().unwrap(), 0);

    // At the limit: the rest comes from compare (one call).
    *api.compare.lock().unwrap() = vec![
        from_api(0x20000, "already in the payload"),
        from_api(0x30001, &format!("beyond the payload {a1}")),
    ];
    assert_eq!(
        deliver(&e, &api, &push_to("main", full(0x20000))).await,
        "ok"
    );
    assert_eq!(*api.compare_calls.lock().unwrap(), 1);
    assert_eq!(refs_of(&e, 0x30001).await, vec![a1.clone()]);
    let r = row(&e, 0x30001).await.unwrap();
    assert_eq!(
        (r.1.as_deref(), r.2.as_deref(), r.3),
        (Some("alice-gh"), Some("bob"), true)
    );
    assert_eq!(row(&e, 0x20000).await.unwrap().0, "chore");

    // More than compare gives: what came is kept and the delivery says so.
    *api.compare.lock().unwrap() = (0..=COMPARE_LIMIT as u64)
        .map(|i| from_api(0x50000 + i, "bulk"))
        .collect();
    assert_eq!(
        deliver(&e, &api, &push_to("main", full(0x40000))).await,
        "commits_truncated"
    );
    assert!(row(&e, 0x50000 + COMPARE_LIMIT as u64 - 1).await.is_some());
    assert!(row(&e, 0x50000 + COMPARE_LIMIT as u64).await.is_none());

    // A new branch has no `before`: nothing to compare with.
    let calls = *api.compare_calls.lock().unwrap();
    let mut fresh = push_to("feat/new", full(0x60000));
    fresh["before"] = json!("0".repeat(40));
    assert_eq!(deliver(&e, &api, &fresh).await, "commits_truncated");
    assert_eq!(*api.compare_calls.lock().unwrap(), calls);
}

// ---------------------------------------------------------------- T-M2-64

#[tokio::test]
async fn unlink_races_a_push() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = e.w.app.clone();
    let (_, a1) = issue(&e, e.w.auth_p, false).await;
    let left = || async { count(&e.w.pool, "SELECT count(*) FROM commit_issue_refs").await };

    // The worker holds the link FOR SHARE and builds refs; unlink waits,
    // then removes them.
    let l = link_to(&e, "AUTH").await;
    let g = Arc::new(Gate::default());
    faults::arm("github.after_links", Action::Block(g.clone()));
    webhook::enqueue(
        &app,
        Uuid::now_v7(),
        "push",
        &push_to("main", vec![commit(1, &a1, true)]),
    )
    .await
    .unwrap();
    let a = app.clone();
    let worker = tokio::spawn(async move { jobs::drain(&a, &Fake::default()).await.unwrap() });
    g.reached.notified().await;
    let (a, alice) = (app.clone(), e.alice);
    let unlink =
        tokio::spawn(async move { links::unlink(&a, alice, Uuid::now_v7(), "AUTH", l).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!unlink.is_finished(), "unlink waits for the worker");
    g.release.notify_one();
    common::join("the GitHub worker", worker).await;
    assert!(common::join("the unlink", unlink).await.unwrap().changed);
    assert_eq!(left().await, 0, "worker first");
    assert!(card(&e, e.alice, &a1).await.is_empty());

    // The other way: unlink holds FOR UPDATE; the worker waits and then
    // sees no live link.
    let l2 = link_to(&e, "AUTH").await;
    let g = Arc::new(Gate::default());
    faults::arm("github.unlink.after_lock", Action::Block(g.clone()));
    let (a, alice) = (app.clone(), e.alice);
    let unlink =
        tokio::spawn(async move { links::unlink(&a, alice, Uuid::now_v7(), "AUTH", l2).await });
    g.reached.notified().await;
    let d = Uuid::now_v7();
    webhook::enqueue(
        &app,
        d,
        "push",
        &push_to("main", vec![commit(2, &a1, true)]),
    )
    .await
    .unwrap();
    let a = app.clone();
    let worker = tokio::spawn(async move { jobs::drain(&a, &Fake::default()).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!worker.is_finished(), "the worker waits for the unlink");
    g.release.notify_one();
    common::join("the unlink", unlink).await.unwrap();
    common::join("the GitHub worker", worker).await;
    assert_eq!(left().await, 0, "unlink first");
    assert!(row(&e, 2).await.is_none(), "nothing stored without a link");
    let result: String =
        sqlx::query_scalar("SELECT result FROM github_deliveries WHERE delivery_id = $1")
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(result, "ignored");
    // Linked again: the commit of the first round is still there without
    // refs; a new push of it (`distinct` as GitHub sends it for a commit it
    // has seen) builds none, a commit that is new does.
    link_to(&e, "AUTH").await;
    assert!(card(&e, e.alice, &a1).await.is_empty());
}

// ---------------------------------------------------------------- T-M2-65, 65a, 66

#[tokio::test]
async fn hide_and_revoke() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let api = Fake::default();
    link_to(&e, "AUTH").await;
    link_to(&e, "WEB").await;
    let (_, a1) = issue(&e, e.w.auth_p, false).await;
    let (_, w1) = issue(&e, e.w.web, false).await;
    let (_, secret) = issue(&e, e.w.auth_p, true).await;
    let both = format!("TOP-SECRET-MESSAGE {a1} {w1}");
    deliver(
        &e,
        &api,
        &push_to(
            "feat/x",
            vec![
                commit(1, &both, true),
                commit(2, &format!("private {secret}"), true),
                commit(3, &format!("other {a1}"), true),
            ],
        ),
    )
    .await;
    assert_eq!(refs_of(&e, 1).await, vec![a1.clone(), w1.clone()]);

    // ---- hide (R-C7): this project only; managers still see it, marked.
    let er = &e;
    let hide = |who, project: &'static str, c: CommitRef, on| async move {
        links::hide_commit(&er.w.app, who, Uuid::now_v7(), project, &c, on).await
    };
    let err = hide(e.bob, "AUTH", of(&e, 1), true).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);
    let key = Uuid::now_v7();
    let r = links::hide_commit(app, e.alice, key, "AUTH", &of(&e, 1), true)
        .await
        .unwrap();
    assert!(r.hidden && r.changed);
    let again = links::hide_commit(app, e.alice, key, "AUTH", &of(&e, 1), true)
        .await
        .unwrap();
    assert!(again.changed, "the same key answers as the first time");
    assert!(
        !hide(e.alice, "AUTH", of(&e, 1), true)
            .await
            .unwrap()
            .changed
    );
    assert_eq!(card(&e, e.bob, &a1).await, vec![sha(3).to_string()]);
    assert_eq!(card(&e, e.bob, &w1).await, vec![sha(1).to_string()]);
    let dev = read::issue_dev(app, e.alice, &a1).await.unwrap();
    assert_eq!(dev.commits.len(), 2);
    assert!(dev.commits.iter().find(|c| c.sha == sha(1)).unwrap().hidden);
    assert!(
        hide(e.alice, "AUTH", of(&e, 1), false)
            .await
            .unwrap()
            .changed
    );
    assert_eq!(card(&e, e.bob, &a1).await.len(), 2);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM project_setting_events WHERE kind IN ('commit_hidden', 'commit_unhidden')"
        )
        .await,
        2
    );

    // ---- T-M2-66: a commit that only names an issue the caller cannot see
    // answers exactly like one that does not exist.
    let none = hide(e.carol, "AUTH", of(&e, 0xdead), true)
        .await
        .unwrap_err();
    assert_eq!(code(&none), ErrorCode::NotFound);
    for c in [
        of(&e, 2),
        one(1),
        CommitRef {
            repo_id: e.repo,
            sha: "abc".into(),
        },
    ] {
        let err = hide(e.carol, "AUTH", c.clone(), true).await.unwrap_err();
        assert_eq!(format!("{err:#}"), format!("{none:#}"), "{c:?}");
        let err = revoke(&e, e.carol, "AUTH", vec![c]).await.unwrap_err();
        assert!(
            matches!(code(&err), ErrorCode::NotFound | ErrorCode::InvalidRequest),
            "{err:#}"
        );
    }
    let err = revoke(&e, e.carol, "AUTH", vec![of(&e, 2)])
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_commit_revocations").await,
        0
    );
    // WEB: the commit of AUTH's issue is not WEB's.
    let err = hide(e.alice, "WEB", of(&e, 3), true).await.unwrap_err();
    assert_eq!(format!("{err:#}"), format!("{none:#}"));
    // One of link_id, pr_ids, commits.
    let err = links::revoke(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &RevokeInput {
            link_id: None,
            pr_ids: Some(vec![Uuid::now_v7()]),
            commits: Some(vec![of(&e, 1)]),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&err), "revoke_target");

    // ---- T-M2-65: revoke in AUTH. WEB still links the commit, so its
    // message stays.
    let before = epoch(&pool, e.w.auth_p).await;
    let web_epoch = epoch(&pool, e.w.web).await;
    let key = Uuid::now_v7();
    let input = RevokeInput {
        link_id: None,
        pr_ids: None,
        commits: Some(vec![of(&e, 1), of(&e, 1)]),
    };
    let r = links::revoke(app, e.alice, &e.w.policy, key, "AUTH", &input)
        .await
        .unwrap();
    assert_eq!(r.security_epoch, before + 1);
    assert_eq!(r.commits, vec![of(&e, 1)]);
    let replay = links::revoke(app, e.alice, &e.w.policy, key, "AUTH", &input)
        .await
        .unwrap();
    assert_eq!(replay.commits, r.commits);
    assert_eq!(epoch(&pool, e.w.auth_p).await, before + 1);
    assert_eq!(epoch(&pool, e.w.web).await, web_epoch, "only this project");
    assert_eq!(refs_of(&e, 1).await, vec![w1.clone()]);
    assert_eq!(row(&e, 1).await.unwrap().0, both);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_commit_revocations").await,
        1
    );
    assert_eq!(card(&e, e.alice, &a1).await, vec![sha(3).to_string()]);
    // The event names the commit, never its message.
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM project_setting_events WHERE kind = 'github_content_revoked'
         ORDER BY at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(payload["commits"][0]["sha"], sha(1));
    assert!(!payload.to_string().contains("TOP-SECRET"));
    // T-M2-66: the history tells that a commit matched an issue here, so
    // only the administrator reads these events (as for PRs, §18.5).
    let kinds = |v: oxledger::project_tabs::SettingEvents| {
        v.items.into_iter().map(|i| i.kind).collect::<Vec<_>>()
    };
    let hidden = ["commit_hidden", "commit_unhidden", "github_content_revoked"];
    for who in [e.alice, e.carol] {
        let seen = kinds(
            oxledger::project_tabs::events(app, who, "AUTH", None)
                .await
                .unwrap(),
        );
        assert!(
            seen.iter().all(|k| !hidden.contains(&k.as_str())),
            "{seen:?}"
        );
    }
    let seen = kinds(
        oxledger::project_tabs::events(app, e.admin, "AUTH", None)
            .await
            .unwrap(),
    );
    assert!(seen.iter().any(|k| k == "github_content_revoked"));

    // The administrator revokes it in WEB (no membership needed): nothing
    // links the commit any more, so its message goes.
    revoke(&e, e.admin, "WEB", vec![of(&e, 1)]).await.unwrap();
    assert!(refs_of(&e, 1).await.is_empty());
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.0.as_str(), r.5), (REDACTED, true));
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM github_commits WHERE message LIKE '%TOP-SECRET%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);

    // ---- T-M2-65a: the same sha comes back (another branch, a force push
    // back, the original body under a new delivery id).
    for p in [
        push_to("feat/again", vec![commit(1, &both, true)]),
        push_to("main", vec![commit(1, &both, false)]),
        push_to("feat/x", vec![commit(1, &both, true)]),
    ] {
        assert_eq!(deliver(&e, &api, &p).await, "ok");
    }
    assert!(refs_of(&e, 1).await.is_empty(), "the tombstones hold");
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.0.as_str(), r.5), (REDACTED, true));
    assert!(card(&e, e.alice, &a1).await.iter().all(|s| s != sha(1)));
    assert!(card(&e, e.alice, &w1).await.is_empty());

    // R-C8: 30 days on, commits without a ref go — but not a revoked one,
    // and never a tombstone. Then the sha is pushed once more.
    deliver(
        &e,
        &api,
        &push_to("feat/x", vec![commit(9, "names nothing", true)]),
    )
    .await;
    assert!(row(&e, 9).await.is_some());
    sync::sweep_once(app, Utc::now() + Duration::days(29))
        .await
        .unwrap();
    assert!(row(&e, 9).await.is_some(), "not yet");
    sync::sweep_once(app, Utc::now() + Duration::days(31))
        .await
        .unwrap();
    assert!(row(&e, 9).await.is_none(), "no ref for 30 days");
    assert!(row(&e, 3).await.is_some(), "a commit with a ref stays");
    assert_eq!(row(&e, 1).await.unwrap().0, REDACTED);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_commit_revocations").await,
        2
    );
    deliver(&e, &api, &push_to("main", vec![commit(1, &both, true)])).await;
    assert!(refs_of(&e, 1).await.is_empty());
    assert_eq!(row(&e, 1).await.unwrap().0, REDACTED);

    // A repository that lost its last link 30 days ago loses its commits,
    // except the revoked ones.
    exec(
        &pool,
        "UPDATE project_repo_links SET unlinked_at = now() - interval '31 days', unlinked_by = linked_by WHERE unlinked_at IS NULL",
    )
    .await;
    exec(&pool, "DELETE FROM commit_issue_refs").await;
    sync::sweep_once(app, Utc::now()).await.unwrap();
    assert!(row(&e, 3).await.is_none());
    assert_eq!(row(&e, 1).await.unwrap().0, REDACTED);
    assert_eq!(commits_total(&e).await, 1);
}

// ---------------------------------------------------------------- admin page (§6.10)

#[tokio::test]
async fn admin_sees_when_push_last_came() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let v = links::admin_get(&e.w.app, e.admin).await.unwrap();
    assert!(v.installations[0].last_push_at.is_none(), "never");
    // Even a push that is ignored (nothing links the repository) shows the
    // App is subscribed.
    assert_eq!(
        deliver(&e, &Fake::default(), &push_to("main", vec![])).await,
        "ignored"
    );
    let v = links::admin_get(&e.w.app, e.admin).await.unwrap();
    assert!(v.installations[0].last_push_at.is_some());
}

// ---------------------------------------------------------------- T-M2-78

#[tokio::test]
async fn the_webhook_sheds_load_instead_of_queueing() {
    let pool = common::test_pool_sized(40).await;
    let e = setup(&pool).await;
    let router = api::router_with_github(
        e.w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
        Some(GithubConfig {
            webhook_secret: SECRET.to_vec(),
        }),
    );
    let post = |r: axum::Router| async move {
        let body = serde_json::to_vec(&push_to("main", vec![])).unwrap();
        let req = Request::post(webhook::PATH)
            .header("x-github-event", "push")
            .header("x-github-delivery", Uuid::now_v7().to_string())
            .header("content-type", "application/json")
            .header("x-hub-signature-256", webhook::sign(SECRET, &body))
            .body(Body::from(body))
            .unwrap();
        r.oneshot(req).await.unwrap().status()
    };
    // Sixteen requests stuck in the handler.
    let g = Arc::new(Gate::default());
    for _ in 0..webhook::MAX_CONCURRENT {
        faults::arm("github.webhook.before_commit", Action::Block(g.clone()));
    }
    let held: Vec<_> = (0..webhook::MAX_CONCURRENT)
        .map(|_| tokio::spawn(post(router.clone())))
        .collect();
    g.reached.notified().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    // The seventeenth is refused at once: it does not wait in line.
    let extra = tokio::time::timeout(std::time::Duration::from_secs(5), post(router.clone()))
        .await
        .expect("the 17th request must not queue");
    assert_eq!(extra, StatusCode::SERVICE_UNAVAILABLE);
    // The sixteen finish as usual.
    let mut done = 0;
    for h in held {
        let mut h = h;
        let status = loop {
            g.release.notify_waiters();
            match tokio::time::timeout(std::time::Duration::from_millis(50), &mut h).await {
                Ok(s) => break s.unwrap(),
                Err(_) => continue,
            }
        };
        assert_eq!(status, StatusCode::ACCEPTED);
        done += 1;
    }
    assert_eq!(done, webhook::MAX_CONCURRENT);
    // With room again, a new request is taken.
    assert_eq!(post(router.clone()).await, StatusCode::ACCEPTED);
}
