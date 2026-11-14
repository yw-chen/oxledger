// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.7: GitHub webhook intake, the worker, normalization and
//! roll-ups, without any network (a fake `GithubApi`): T-G1, T-G2, T-G3,
//! T-G4, T-G5 (refs), T-G6, T-G7, T-G8, T-G19, T-G20, T-G22, T-G25, T-G26,
//! T-G32, T-G33, T-G35, T-G36. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use common::test_pool;
use oxledger::api::{self, ApiConfig};
use oxledger::auth::SessionPolicy;
use oxledger::github::client::{Fut, GithubApi, HookDelivery};
use oxledger::github::rollup::Review;
use oxledger::github::{GithubConfig, jobs, webhook};
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

const SECRET: &[u8] = b"test-webhook-secret";
const INST: i64 = 77;
const REPO: i64 = 1001;
const APP_A: i64 = 15368;
const APP_B: i64 = 99;

// ---------------------------------------------------------------- fake GitHub

#[derive(Default)]
struct State {
    pulls: HashMap<i32, Value>,
    commits: HashMap<i32, Vec<String>>,
    reviews: HashMap<i32, Vec<Review>>,
    suites: HashMap<i64, Value>,
    runs: HashMap<i64, Value>,
    calls: HashMap<&'static str, usize>,
    /// T-G25/T-G35: a newer webhook that commits while a refetch is in flight.
    during_get: Option<(String, Value)>,
}

struct Fake {
    app: Arc<App>,
    s: Mutex<State>,
}

impl Fake {
    fn new(app: Arc<App>) -> Arc<Fake> {
        Arc::new(Fake {
            app,
            s: Mutex::new(State::default()),
        })
    }

    fn calls(&self, k: &str) -> usize {
        *self.s.lock().unwrap().calls.get(k).unwrap_or(&0)
    }

    fn count(&self, k: &'static str) {
        *self.s.lock().unwrap().calls.entry(k).or_default() += 1;
    }

    async fn interleave(&self) {
        let w = self.s.lock().unwrap().during_get.take();
        if let Some((event, payload)) = w {
            webhook::enqueue(&self.app, Uuid::now_v7(), &event, &payload)
                .await
                .unwrap();
            jobs::drain(&self.app, self).await.unwrap();
        }
    }
}

