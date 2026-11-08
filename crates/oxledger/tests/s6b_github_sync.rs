// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.8: the REST client against a mock GitHub (no network,
//! no real GitHub): App JWT, installation tokens, pages, rate limit;
//! backfill, reconcile, redelivery: T-G13, T-G15, T-G16, T-G21, T-G30,
//! T-G31, T-G34. Needs `--features github` and PostgreSQL 18.
#![cfg(feature = "github")]

mod common;
#[path = "github_mock/mod.rs"]
mod mock;
#[path = "s6_world/mod.rs"]
mod world;

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use common::test_pool;
use mock::{APP_ID, KEY_PEM, Mock, calls, deliver, pr, ts};
use oxledger::api::{self, ApiConfig};
use oxledger::auth::SessionPolicy;
use oxledger::changesets::faults::{self, Action};
use oxledger::github::client::{GithubApi, RateLimited};
use oxledger::github::http::HttpGithub;
use oxledger::github::{GithubConfig, GithubSettings, jobs, sync};
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use world::*;

const SECRET: &[u8] = b"sync-test-webhook-secret-7f3a";
const INST: i64 = 501;
const REPO: i64 = 9001;

struct Env {
    w: World,
    m: Mock,
    api: HttpGithub,
    repo: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> Env {
    let w = World::build(pool).await;
    exec(
        pool,
        "TRUNCATE github_event_jobs, github_deliveries, pr_issue_refs, project_pr_hidden,
                  github_commit_statuses, github_check_suites, github_check_runs,
                  github_pull_requests, project_repo_links, github_repos, github_installations CASCADE",
    )
    .await;
    faults::clear();
    let m = mock::start().await;
    {
        let mut g = m.lock().unwrap();
        g.secret = SECRET.to_vec();
        g.hook = Some(api::router_with_github(
            w.app.clone(),
            ApiConfig {
                spa_origin: "https://spa.test".into(),
                session: SessionPolicy::default(),
            },
            Some(GithubConfig {
                webhook_secret: SECRET.to_vec(),
            }),
        ));
        g.installations =
            vec![json!({"id": INST, "account": {"login": "acme"}, "suspended_at": null})];
        g.repos.insert(
            INST,
            vec![json!({"id": REPO, "full_name": "acme/app", "private": true})],
        );
    }
    let api = HttpGithub::new(&mock::base(&m), APP_ID, KEY_PEM, true).unwrap();
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
    Env { w, m, api, repo }
}

async fn link(e: &Env, state: &str, synced: Option<chrono::DateTime<Utc>>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO project_repo_links (project_id, repo_id, linked_by, sync_state, synced_through)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(e.w.auth_p)
    .bind(e.repo)
    .bind(e.w.alice)
    .bind(state)
    .bind(synced)
    .fetch_one(&e.w.pool)
    .await
    .unwrap()
}

async fn link_state(
    e: &Env,
    id: Uuid,
) -> (
    String,
    Option<chrono::DateTime<Utc>>,
    Option<chrono::DateTime<Utc>>,
) {
    sqlx::query_as(
        "SELECT sync_state, synced_through, paused_until FROM project_repo_links WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.w.pool)
    .await
    .unwrap()
}

async fn mk(e: &Env) -> String {
    let w = &e.w;
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.auth_p,
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

fn put_pr(e: &Env, v: serde_json::Value) {
    let n = v["number"].as_i64().unwrap();
    e.m.lock().unwrap().pulls.insert(("acme/app".into(), n), v);
}

async fn pr_state(e: &Env, n: i32) -> Option<(String, Option<String>, String)> {
    sqlx::query_as("SELECT state, ci_state, title FROM github_pull_requests WHERE repo_id = $1 AND number = $2")
        .bind(e.repo)
        .bind(n)
        .fetch_optional(&e.w.pool)
        .await
        .unwrap()
}

fn limit_after(e: &Env, n: usize) -> i64 {
    let reset = (Utc::now() + Duration::minutes(7)).timestamp();
    let mut g = e.m.lock().unwrap();
    g.calls_left = Some(n);
    g.reset_at = reset;
    reset
}

fn unlimit(e: &Env) {
    e.m.lock().unwrap().calls_left = None;
}

fn suite(
    id: i64,
    sha: &str,
    status: &str,
    concl: Option<&str>,
    at: chrono::DateTime<Utc>,
) -> serde_json::Value {
    json!({"id": id, "head_sha": sha, "app": {"id": 15368}, "status": status,
           "conclusion": concl, "updated_at": ts(at)})
}

fn run_in(id: i64, sha: &str, suite: i64, concl: Option<&str>) -> serde_json::Value {
    json!({"id": id, "head_sha": sha, "app": {"id": 15368}, "check_suite": {"id": suite},
           "name": "test", "status": if concl.is_some() { "completed" } else { "in_progress" },
           "conclusion": concl})
}

// ---------------------------------------------------------------- the client

#[tokio::test]
async fn client_auth_pages_and_limits() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let now = Utc::now();
    for n in 1..=5 {
        put_pr(
            &e,
            pr(
                n,
                &format!("pr {n}"),
                "b",
                "s",
                "open",
                false,
                now - Duration::minutes(n),
            ),
        );
    }
    e.m.lock().unwrap().page_size = 2;

    // JWT accepted (mock verifies RS256 with the public key), token issued once, pages followed.
    let all = e
        .api
        .list_pulls(INST, "acme/app", "open", None)
        .await
        .unwrap();
    let nums: Vec<i64> = all.iter().map(|p| p["number"].as_i64().unwrap()).collect();
    assert_eq!(nums, vec![1, 2, 3, 4, 5], "newest first, all three pages");
    assert_eq!(calls(&e.m, "list_pulls"), 3);
    assert_eq!(calls(&e.m, "access_tokens"), 1);
    assert_eq!(e.m.lock().unwrap().bad_auth, 0);
    // `since` stops at the first older PR.
    let some = e
        .api
        .list_pulls(INST, "acme/app", "open", Some(now - Duration::seconds(150)))
        .await
        .unwrap();
    assert_eq!(some.len(), 2);
    assert_eq!(calls(&e.m, "access_tokens"), 1, "token cached");
    // 404 → None.
    assert!(
        e.api
            .get_pull(INST, "acme/app", 99)
            .await
            .unwrap()
            .is_none()
    );
    // App-level calls use the JWT.
    let inst = e.api.list_installations().await.unwrap();
    assert_eq!(inst, vec![(INST, "acme".to_string(), false)]);

    // A token with less than 5 minutes left is renewed before use.
    e.m.lock().unwrap().token_ttl = 240;
    let short = HttpGithub::new(&mock::base(&e.m), APP_ID, KEY_PEM, true).unwrap();
    let before = calls(&e.m, "access_tokens");
    short.get_pull(INST, "acme/app", 1).await.unwrap();
    short.get_pull(INST, "acme/app", 1).await.unwrap();
    assert_eq!(
        calls(&e.m, "access_tokens"),
        before + 2,
        "every call renews a short token"
    );
    e.m.lock().unwrap().token_ttl = 3600;
    // A token GitHub revoked: one 401, then a new token.
    e.m.lock().unwrap().tokens.clear();
    let before = calls(&e.m, "access_tokens");
    let err = e.api.get_pull(INST, "acme/app", 1).await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
    e.api.get_pull(INST, "acme/app", 1).await.unwrap();
    assert_eq!(calls(&e.m, "access_tokens"), before + 1);

    // A wrong key: the mock refuses the JWT.
    let other = {
        let out = std::process::Command::new("openssl")
            .args(["genrsa", "-traditional", "2048"])
            .output();
        out.ok().map(|o| String::from_utf8(o.stdout).unwrap())
    };
    if let Some(pem) = other.filter(|p| p.contains("BEGIN RSA")) {
        let bad = HttpGithub::new(&mock::base(&e.m), APP_ID, &pem, true).unwrap();
        let err = bad.list_installations().await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    // Rate limit → RateLimited with GitHub's reset time.
    let reset = limit_after(&e, 0);
    let err = e.api.get_pull(INST, "acme/app", 1).await.unwrap_err();
    let rl = err.downcast_ref::<RateLimited>().expect("rate limited");
    assert_eq!(rl.reset_at.timestamp(), reset);
    unlimit(&e);

    // https only outside test builds' explicit opt-in.
    assert!(HttpGithub::new("http://127.0.0.1:1", APP_ID, KEY_PEM, false).is_err());
    assert!(HttpGithub::new("https://api.github.com", APP_ID, KEY_PEM, false).is_ok());
}

// ---------------------------------------------------------------- backfill (T-G13)

#[tokio::test]
async fn backfill_pauses_and_resumes() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let key = mk(&e).await;
    let now = Utc::now();
    put_pr(
        &e,
        pr(
            1,
            &format!("{key} one"),
            "b1",
            "h1",
            "open",
            false,
            now - Duration::hours(1),
        ),
    );
    put_pr(
        &e,
        pr(
            2,
            "two",
            "b2",
            "h2",
            "open",
            false,
            now - Duration::hours(2),
        ),
    );
    put_pr(
        &e,
        pr(
            3,
            "merged ten days ago",
            "b3",
            "h3",
            "closed",
            true,
            now - Duration::days(10),
        ),
    );
    put_pr(
        &e,
        pr(
            4,
            "closed long ago",
            "b4",
            "h4",
            "closed",
            false,
            now - Duration::days(40),
        ),
    );
    e.m.lock().unwrap().checks.insert(
        "h1".into(),
        (
            vec![suite(70, "h1", "completed", Some("success"), now)],
            vec![run_in(71, "h1", 70, Some("success"))],
            vec![],
        ),
    );
    let l = link(&e, "backfilling", None).await;

    // T-G13: the limit is spent part-way.
    let reset = limit_after(&e, 3);
    assert_eq!(sync::backfill_once(&e.w.app, &e.api, now).await.unwrap(), 0);
    let (st, through, until) = link_state(&e, l).await;
    assert_eq!(st, "paused");
    assert_eq!(through, None);
    assert_eq!(until.unwrap().timestamp(), reset);
    unlimit(&e);
    // Before the reset: stays paused, nothing runs.
    assert_eq!(sync::resume_paused_once(&e.w.app, now).await.unwrap(), 0);
    assert_eq!(sync::backfill_once(&e.w.app, &e.api, now).await.unwrap(), 0);
    // After the reset: back to backfilling, then done.
    let later = now + Duration::minutes(8);
    assert_eq!(sync::resume_paused_once(&e.w.app, later).await.unwrap(), 1);
    assert_eq!(link_state(&e, l).await.0, "backfilling");
    assert_eq!(
        sync::backfill_once(&e.w.app, &e.api, later).await.unwrap(),
        1
    );
    let (st, through, until) = link_state(&e, l).await;
    assert_eq!((st.as_str(), until), ("ok", None));
    assert_eq!(
        through.unwrap().timestamp(),
        later.timestamp(),
        "watermark = backfill start"
    );
    assert_eq!(pr_state(&e, 1).await.unwrap().1.as_deref(), Some("success"));
    assert_eq!(pr_state(&e, 3).await.unwrap().0, "merged");
    assert!(
        pr_state(&e, 4).await.is_none(),
        "closed over 30 days ago is not taken"
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM pr_issue_refs").await, 1);
}

// ---------------------------------------------------------------- reconcile

#[tokio::test]
async fn reconcile_watermark() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let key = mk(&e).await;
    let now = Utc::now();
    let mark = now - Duration::hours(1);
    let l = link(&e, "ok", Some(mark)).await;

    // T-G30: created and merged between two reconciles; no webhook, no redelivery.
    put_pr(
        &e,
        pr(
            10,
            &format!("fix {key}"),
            "f",
            "m1",
            "closed",
            true,
            now - Duration::minutes(20),
        ),
    );
    // An old closed PR outside the watermark is not touched.
    put_pr(
        &e,
        pr(
            11,
            "old",
            "o",
            "m2",
            "closed",
            false,
            now - Duration::days(3),
        ),
    );
    assert_eq!(
        sync::reconcile_full_once(&e.w.app, &e.api, now)
            .await
            .unwrap(),
        1
    );
    assert_eq!(pr_state(&e, 10).await.unwrap().0, "merged", "T-G30");
    assert!(pr_state(&e, 11).await.is_none());
    assert_eq!(
        count(&pool, "SELECT count(*) FROM pr_issue_refs").await,
        1,
        "refs too"
    );
    assert_eq!(
        link_state(&e, l).await.1.unwrap().timestamp(),
        now.timestamp()
    );

    // T-G31: rate limit on page 2 of (a): the watermark stays; the next run
    // starts from it again and gets everything.
    let now2 = now + Duration::hours(1);
    for n in 20..26 {
        put_pr(
            &e,
            pr(
                n,
                &format!("n{n}"),
                "x",
                "y",
                "open",
                false,
                now2 - Duration::minutes(n),
            ),
        );
    }
    e.m.lock().unwrap().page_size = 2;
    let base_calls = calls(&e.m, "list_pulls");
    // installations, repositories, page 1 of (a); page 2 is refused.
    limit_after(&e, 3);
    assert_eq!(
        sync::reconcile_full_once(&e.w.app, &e.api, now2)
            .await
            .unwrap(),
        0
    );
    assert!(calls(&e.m, "list_pulls") > base_calls);
    let (st, through, _) = link_state(&e, l).await;
    assert_eq!(st, "paused");
    assert_eq!(
        through.unwrap().timestamp(),
        now.timestamp(),
        "T-G31: not advanced"
    );
    assert!(
        pr_state(&e, 25).await.is_none(),
        "the last page was never read"
    );
    unlimit(&e);
    sync::resume_paused_once(&e.w.app, now2 + Duration::minutes(8))
        .await
        .unwrap();
    let now3 = now2 + Duration::minutes(10);
    assert_eq!(
        sync::reconcile_full_once(&e.w.app, &e.api, now3)
            .await
            .unwrap(),
        1
    );
    for n in 20..26 {
        assert!(pr_state(&e, n).await.is_some(), "PR {n} after the rerun");
    }
    assert_eq!(
        link_state(&e, l).await.1.unwrap().timestamp(),
        now3.timestamp()
    );

    // (b): locally open, GitHub says closed, and its update is before the watermark.
    e.m.lock().unwrap().page_size = 100;
    let stale = pr(
        20,
        "n20",
        "x",
        "y",
        "closed",
        false,
        now3 - Duration::hours(5),
    );
    put_pr(&e, stale);
    let now4 = now3 + Duration::hours(1);
    sync::reconcile_full_once(&e.w.app, &e.api, now4)
        .await
        .unwrap();
    // The stored row is newer (updated now2-20m) than GitHub's (now3-5h): R-G8 keeps it.
    // Make GitHub's newer than the row but older than the watermark.
    let fresh = pr(
        20,
        "n20",
        "x",
        "y",
        "closed",
        false,
        now2 - Duration::minutes(5),
    );
    put_pr(&e, fresh);
    sync::reconcile_full_once(&e.w.app, &e.api, now4 + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(
        pr_state(&e, 20).await.unwrap().0,
        "closed",
        "(b) refetched by number"
    );

    // T-G34: a merged PR with CI still pending locally, updated long before
    // the watermark; CI finished, webhook and redelivery both lost.
    let old = now4 - Duration::days(9);
    put_pr(&e, pr(30, "ci late", "c", "c1", "closed", true, old));
    e.m.lock().unwrap().checks.insert(
        "c1".into(),
        (
            vec![suite(80, "c1", "in_progress", None, old)],
            vec![run_in(81, "c1", 80, None)],
            vec![],
        ),
    );
    let v = e.api.get_pull(INST, "acme/app", 30).await.unwrap().unwrap();
    let row = sync::LinkRow {
        id: l,
        repo: e.repo,
        inst: INST,
        full_name: "acme/app".into(),
        synced_through: None,
    };
    sync::sync_pr(&e.w.app, &e.api, &row, &v).await.unwrap();
    assert_eq!(
        pr_state(&e, 30).await.unwrap().1.as_deref(),
        Some("pending")
    );
    e.m.lock().unwrap().checks.insert(
        "c1".into(),
        (
            vec![suite(80, "c1", "completed", Some("success"), now4)],
            vec![run_in(81, "c1", 80, Some("success"))],
            vec![],
        ),
    );
    sync::reconcile_full_once(&e.w.app, &e.api, now4 + Duration::hours(2))
        .await
        .unwrap();
    assert_eq!(
        pr_state(&e, 30).await.unwrap().1.as_deref(),
        Some("success"),
        "T-G34 (d)"
    );

    // T-G15: renamed on GitHub; same numeric id, link kept.
    e.m.lock().unwrap().repos.insert(
        INST,
        vec![json!({"id": REPO, "full_name": "acme/app-renamed", "private": true})],
    );
    sync::reconcile_full_once(&e.w.app, &e.api, now4 + Duration::hours(3))
        .await
        .unwrap();
    let (name, links): (String, i64) = sqlx::query_as(
        "SELECT r.full_name, (SELECT count(*) FROM project_repo_links l WHERE l.repo_id = r.id AND l.unlinked_at IS NULL)
         FROM github_repos r WHERE r.gh_repo_id = $1",
    )
    .bind(REPO)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((name.as_str(), links), ("acme/app-renamed", 1), "T-G15");
    // Removed from the installation → inaccessible, link kept.
    e.m.lock().unwrap().repos.insert(INST, vec![]);
    sync::reconcile_full_once(&e.w.app, &e.api, now4 + Duration::hours(4))
        .await
        .unwrap();
    let st: String = sqlx::query_scalar("SELECT state FROM github_repos WHERE gh_repo_id = $1")
        .bind(REPO)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "inaccessible");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM project_repo_links WHERE unlinked_at IS NULL"
        )
        .await,
        1
    );
}