impl GithubApi for Fake {
    fn get_pull<'a>(&'a self, _: i64, _: &'a str, n: i32) -> Fut<'a, Option<Value>> {
        Box::pin(async move {
            self.count("get_pull");
            let v = self.s.lock().unwrap().pulls.get(&n).cloned();
            self.interleave().await;
            Ok(v)
        })
    }
    fn list_pulls<'a>(
        &'a self,
        _: i64,
        _: &'a str,
        _: &'a str,
        _: Option<DateTime<Utc>>,
    ) -> Fut<'a, Vec<Value>> {
        Box::pin(async move { Ok(self.s.lock().unwrap().pulls.values().cloned().collect()) })
    }
    fn list_pull_commits<'a>(&'a self, _: i64, _: &'a str, n: i32) -> Fut<'a, Vec<String>> {
        Box::pin(async move {
            Ok(self
                .s
                .lock()
                .unwrap()
                .commits
                .get(&n)
                .cloned()
                .unwrap_or_default())
        })
    }
    fn list_reviews<'a>(&'a self, _: i64, _: &'a str, n: i32) -> Fut<'a, Vec<Review>> {
        Box::pin(async move {
            Ok(self
                .s
                .lock()
                .unwrap()
                .reviews
                .get(&n)
                .cloned()
                .unwrap_or_default())
        })
    }
    fn get_check_suite<'a>(&'a self, _: i64, _: &'a str, id: i64) -> Fut<'a, Option<Value>> {
        Box::pin(async move {
            self.count("get_check_suite");
            let v = self.s.lock().unwrap().suites.get(&id).cloned();
            self.interleave().await;
            Ok(v)
        })
    }
    fn get_check_run<'a>(&'a self, _: i64, _: &'a str, id: i64) -> Fut<'a, Option<Value>> {
        Box::pin(async move {
            self.count("get_check_run");
            Ok(self.s.lock().unwrap().runs.get(&id).cloned())
        })
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

// ---------------------------------------------------------------- helpers

fn router(w: &World) -> Router {
    api::router_with_github(
        w.app.clone(),
        ApiConfig {
            spa_origin: "https://spa.test".into(),
            session: SessionPolicy::default(),
        },
        Some(GithubConfig {
            webhook_secret: SECRET.to_vec(),
        }),
    )
}

async fn post(
    r: &Router,
    event: &str,
    delivery: &str,
    body: &[u8],
    sig: Option<String>,
) -> StatusCode {
    let mut b = Request::post(webhook::PATH)
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("content-type", "application/json");
    if let Some(s) = sig {
        b = b.header("x-hub-signature-256", s);
    }
    r.clone()
        .oneshot(b.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
        .status()
}

/// A signed delivery with a fresh id; the worker runs to empty.
async fn send(w: &World, r: &Router, api: &Fake, event: &str, mut p: Value) -> Uuid {
    p["installation"] = json!({"id": INST});
    if p.get("repository").is_none() {
        p["repository"] = json!({"id": REPO, "full_name": "acme/app", "private": true});
    }
    let body = serde_json::to_vec(&p).unwrap();
    let d = Uuid::now_v7();
    let st = post(
        r,
        event,
        &d.to_string(),
        &body,
        Some(webhook::sign(SECRET, &body)),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    jobs::drain(&w.app, api).await.unwrap();
    d
}

async fn result(w: &World, d: Uuid) -> String {
    sqlx::query_scalar("SELECT result FROM github_deliveries WHERE delivery_id = $1")
        .bind(d)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn at(sec: u32) -> String {
    format!("2026-11-01T00:00:{sec:02}Z")
}

fn pr(n: i32, title: &str, branch: &str, sha: &str, sec: u32) -> Value {
    json!({"number": n, "title": title, "head": {"ref": branch, "sha": sha}, "state": "open",
           "draft": false, "merged_at": null, "html_url": format!("https://github.com/acme/app/pull/{n}"),
           "updated_at": at(sec), "requested_reviewers": [], "requested_teams": []})
}

fn run(
    id: i64,
    sha: &str,
    app: i64,
    suite: Option<i64>,
    name: &str,
    status: &str,
    concl: Option<&str>,
) -> Value {
    json!({"check_run": {"id": id, "head_sha": sha, "app": {"id": app},
           "check_suite": suite.map(|s| json!({"id": s})), "name": name, "status": status,
           "conclusion": concl}})
}

fn suite(id: i64, sha: &str, status: &str, concl: Option<&str>, sec: u32) -> Value {
    json!({"id": id, "head_sha": sha, "app": {"id": APP_A}, "status": status,
           "conclusion": concl, "updated_at": at(sec)})
}

async fn pr_row(w: &World, n: i32) -> (Uuid, String, String, Option<String>, String) {
    sqlx::query_as(
        "SELECT p.id, p.title, p.review_state, p.ci_state, p.head_sha FROM github_pull_requests p
         JOIN github_repos r ON r.id = p.repo_id WHERE r.gh_repo_id = $1 AND p.number = $2",
    )
    .bind(REPO)
    .bind(n)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn ci(w: &World, n: i32) -> Option<String> {
    pr_row(w, n).await.3
}

async fn refs(w: &World, n: i32) -> Vec<(String, Vec<String>)> {
    sqlx::query_as(
        "SELECT k.key, f.sources FROM pr_issue_refs f
         JOIN github_pull_requests p ON p.id = f.pr_id
         JOIN issue_keys k ON k.issue_id = f.issue_id AND k.is_current
         WHERE p.number = $1 ORDER BY k.key",
    )
    .bind(n)
    .fetch_all(&w.pool)
    .await
    .unwrap()
}

async fn mk(w: &World, project: Uuid) -> String {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: project,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "work",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap()
    .1
}

async fn link(w: &World, project: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO project_repo_links (project_id, repo_id, linked_by)
         SELECT $1, id, $2 FROM github_repos WHERE gh_repo_id = $3 RETURNING id",
    )
    .bind(project)
    .bind(w.alice)
    .bind(REPO)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn setup(pool: &sqlx::PgPool) -> (World, Router, Arc<Fake>) {
    let w = World::build(pool).await;
    exec(
        pool,
        "TRUNCATE github_event_jobs, github_deliveries, pr_issue_refs, project_pr_hidden,
                  github_commit_statuses, github_check_suites, github_check_runs,
                  github_pull_requests, project_repo_links, github_repos, github_installations CASCADE",
    )
    .await;
    let r = router(&w);
    let api = Fake::new(w.app.clone());
    // T-G22 (a): the first installation with three repositories.
    send(
        &w,
        &r,
        &api,
        "installation",
        json!({"action": "created", "installation": {"id": INST, "account": {"login": "acme"}},
               "repository": null,
               "repositories": [{"id": REPO, "full_name": "acme/app", "private": true},
                                {"id": 1002, "full_name": "acme/web", "private": false},
                                {"id": 1003, "full_name": "acme/ops", "private": true}]}),
    )
    .await;
    (w, r, api)
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn webhook_intake() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    let deliveries = || count(&pool, "SELECT count(*) FROM github_deliveries");
    let base = deliveries().await;

    // T-G1: bad signature, no header, one byte changed.
    let body = br#"{"zen": "hi"}"#.to_vec();
    let good = webhook::sign(SECRET, &body);
    let d = Uuid::now_v7().to_string();
    assert_eq!(
        post(&r, "ping", &d, &body, Some(webhook::sign(b"other", &body))).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&r, "ping", &d, &body, None).await,
        StatusCode::UNAUTHORIZED
    );
    let mut changed = body.clone();
    changed[3] ^= 1;
    assert_eq!(
        post(&r, "ping", &d, &changed, Some(good.clone())).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&r, "ping", &d, &body, Some("sha256=zz".into())).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(deliveries().await, base, "T-G1: nothing stored");
    // B19: a delivery id that is not a UUID.
    assert_eq!(
        post(&r, "ping", "not-a-uuid", &body, Some(good.clone())).await,
        StatusCode::BAD_REQUEST
    );

    // T-G2: the same delivery twice.
    assert_eq!(
        post(&r, "ping", &d, &body, Some(good.clone())).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post(&r, "ping", &d, &body, Some(good.clone())).await,
        StatusCode::OK
    );
    assert_eq!(deliveries().await, base + 1);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM github_event_jobs WHERE payload IS NOT NULL"
        )
        .await,
        1
    );
    assert_eq!(jobs::drain(&w.app, api.as_ref()).await.unwrap(), 1);
    assert_eq!(
        jobs::drain(&w.app, api.as_ref()).await.unwrap(),
        0,
        "processed once"
    );
    assert_eq!(result(&w, Uuid::parse_str(&d).unwrap()).await, "ignored");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM github_event_jobs WHERE payload IS NOT NULL"
        )
        .await,
        0
    );
    // Too large: 413.
    let big = vec![b' '; webhook::MAX_BODY + 1];
    assert_eq!(
        post(
            &r,
            "ping",
            &Uuid::now_v7().to_string(),
            &big,
            Some(webhook::sign(SECRET, &big))
        )
        .await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // Without GitHub configured the path does not exist.
    let plain = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: "https://spa.test".into(),
            session: SessionPolicy::default(),
        },
    );
    assert_ne!(
        post(&plain, "ping", &d, &body, Some(good)).await,
        StatusCode::ACCEPTED
    );

    // A failing event backs off and gives up after 5 attempts.
    link(&w, w.auth_p).await;
    let bad = send(
        &w,
        &r,
        &api,
        "status",
        json!({"id": 1, "sha": "s", "context": "c", "state": "weird"}),
    )
    .await;
    for _ in 0..5 {
        exec(&pool, "UPDATE github_event_jobs SET next_at = now() - interval '1 second' WHERE payload IS NOT NULL").await;
        jobs::drain(&w.app, api.as_ref()).await.unwrap();
    }
    assert_eq!(result(&w, bad).await, "error");
    let (attempts, payload_null): (i16, bool) = sqlx::query_as(
        "SELECT attempts, payload IS NULL FROM github_event_jobs WHERE delivery_id = $1",
    )
    .bind(bad)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, payload_null), (5, true));
}

#[tokio::test]
async fn installations_and_access() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    let state = |gh: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT state FROM github_repos WHERE gh_repo_id = $1")
                .bind(gh)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM github_repos WHERE state = 'accessible'"
        )
        .await,
        3
    );
    link(&w, w.auth_p).await;
    // T-G22 (b): a repository removed from the installation.
    send(&w, &r, &api, "installation_repositories",
         json!({"action": "removed", "repositories_added": [], "repositories_removed": [{"id": 1003}]})).await;
    assert_eq!(state(1003).await, "inaccessible");
    // T-G15 (rename, event side): same numeric id, new name.
    send(&w, &r, &api, "repository",
         json!({"action": "renamed", "repository": {"id": REPO, "full_name": "acme/app2", "private": true}})).await;
    let name: String =
        sqlx::query_scalar("SELECT full_name FROM github_repos WHERE gh_repo_id = $1")
            .bind(REPO)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "acme/app2");
    // T-G22 (c): suspended; a late check_run is ignored.
    send(
        &w,
        &r,
        &api,
        "installation",
        json!({"action": "suspend", "installation": {"id": INST}, "repository": null}),
    )
    .await;
    assert_eq!(state(REPO).await, "inaccessible");
    let d = send(
        &w,
        &r,
        &api,
        "check_run",
        run(5, "abc", APP_A, None, "test", "completed", Some("failure")),
    )
    .await;
    assert_eq!(result(&w, d).await, "ignored");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_check_runs").await,
        0
    );
    // Unsuspended: accessible again (the removed one stays out).
    send(
        &w,
        &r,
        &api,
        "installation",
        json!({"action": "unsuspend", "installation": {"id": INST}, "repository": null}),
    )
    .await;
    assert_eq!(state(REPO).await, "accessible");
    assert_eq!(state(1003).await, "inaccessible");
    // No live link → ignored, no PR stored.
    let d = send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "opened", "pull_request": pr(1, "x", "b", "s", 1),
                        "repository": {"id": 1002}}),
    )
    .await;
    assert_eq!(result(&w, d).await, "ignored");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_pull_requests").await,
        0
    );
}