// ---------------------------------------------------------------- redelivery (T-G21)

#[tokio::test]
async fn redelivery() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let key = mk(&e).await;
    link(&e, "ok", Some(Utc::now())).await;
    let payload = json!({"action": "opened", "installation": {"id": INST},
        "repository": {"id": REPO, "full_name": "acme/app", "private": true},
        "pull_request": pr(40, &format!("{key} lost"), "l", "z", "open", false, Utc::now())});
    put_pr(&e, payload["pull_request"].clone());

    // The delivery's transaction fails before commit.
    faults::arm("github.webhook.before_commit", Action::Fail);
    let lost = Uuid::now_v7().to_string();
    assert_eq!(
        deliver(&e.m, "pull_request", &lost, &payload).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_deliveries").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_event_jobs").await,
        0,
        "no delivery without job"
    );
    // One that worked, and one GitHub failed to send but we did receive.
    let fine = Uuid::now_v7().to_string();
    assert_eq!(
        deliver(&e.m, "ping", &fine, &json!({"zen": "x"})).await,
        StatusCode::ACCEPTED
    );
    {
        let mut g = e.m.lock().unwrap();
        let id = g.next_delivery;
        g.next_delivery += 1;
        g.deliveries.push(
            json!({"id": id, "guid": fine, "status_code": 502, "delivered_at": ts(Utc::now())}),
        );
    }

    let now = Utc::now();
    let mut asked = std::collections::HashSet::new();
    assert_eq!(
        sync::redeliver_once(&e.w.app, &e.api, now, &mut asked)
            .await
            .unwrap(),
        1,
        "only the lost one"
    );
    assert_eq!(calls(&e.m, "redeliver"), 1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_deliveries").await,
        2
    );
    let api: &dyn GithubApi = &e.api;
    jobs::drain(&e.w.app, api).await.unwrap();
    assert_eq!(pr_state(&e, 40).await.unwrap().2, format!("{key} lost"));
    // Asked again: GitHub now lists a 202 for that guid → nothing to do.
    assert_eq!(
        sync::redeliver_once(&e.w.app, &e.api, now, &mut asked)
            .await
            .unwrap(),
        0
    );

    // Retention: deliveries older than 7 days go; a PR of a repo unlinked 31 days ago goes.
    exec(
        &pool,
        "UPDATE github_deliveries SET received_at = now() - interval '8 days'",
    )
    .await;
    exec(
        &pool,
        "UPDATE project_repo_links SET unlinked_at = now() - interval '31 days', unlinked_by = linked_by",
    )
    .await;
    let (d, r) = sync::sweep_once(&e.w.app, Utc::now()).await.unwrap();
    assert_eq!((d, r), (2, 1));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM github_pull_requests").await,
        0
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM pr_issue_refs").await, 0);
}

// ---------------------------------------------------------------- T-G16

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn no_secrets_in_logs() {
    let buf = Buf::default();
    let w = buf.clone();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || w.clone())
        .finish();
    let _g = tracing::subscriber::set_default(sub);

    let pool = test_pool().await;
    let e = setup(&pool).await;
    let l = link(&e, "backfilling", None).await;
    let marker = "PAYLOAD-MARKER-客戶名稱";
    put_pr(
        &e,
        pr(1, marker, "secret-branch", "h", "open", false, Utc::now()),
    );
    // An API error part-way (commits answer 500) and a bad signature.
    e.m.lock().unwrap().fail_path = Some("/commits".into());
    assert_eq!(
        sync::backfill_once(&e.w.app, &e.api, Utc::now())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        link_state(&e, l).await.0,
        "backfilling",
        "an error is not a pause"
    );
    let body = serde_json::to_vec(&json!({"title": marker})).unwrap();
    let hook = e.m.lock().unwrap().hook.clone().unwrap();
    assert_eq!(
        mock::deliver_with(
            &e.m,
            &hook,
            b"wrong",
            "pull_request",
            &Uuid::now_v7().to_string(),
            body
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    // A failing webhook event (bad payload) logs too.
    let bad = json!({"installation": {"id": INST}, "repository": {"id": REPO},
                     "pull_request": {"number": 1, "title": marker}});
    deliver(&e.m, "pull_request", &Uuid::now_v7().to_string(), &bad).await;
    let api: &dyn GithubApi = &e.api;
    jobs::drain(&e.w.app, api).await.unwrap();
    // Errors returned to callers.
    e.m.lock().unwrap().fail_path = Some("/pulls/1".into());
    let err = format!(
        "{:#}",
        e.api.get_pull(INST, "acme/app", 1).await.unwrap_err()
    );

    let tokens: Vec<String> =
        e.m.lock()
            .unwrap()
            .tokens
            .iter()
            .map(|t| t.0.clone())
            .collect();
    assert!(!tokens.is_empty());
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("github"), "something was logged: {log}");
    let secret = std::str::from_utf8(SECRET).unwrap();
    for (what, hay) in [("log", &log), ("error", &err)] {
        assert!(!hay.contains(secret), "{what} has the webhook secret");
        assert!(!hay.contains(marker), "{what} has payload text");
        assert!(!hay.contains("secret-branch"), "{what} has payload text");
        assert!(!hay.contains("PRIVATE KEY"), "{what} has the key");
        for t in &tokens {
            assert!(
                !hay.contains(t.as_str()),
                "{what} has an installation token"
            );
        }
        assert!(!hay.contains("Bearer ey"), "{what} has the JWT");
    }
    assert!(!format!("{:?}", e.api).contains("PRIVATE"));
    let s = GithubSettings::from_vars(|k| match k {
        "OXLEDGER_GITHUB_APP_ID" => Some("1".into()),
        "OXLEDGER_GITHUB_PRIVATE_KEY_FILE" => Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/github-test-app.pem"
            )
            .into(),
        ),
        "OXLEDGER_GITHUB_WEBHOOK_SECRET" => Some(secret.into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let dbg = format!("{s:?}");
    assert!(!dbg.contains(secret) && !dbg.contains("PRIVATE"), "{dbg}");
    assert_eq!(s.api_url, "https://api.github.com");
    // Partial settings are an error, none is off.
    assert!(
        GithubSettings::from_vars(|k| (k == "OXLEDGER_GITHUB_APP_ID").then(|| "1".into())).is_err()
    );
    assert!(GithubSettings::from_vars(|_| None).unwrap().is_none());
}

// ---------------------------------------------------------------- reconcile edges

#[tokio::test]
async fn reconcile_edges() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let now = Utc::now();
    let mark = now - Duration::hours(1);
    let l = link(&e, "ok", Some(mark)).await;
    let row = sync::LinkRow {
        id: l,
        repo: e.repo,
        inst: INST,
        full_name: "acme/app".into(),
        synced_through: Some(mark),
    };

    // (d): merged 3 days ago → CI asked again; merged 9 days ago with CI done → not.
    for (n, sha, age) in [(60, "d3", 3), (61, "d9", 9)] {
        let at = now - Duration::days(age);
        put_pr(&e, pr(n, "ci", "c", sha, "closed", true, at));
        e.m.lock().unwrap().checks.insert(
            sha.into(),
            (
                vec![suite(n * 10, sha, "completed", Some("success"), at)],
                vec![run_in(n * 10 + 1, sha, n * 10, Some("success"))],
                vec![],
            ),
        );
        let v = e
            .api
            .get_pull(INST, "acme/app", n as i32)
            .await
            .unwrap()
            .unwrap();
        sync::sync_pr(&e.w.app, &e.api, &row, &v).await.unwrap();
        assert_eq!(
            pr_state(&e, n as i32).await.unwrap().1.as_deref(),
            Some("success")
        );
        e.m.lock().unwrap().checks.insert(
            sha.into(),
            (
                vec![suite(n * 10, sha, "completed", Some("failure"), now)],
                vec![run_in(n * 10 + 1, sha, n * 10, Some("failure"))],
                vec![],
            ),
        );
    }

    // The watermark has 10 minutes of slack: 5 minutes before it is read, 15 is not.
    put_pr(
        &e,
        pr(
            50,
            "slack",
            "s",
            "w1",
            "closed",
            false,
            mark - Duration::minutes(5),
        ),
    );
    put_pr(
        &e,
        pr(
            51,
            "too old",
            "s",
            "w2",
            "closed",
            false,
            mark - Duration::minutes(15),
        ),
    );
    sync::reconcile_full_once(&e.w.app, &e.api, now)
        .await
        .unwrap();
    assert!(pr_state(&e, 50).await.is_some(), "inside the slack");
    assert!(pr_state(&e, 51).await.is_none(), "outside the slack");
    assert_eq!(
        pr_state(&e, 60).await.unwrap().1.as_deref(),
        Some("failure"),
        "(d) closed within 7 days"
    );
    assert_eq!(
        pr_state(&e, 61).await.unwrap().1.as_deref(),
        Some("success"),
        "(d) closed 9 days ago, CI done: left"
    );

    // Retention edges: a 6-day delivery stays; an unlinked repo 29 days ago
    // stays; a repo still linked by another project stays.
    exec(
        &pool,
        "INSERT INTO github_deliveries (delivery_id, event, received_at, processed_at, result)
         VALUES (uuidv7(), 'ping', now() - interval '6 days', now(), 'ignored'),
                (uuidv7(), 'ping', now() - interval '8 days', now(), 'ignored')",
    )
    .await;
    exec(
        &pool,
        "UPDATE project_repo_links SET unlinked_at = now() - interval '29 days', unlinked_by = linked_by",
    )
    .await;
    let (d, r) = sync::sweep_once(&e.w.app, Utc::now()).await.unwrap();
    assert_eq!((d, r), (1, 0), "6 days and 29 days are kept");
    exec(
        &pool,
        "UPDATE project_repo_links SET unlinked_at = now() - interval '31 days'",
    )
    .await;
    sqlx::query(
        "INSERT INTO project_repo_links (project_id, repo_id, linked_by, sync_state) VALUES ($1, $2, $3, 'ok')",
    )
    .bind(e.w.web)
    .bind(e.repo)
    .bind(e.w.alice)
    .execute(&pool)
    .await
    .unwrap();
    let (_, r) = sync::sweep_once(&e.w.app, Utc::now()).await.unwrap();
    assert_eq!(r, 0, "still linked elsewhere");
    assert!(count(&pool, "SELECT count(*) FROM github_pull_requests").await > 0);
}