#[tokio::test]
async fn refs_and_reviews() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    let a1 = mk(&w, w.auth_p).await;
    let a2 = mk(&w, w.auth_p).await;
    let w1 = mk(&w, w.web).await;
    let auth_link = link(&w, w.auth_p).await;

    // T-G3: title, branch, commit; only AUTH is linked.
    api.s
        .lock()
        .unwrap()
        .commits
        .insert(1, vec![format!("{w1} refactor"), format!("touch {a1}")]);
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "opened",
         "pull_request": pr(1, &format!("修 {a1}"), &format!("fix/{a2}-x"), "s1", 1)}),
    )
    .await;
    let mut want = vec![
        (a1.clone(), vec!["title".to_string(), "commit".into()]),
        (a2.clone(), vec!["branch".into()]),
    ];
    want.sort();
    assert_eq!(refs(&w, 1).await, want, "T-G3: WEB key ignored");
    let lid: Uuid = sqlx::query_scalar("SELECT DISTINCT link_id FROM pr_issue_refs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lid, auth_link);

    // T-G4: none of these match.
    let n = a1.trim_start_matches("AUTH-");
    send(&w, &r, &api, "pull_request", json!({"action": "opened",
         "pull_request": pr(2, &format!("auth-{n} XAUTH-{n} AUTH-0{n} AUTH-{n}a"), "main", "s2", 1)})).await;
    assert!(refs(&w, 2).await.is_empty(), "T-G4");

    // T-G5 (refs): link WEB too; each project gets its own ref and link.
    let web_link = link(&w, w.web).await;
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited",
         "pull_request": pr(1, &format!("修 {a1}"), &format!("fix/{a2}-x"), "s1", 2)}),
    )
    .await;
    assert_eq!(refs(&w, 1).await.len(), 3);
    let wl: Uuid = sqlx::query_scalar("SELECT link_id FROM pr_issue_refs WHERE project_id = $1")
        .bind(w.web)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(wl, web_link);
    // R-G4: issues untouched.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_events WHERE kind <> 'created'"
        )
        .await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issues WHERE lock_version <> 0").await,
        0
    );

    // T-G8: A approves, B requests changes, B approves.
    api.s.lock().unwrap().reviews.insert(
        1,
        vec![
            Review {
                reviewer: 1,
                state: "APPROVED".into(),
                order: 1,
            },
            Review {
                reviewer: 2,
                state: "CHANGES_REQUESTED".into(),
                order: 2,
            },
        ],
    );
    send(
        &w,
        &r,
        &api,
        "pull_request_review",
        json!({"action": "submitted",
         "pull_request": pr(1, &format!("修 {a1}"), &format!("fix/{a2}-x"), "s1", 2)}),
    )
    .await;
    assert_eq!(pr_row(&w, 1).await.2, "changes_requested");
    api.s
        .lock()
        .unwrap()
        .reviews
        .get_mut(&1)
        .unwrap()
        .push(Review {
            reviewer: 2,
            state: "APPROVED".into(),
            order: 3,
        });
    send(
        &w,
        &r,
        &api,
        "pull_request_review",
        json!({"action": "submitted",
         "pull_request": pr(1, &format!("修 {a1}"), &format!("fix/{a2}-x"), "s1", 2)}),
    )
    .await;
    assert_eq!(pr_row(&w, 1).await.2, "approved", "T-G8");
    // Requested reviewers and no reviews: pending.
    let mut p3 = pr(3, "x", "y", "s3", 1);
    p3["requested_reviewers"] = json!([{"id": 9}]);
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "review_requested", "pull_request": p3}),
    )
    .await;
    assert_eq!(pr_row(&w, 3).await.2, "pending");
    // Draft / merged normalization.
    let mut p4 = pr(4, "x", "y", "s4", 1);
    p4["draft"] = json!(true);
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "opened", "pull_request": p4.clone()}),
    )
    .await;
    let st: String = sqlx::query_scalar("SELECT state FROM github_pull_requests WHERE number = 4")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "draft");
    p4["state"] = json!("closed");
    p4["merged_at"] = json!(at(5));
    p4["updated_at"] = json!(at(5));
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "closed", "pull_request": p4}),
    )
    .await;
    let st: String = sqlx::query_scalar("SELECT state FROM github_pull_requests WHERE number = 4")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "merged");
}

#[tokio::test]
async fn ordering_and_ci() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    link(&w, w.auth_p).await;
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "opened", "pull_request": pr(1, "t", "b", "sha1", 1)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await, None, "no checks → NULL");

    // T-G6: the newer success first, the older in_progress after.
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            10,
            "sha1",
            APP_A,
            None,
            "build",
            "completed",
            Some("success"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(10, "sha1", APP_A, None, "build", "in_progress", None),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("success"), "T-G6");

    // T-G32: one waiting, the others success → pending.
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(11, "sha1", APP_A, None, "deploy", "waiting", None),
    )
    .await;
    let st: String =
        sqlx::query_scalar("SELECT status FROM github_check_runs WHERE gh_run_id = 11")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(st, "pending");
    assert_eq!(ci(&w, 1).await.as_deref(), Some("pending"), "T-G32");
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            11,
            "sha1",
            APP_A,
            None,
            "deploy",
            "completed",
            Some("skipped"),
        ),
    )
    .await;
    assert_eq!(
        ci(&w, 1).await.as_deref(),
        Some("success"),
        "skipped does not count"
    );

    // T-G19 (a): two apps, both `test`, no suites: failure and a later success → failure.
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            12,
            "sha1",
            APP_A,
            None,
            "test",
            "completed",
            Some("failure"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            13,
            "sha1",
            APP_B,
            None,
            "test",
            "completed",
            Some("success"),
        ),
    )
    .await;
    assert_eq!(
        ci(&w, 1).await.as_deref(),
        Some("failure"),
        "T-G19: no guessing re-runs"
    );
    // A run with two different completed conclusions → refetch decides.
    api.s.lock().unwrap().runs.insert(
        12,
        run(
            12,
            "sha1",
            APP_A,
            None,
            "test",
            "completed",
            Some("success"),
        )["check_run"]
            .clone(),
    );
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            12,
            "sha1",
            APP_A,
            None,
            "test",
            "completed",
            Some("neutral"),
        ),
    )
    .await;
    assert_eq!(api.calls("get_check_run"), 1);
    assert_eq!(ci(&w, 1).await.as_deref(), Some("success"));

    // T-G7: a new commit; old checks stay but do not count.
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "synchronize", "pull_request": pr(1, "t", "b", "sha2", 3)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await, None, "T-G7: new head without results");
    assert!(
        count(
            &pool,
            "SELECT count(*) FROM github_check_runs WHERE head_sha = 'sha1'"
        )
        .await
            >= 4
    );
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(20, "sha2", APP_A, None, "build", "queued", None),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("pending"));

    // Commit statuses: the larger id wins per context.
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            20,
            "sha2",
            APP_A,
            None,
            "build",
            "completed",
            Some("success"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "status",
        json!({"id": 6, "sha": "sha2", "context": "ci/old", "state": "success"}),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "status",
        json!({"id": 5, "sha": "sha2", "context": "ci/old", "state": "failure"}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("success"));
    send(
        &w,
        &r,
        &api,
        "status",
        json!({"id": 7, "sha": "sha2", "context": "ci/old", "state": "error"}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("failure"));
}

#[tokio::test]
async fn suites() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    link(&w, w.auth_p).await;
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "opened", "pull_request": pr(1, "t", "b", "h", 1)}),
    )
    .await;
    // Two workflows (suites) of one app, each with a job named `test`.
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            1,
            "h",
            APP_A,
            Some(100),
            "test",
            "completed",
            Some("failure"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            2,
            "h",
            APP_A,
            Some(200),
            "test",
            "completed",
            Some("success"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(100, "h", "completed", Some("failure"), 1)}),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(200, "h", "completed", Some("success"), 2)}),
    )
    .await;
    assert_eq!(
        ci(&w, 1).await.as_deref(),
        Some("failure"),
        "T-G26: each suite counts"
    );

    // T-G19 (b) / T-G35: the same workflow re-run: suite 100 success (newer)
    // arrives first, the old failure after → success.
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(100, "h", "completed", Some("success"), 5)}),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(100, "h", "completed", Some("failure"), 1)}),
    )
    .await;
    assert_eq!(
        ci(&w, 1).await.as_deref(),
        Some("success"),
        "T-G35 / T-G19 re-run"
    );

    // T-G33: two jobs `test` in one suite, failure and later success: GitHub's
    // suite conclusion is failure → failure (the runs are only details).
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            3,
            "h",
            APP_A,
            Some(300),
            "test",
            "completed",
            Some("failure"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_run",
        run(
            4,
            "h",
            APP_A,
            Some(300),
            "test",
            "completed",
            Some("success"),
        ),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(300, "h", "completed", Some("failure"), 6)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("failure"), "T-G33");
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(300, "h", "completed", Some("success"), 7)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("success"));

    // T-G36: completed failure → in_progress (newer, re-run) → pending.
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(300, "h", "in_progress", None, 8)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("pending"), "T-G36");

    // A suite with no run of this head is not a unit (queued forever by other apps).
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(400, "h", "queued", None, 9)}),
    )
    .await;
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(300, "h", "completed", Some("success"), 10)}),
    )
    .await;
    assert_eq!(ci(&w, 1).await.as_deref(), Some("success"));

    // T-G35 (refetch side): same time, different content → refetch; while
    // it is in flight a newer webhook (success) commits; the old answer
    // (failure) does not overwrite it.
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(200, "h", "completed", Some("success"), 20)}),
    )
    .await;
    {
        let mut s = api.s.lock().unwrap();
        s.suites
            .insert(200, suite(200, "h", "completed", Some("failure"), 20));
        s.during_get = Some((
            "check_suite".into(),
            json!({"check_suite": suite(200, "h", "completed", Some("success"), 30),
            "installation": {"id": INST}, "repository": {"id": REPO}}),
        ));
    }
    send(
        &w,
        &r,
        &api,
        "check_suite",
        json!({"check_suite": suite(200, "h", "completed", Some("cancelled"), 20)}),
    )
    .await;
    assert_eq!(api.calls("get_check_suite"), 1);
    assert_eq!(
        ci(&w, 1).await.as_deref(),
        Some("success"),
        "T-G35: newer webhook kept"
    );
}

#[tokio::test]
async fn pr_refetch() {
    let pool = test_pool().await;
    let (w, r, api) = setup(&pool).await;
    link(&w, w.auth_p).await;
    // T-G20: same updated_at, different content; the newer state came first.
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited", "pull_request": pr(1, "newer title", "b", "h", 5)}),
    )
    .await;
    api.s
        .lock()
        .unwrap()
        .pulls
        .insert(1, pr(1, "current title", "b", "h", 6));
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited", "pull_request": pr(1, "older title", "b", "h", 5)}),
    )
    .await;
    assert_eq!(api.calls("get_pull"), 1, "T-G20: refetched");
    assert_eq!(
        pr_row(&w, 1).await.1,
        "current title",
        "the refetch is the current state"
    );
    // An older snapshot is ignored without refetching.
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited", "pull_request": pr(1, "ancient", "b", "h", 1)}),
    )
    .await;
    assert_eq!(pr_row(&w, 1).await.1, "current title");
    assert_eq!(api.calls("get_pull"), 1);

    // T-G25: the refetch reads state A while a newer webhook writes B.
    api.s
        .lock()
        .unwrap()
        .pulls
        .insert(1, pr(1, "A (old)", "b", "h", 6));
    api.s.lock().unwrap().during_get = Some((
        "pull_request".into(),
        json!({"action": "edited",
        "pull_request": pr(1, "B (newer)", "b", "h", 9),
        "installation": {"id": INST}, "repository": {"id": REPO}}),
    ));
    send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited", "pull_request": pr(1, "same-time other", "b", "h", 6)}),
    )
    .await;
    assert_eq!(api.calls("get_pull"), 2);
    assert_eq!(
        pr_row(&w, 1).await.1,
        "B (newer)",
        "T-G25: row_version changed, B kept"
    );

    // Same time, still different every time → gives up after 3, keeps the row.
    api.s
        .lock()
        .unwrap()
        .pulls
        .insert(1, pr(1, "flip", "b", "h", 9));
    let d = send(
        &w,
        &r,
        &api,
        "pull_request",
        json!({"action": "edited", "pull_request": pr(1, "flop", "b", "h", 9)}),
    )
    .await;
    // The refetched "flip" is taken (row_version unchanged) on the first round.
    assert_eq!(pr_row(&w, 1).await.1, "flip");
    assert_eq!(result(&w, d).await, "ok");
}

// ---------------------------------------------------------------- R-G8 race on a new run

fn race_run(status: &str, conclusion: Option<&str>) -> oxledger::github::model::RunSnap {
    oxledger::github::model::RunSnap {
        id: 990_001,
        head_sha: "abc".into(),
        app_id: 1,
        suite_id: None,
        name: "test".into(),
        gh_status: status.into(),
        conclusion: conclusion.map(String::from),
        started_at: None,
        completed_at: None,
    }
}

/// Two writers of a run not stored yet: the completed one commits last, the
/// older in_progress one must not set it back (found by the S6b verifier).
#[tokio::test]
async fn new_run_race_keeps_completed() {
    use oxledger::github::model;
    let pool = test_pool().await;
    exec(
        &pool,
        "DELETE FROM github_check_runs WHERE gh_run_id = 990001",
    )
    .await;
    let repo: Uuid = sqlx::query_scalar(
        "INSERT INTO github_repos (gh_repo_id, full_name, is_private, state)
         VALUES (990001, 'probe/race', true, 'accessible')
         ON CONFLICT (gh_repo_id) DO UPDATE SET full_name = EXCLUDED.full_name RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut a = pool.begin().await.unwrap();
    model::upsert_run(&mut a, repo, &race_run("completed", Some("success")))
        .await
        .unwrap();
    let p2 = pool.clone();
    let b = tokio::spawn(async move {
        let mut b = p2.begin().await.unwrap();
        model::upsert_run(&mut b, repo, &race_run("in_progress", None))
            .await
            .unwrap();
        b.commit().await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!b.is_finished(), "the second insert waits for the first");
    a.commit().await.unwrap();
    common::join("the newer webhook", b).await;
    let st: String =
        sqlx::query_scalar("SELECT status FROM github_check_runs WHERE gh_run_id = 990001")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(st, "completed", "R-G8: never set back");
    exec(
        &pool,
        "DELETE FROM github_check_runs WHERE gh_run_id = 990001",
    )
    .await;
    exec(&pool, "DELETE FROM github_repos WHERE gh_repo_id = 990001").await;
}
