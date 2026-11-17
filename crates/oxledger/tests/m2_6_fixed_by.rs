// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.6 (§3.8, R-F1–R-F11): `fixed_by`, without any network (a
//! fake `GithubApi`): T-M2-67, 68, 68a, 68b, 69, 70, 70a, 70b, 71, 71a, 72,
//! 73, 74, 74a, 75, 77 and the rules API. Decision A3 (a) + (i): the
//! sponsor's permissions, pull request merges only. Needs PostgreSQL 18;
//! see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use chrono::{DateTime, Duration, Utc};
use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::commands::{self, SYSTEM_PRINCIPAL};
use oxledger::error::ErrorCode;
use oxledger::github::client::{Fut, GithubApi, HookDelivery};
use oxledger::github::fixes::{self, DevFix, FixRuleInput, FixRules, FixRulesPut};
use oxledger::github::links::{self, LinkInput};
use oxledger::github::rollup::Review;
use oxledger::github::{jobs, read, webhook};
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::{issue_view, project_tabs, undo, views};
use serde_json::{Value, json};
use std::sync::Mutex;
use uuid::Uuid;
use world::*;

const INST: i64 = 61;
const REPO: i64 = 7001;

// ---------------------------------------------------------------- fake GitHub

/// What `GET /pulls/{n}` answers.
#[derive(Clone)]
enum Pull {
    Merged(&'static str),
    Open,
    Missing,
    Error,
}

struct Fake {
    pull: Mutex<Pull>,
    pull_calls: Mutex<usize>,
    commits: Mutex<Vec<String>>,
    compare_calls: Mutex<usize>,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            pull: Mutex::new(Pull::Merged("main")),
            pull_calls: Mutex::new(0),
            commits: Mutex::new(Vec::new()),
            compare_calls: Mutex::new(0),
        }
    }
}

impl Fake {
    fn calls(&self) -> usize {
        *self.pull_calls.lock().unwrap()
    }
    fn answer(&self, p: Pull) {
        *self.pull.lock().unwrap() = p;
    }
}

impl GithubApi for Fake {
    fn get_pull<'a>(&'a self, _: i64, repo: &'a str, n: i32) -> Fut<'a, Option<Value>> {
        Box::pin(async move {
            assert_eq!(repo, "acme/app");
            *self.pull_calls.lock().unwrap() += 1;
            let p = self.pull.lock().unwrap().clone();
            match p {
                Pull::Merged(base) => {
                    let mut v = pr(n, "forged or not, the API says merged", base);
                    v["merged"] = json!(true);
                    Ok(Some(v))
                }
                Pull::Open => {
                    let mut v = pr(n, "t", "main");
                    v["merged"] = json!(false);
                    v["merged_at"] = Value::Null;
                    v["state"] = json!("open");
                    Ok(Some(v))
                }
                Pull::Missing => Ok(None),
                Pull::Error => Err(anyhow::anyhow!("github is down")),
            }
        })
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
        Box::pin(async { Ok(self.commits.lock().unwrap().clone()) })
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
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> Fut<'a, Vec<Value>> {
        Box::pin(async {
            *self.compare_calls.lock().unwrap() += 1;
            Ok(vec![])
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
    /// Manager of AUTH and WEB with the Dev workflow (New → Doing).
    alice: AuthContext,
    /// Manager of AUTH: no workflow, does not see private issues.
    carol: AuthContext,
    /// System administrator, no membership.
    admin: AuthContext,
    /// A member who manages nothing.
    bob: AuthContext,
    repo: Uuid,
    mgr2: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> Env {
    let w = World::build(pool).await;
    // The system principal of 0008 (the world's reset truncates principals):
    // the integration writes as it.
    exec(
        pool,
        "INSERT INTO principals (id, kind, name) VALUES ('00000000-0000-7000-8000-000000000001', 'system', 'oxledger')
         ON CONFLICT DO NOTHING",
    )
    .await;
    exec(
        pool,
        "TRUNCATE github_event_jobs, github_deliveries, pr_issue_refs, project_pr_hidden, issue_fix_links,
                  project_fix_rules, issue_agent_chains,
                  commit_issue_refs, project_commit_hidden, project_commit_revocations, github_commits,
                  github_commit_statuses, github_check_suites, github_check_runs,
                  github_pull_requests, project_repo_links, github_repos, github_installations CASCADE",
    )
    .await;
    exec(
        pool,
        "UPDATE project_tab_settings SET fix_rules_version = 0",
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
    grant(
        pool,
        mgr2,
        &["view_issues", "edit_issues", "manage_project"],
    )
    .await;
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
        "INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private, linkable, state,
                                   default_branch)
         VALUES ($1, $2, 'acme/app', true, true, 'accessible', 'main') RETURNING id",
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
        mgr2,
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

fn rule(enabled: bool, keywords: &[&str], target: Uuid) -> FixRuleInput {
    FixRuleInput {
        id: None,
        enabled,
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        target_status_id: target,
        tracker_ids: None,
    }
}

async fn put_rules(
    e: &Env,
    who: AuthContext,
    project: &str,
    rules: Vec<FixRuleInput>,
) -> anyhow::Result<FixRules> {
    let lock_version = fixes::get(&e.w.app, who, project).await?.lock_version;
    fixes::put(
        &e.w.app,
        who,
        Uuid::now_v7(),
        project,
        &FixRulesPut {
            rules,
            lock_version,
        },
    )
    .await
}

/// AUTH: `fixes`/`fix` → Doing, sponsored by `who`.
async fn standard_rule(e: &Env, who: AuthContext) -> Uuid {
    put_rules(
        e,
        who,
        "AUTH",
        vec![rule(true, &["fixes", "Fix"], e.w.doing)],
    )
    .await
    .unwrap()
    .rules[0]
        .id
}

fn sha(n: u64) -> String {
    format!("{n:040x}")
}

fn pr(n: i32, title: &str, base: &str) -> Value {
    json!({"number": n, "title": title, "body": "", "head": {"ref": format!("feat/{n}"), "sha": sha(n as u64)},
           "base": {"ref": base, "repo": {"default_branch": "main"}}, "state": "closed", "draft": false, "merged": true,
           "merged_at": "2026-11-02T00:00:00Z",
           "html_url": format!("https://github.com/acme/app/pull/{n}"),
           "updated_at": "2026-11-02T00:00:00Z", "requested_reviewers": [], "requested_teams": []})
}

fn repository() -> Value {
    json!({"id": REPO, "full_name": "acme/app", "private": true, "default_branch": "main"})
}

fn merged(n: i32, title: &str, body: &str, base: &str) -> Value {
    let mut v = pr(n, title, base);
    v["body"] = json!(body);
    json!({"action": "closed", "pull_request": v, "installation": {"id": INST}, "repository": repository()})
}

fn commit(n: u64, message: &str) -> Value {
    json!({"id": sha(n), "message": message, "timestamp": "2026-11-01T08:00:00+08:00",
           "author": {"username": "mallory"}, "distinct": true})
}

fn push_to(branch: &str, commits: Vec<Value>) -> Value {
    json!({"ref": format!("refs/heads/{branch}"), "before": sha(0xb), "after": sha(0xa),
           "deleted": false, "installation": {"id": INST}, "repository": repository(),
           "sender": {"login": "bob"}, "commits": commits})
}

/// Enqueues an event, runs the job worker, and gives the delivery's result.
async fn deliver(e: &Env, api: &Fake, event: &str, payload: &Value) -> String {
    let d = Uuid::now_v7();
    assert!(webhook::enqueue(&e.w.app, d, event, payload).await.unwrap());
    jobs::drain(&e.w.app, api).await.unwrap();
    sqlx::query_scalar("SELECT result FROM github_deliveries WHERE delivery_id = $1")
        .bind(d)
        .fetch_one(&e.w.pool)
        .await
        .unwrap()
}

/// The worker's confirmation pass, as of now.
async fn work(e: &Env, api: &Fake) {
    fixes::run_pending_once(&e.w.app, api, Utc::now())
        .await
        .unwrap();
}

/// (state, reason) of the issue's fix links, oldest first.
async fn states(e: &Env, issue: Uuid) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT state, reason FROM issue_fix_links WHERE issue_id = $1 ORDER BY detected_at, id",
    )
    .bind(issue)
    .fetch_all(&e.w.pool)
    .await
    .unwrap()
}

fn st(state: &str, reason: Option<&str>) -> Vec<(String, Option<String>)> {
    vec![(state.to_string(), reason.map(String::from))]
}

async fn status_of(e: &Env, issue: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT status_id FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&e.w.pool)
        .await
        .unwrap()
}

async fn transitions(e: &Env, issue: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM issue_events WHERE issue_id = $1 AND kind = 'status_changed'",
    )
    .bind(issue)
    .fetch_one(&e.w.pool)
    .await
    .unwrap()
}

async fn dev_fixes(e: &Env, who: AuthContext, key: &str) -> Vec<DevFix> {
    read::issue_dev(&e.w.app, who, key).await.unwrap().fix_links
}

// ---------------------------------------------------------------- rules API (§6.9)

#[tokio::test]
async fn rules_are_validated_and_sponsored() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    // manage_project only.
    for who in [e.bob] {
        let err = fixes::get(app, who, "AUTH").await.unwrap_err();
        assert!(matches!(
            code(&err),
            ErrorCode::ForbiddenOp | ErrorCode::NotFound
        ));
    }
    let err = put_rules(&e, e.bob, "AUTH", vec![rule(true, &["fixes"], e.w.doing)])
        .await
        .unwrap_err();
    assert!(matches!(
        code(&err),
        ErrorCode::ForbiddenOp | ErrorCode::NotFound
    ));
    let v = fixes::get(app, e.alice, "AUTH").await.unwrap();
    assert!(v.rules.is_empty() && !v.has_linked_repo && v.lock_version == 0);
    assert_eq!(v.default_keywords.len(), 9);
    assert_eq!(v.trackers.len(), 1);
    assert!(v.statuses.iter().any(|s| s.id == e.w.doing));

    let bad = |r: Vec<FixRuleInput>| async {
        let err = put_rules(&e, e.alice, "AUTH", r).await.unwrap_err();
        assert_eq!(code(&err), ErrorCode::FixRuleInvalid);
        let d = oxledger::error::classify(&err).unwrap().details.unwrap();
        (
            d["reason"].as_str().unwrap().to_string(),
            d["index"].as_u64().unwrap(),
        )
    };
    let d = e.w.doing;
    assert_eq!(
        bad(vec![rule(true, &[], d)]).await,
        ("keywords_empty".into(), 0)
    );
    assert_eq!(
        bad(vec![rule(true, &["ok"], d), rule(true, &["two words"], d)]).await,
        ("keyword_invalid".into(), 1)
    );
    assert_eq!(
        bad(vec![rule(true, &["fix"], d), rule(false, &["FIX"], d)]).await,
        ("keyword_duplicate".into(), 1)
    );
    let eleven: Vec<String> = (0..11).map(|i| format!("k{i}")).collect();
    let eleven: Vec<&str> = eleven.iter().map(String::as_str).collect();
    assert_eq!(
        bad(vec![rule(true, &eleven, d)]).await,
        ("too_many_keywords".into(), 0)
    );
    assert_eq!(
        bad(vec![rule(true, &["fix"], Uuid::now_v7())]).await,
        ("status_unknown".into(), 0)
    );
    let mut t = rule(true, &["fix"], d);
    t.tracker_ids = Some(vec![Uuid::now_v7()]);
    assert_eq!(bad(vec![t]).await, ("tracker_not_in_project".into(), 0));
    let six: Vec<FixRuleInput> = (0..6)
        .map(|i| FixRuleInput {
            keywords: vec![format!("k{i}")],
            ..rule(false, &[], d)
        })
        .collect();
    assert_eq!(bad(six).await.0, "too_many_rules");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_fix_rules").await,
        0
    );

    // alice saves two rules: stored lower case, she sponsors both.
    let v = put_rules(
        &e,
        e.alice,
        "AUTH",
        vec![
            rule(true, &["Fixes", "修正"], d),
            rule(false, &["closes"], e.w.done),
        ],
    )
    .await
    .unwrap();
    assert_eq!(v.lock_version, 1);
    assert_eq!(v.rules[0].keywords, vec!["fixes", "修正"]);
    assert!(v.rules.iter().all(|r| r.sponsor.id == e.w.alice));
    assert_eq!((v.rules[0].enabled, v.rules[1].enabled), (true, false));

    // A stale version is a conflict.
    let err = fixes::put(
        app,
        e.carol,
        Uuid::now_v7(),
        "AUTH",
        &FixRulesPut {
            rules: vec![],
            lock_version: 0,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);

    // carol turns the second on and leaves the first as it is: she becomes
    // the sponsor of the one she touched only (R-F8).
    let keep = |r: &fixes::FixRule, enabled| FixRuleInput {
        id: Some(r.id),
        enabled,
        keywords: r.keywords.clone(),
        target_status_id: r.target_status_id,
        tracker_ids: r.tracker_ids.clone(),
    };
    let v2 = put_rules(
        &e,
        e.carol,
        "AUTH",
        vec![keep(&v.rules[0], true), keep(&v.rules[1], true)],
    )
    .await
    .unwrap();
    assert_eq!(v2.rules[0].sponsor.id, e.w.alice);
    assert_eq!(v2.rules[1].sponsor.id, e.w.carol);
    // Turning one off records why; the sponsor stays.
    let v3 = put_rules(
        &e,
        e.alice,
        "AUTH",
        vec![keep(&v2.rules[0], true), keep(&v2.rules[1], false)],
    )
    .await
    .unwrap();
    assert_eq!(v3.rules[1].disabled_reason.as_deref(), Some("manual"));
    assert_eq!(v3.rules[1].sponsor.id, e.w.carol);
    // The same list again changes nothing: no new version, no event.
    let events = count(
        &pool,
        "SELECT count(*) FROM project_setting_events WHERE kind = 'fix_rule_changed'",
    )
    .await;
    assert_eq!(events, 3);
    let v4 = put_rules(
        &e,
        e.carol,
        "AUTH",
        vec![keep(&v3.rules[0], true), keep(&v3.rules[1], false)],
    )
    .await
    .unwrap();
    assert_eq!(v4.lock_version, v3.lock_version);
    assert_eq!(v4.rules[0].sponsor.id, e.w.alice);
    // Removing a rule; the same Idempotency-Key answers with the current list.
    let key = Uuid::now_v7();
    let input = FixRulesPut {
        rules: vec![keep(&v4.rules[0], true)],
        lock_version: v4.lock_version,
    };
    let once = fixes::put(app, e.alice, key, "AUTH", &input).await.unwrap();
    let again = fixes::put(app, e.alice, key, "AUTH", &input).await.unwrap();
    assert_eq!((once.rules.len(), again.rules.len()), (1, 1));
    assert_eq!(once.lock_version, again.lock_version);
    // The history of the settings names who changed the rules.
    let kinds: Vec<String> = project_tabs::events(app, e.alice, "AUTH", None)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.kind)
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "fix_rule_changed").count(), 4);
}

// ---------------------------------------------------------------- T-M2-67, 69, 72, 73, 74, 74a

#[tokio::test]
async fn a_merged_pr_moves_the_issues() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    link_to(&e, "AUTH").await;
    let rule_id = standard_rule(&e, e.alice).await;
    let (i1, a1) = issue(&e, e.w.auth_p, false).await;
    let (i2, a2) = issue(&e, e.w.auth_p, false).await;
    let (i3, a3) = issue(&e, e.w.auth_p, false).await;
    let (i4, a4) = issue(&e, e.w.auth_p, false).await;
    // T-M2-73: an agent chain on the issue is neither reset nor extended.
    let exec_ref = id(
        &pool,
        "INSERT INTO execution_refs (id, kind) VALUES (uuidv7(), 'command') RETURNING id",
    )
    .await;
    sqlx::query(
        "INSERT INTO issue_agent_chains (issue_id, started_at, started_by_execution_id, writes, depth,
                                         last_agent_principal_id, agents, updated_at)
         VALUES ($1, now(), $2, 2, 0, $3, ARRAY[$3], now())",
    )
    .bind(i1)
    .bind(exec_ref)
    .bind(e.w.alice)
    .execute(&pool)
    .await
    .unwrap();

    let api = Fake::default();
    *api.commits.lock().unwrap() = vec![format!("wip\n\nFix: {a3}")];
    let title = format!("TOP-SECRET-TITLE fixes {a1}, {a2}");
    // `closes` is GitHub's keyword but no rule of AUTH: a hint only.
    let body = format!("see the notes.\ncloses {a4}");
    assert_eq!(
        deliver(&e, &api, "pull_request", &merged(12, &title, &body, "main")).await,
        "ok"
    );
    for i in [i1, i2, i3] {
        assert_eq!(states(&e, i).await, st("pending", None));
        assert_eq!(
            status_of(&e, i).await,
            e.w.new,
            "nothing before GitHub confirms"
        );
    }
    assert_eq!(states(&e, i4).await, st("suggested", Some("no_rule")));
    assert_eq!(api.calls(), 0);

    work(&e, &api).await;
    assert_eq!(api.calls(), 1, "one confirmation for the pull request");
    for i in [i1, i2, i3] {
        assert_eq!(states(&e, i).await, st("applied", None));
        assert_eq!(status_of(&e, i).await, e.w.doing);
    }
    assert_eq!(status_of(&e, i4).await, e.w.new);

    // The event: the integration, for the sponsor, through the engine.
    type Ev = (
        String,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Value,
        bool,
        bool,
    );
    let ev: Ev = sqlx::query_as(
        "SELECT origin, actor_id, actor_kind, on_behalf_of_id, system_operation, payload,
                command_execution_id IS NOT NULL, operation_request_id IS NOT NULL
         FROM issue_events WHERE issue_id = $1 AND kind = 'status_changed'",
    )
    .bind(i1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (ev.0.as_str(), ev.1, ev.2.as_str()),
        ("integration", SYSTEM_PRINCIPAL, "system")
    );
    assert_eq!((ev.3, ev.4.as_deref()), (Some(e.w.alice), Some("fixed_by")));
    assert!(ev.6 && ev.7);
    assert_eq!(
        ev.5["fix"],
        json!({"repo": "acme/app", "pr_number": 12, "keyword": "fixes"})
    );
    assert!(!ev.5.to_string().contains("TOP-SECRET"));
    let (origin, obo): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT x.origin, x.on_behalf_of_id FROM command_executions x
         JOIN issue_fix_links l ON l.execution_id = x.id WHERE l.issue_id = $1",
    )
    .bind(i1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((origin.as_str(), obo), ("integration", Some(e.w.alice)));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_agent_chains WHERE writes = 2"
        )
        .await,
        1,
        "the agent chain is untouched"
    );

    // The history shows the integration and whose rule it was.
    let v = issue_view::get(
        app,
        e.bob,
        issue_view::GetParams {
            key: a1.clone(),
            fields: None,
            citations: None,
            include: Some(vec!["recent_events".into(), "dev".into()]),
            note_limit: None,
            event_limit: None,
        },
    )
    .await
    .unwrap();
    let events = serde_json::to_value(v.recent_events.unwrap()).unwrap();
    let moved = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "status_changed")
        .unwrap()
        .clone();
    assert_eq!(moved["actor_kind"], "system");
    assert_eq!(moved["on_behalf_of"]["id"], json!(e.w.alice));
    assert_eq!(moved["detail"]["fix"]["pr_number"], 12);
    // The card.
    let f = &v.dev.unwrap().fix_links[0];
    assert_eq!(
        (f.state.as_str(), f.pr_number, f.repo.as_str()),
        ("applied", Some(12), "acme/app")
    );
    assert_eq!(f.target_status.as_deref(), Some("Doing"));
    assert_eq!(f.sponsor.as_deref(), Some("alice"));
    let hint = &dev_fixes(&e, e.bob, &a4).await[0];
    assert_eq!(
        (hint.state.as_str(), hint.reason.as_deref()),
        ("suggested", Some("no_rule"))
    );
    assert!(hint.target_status_id.is_none());

    // T-M2-69: the same body under a new delivery id changes nothing.
    assert_eq!(
        deliver(&e, &api, "pull_request", &merged(12, &title, &body, "main")).await,
        "ok"
    );
    work(&e, &api).await;
    assert_eq!(api.calls(), 1);
    assert_eq!(transitions(&e, i1).await, 1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_fix_links").await,
        4
    );

    // T-M2-72: the squash commit of the same fix reaches main by push: a
    // hint (A3 (i)), never a second transition.
    let squash = push_to(
        "main",
        vec![commit(0x51, &format!("squash (#12)\n\nfixes {a1}"))],
    );
    assert_eq!(deliver(&e, &api, "push", &squash).await, "ok");
    work(&e, &api).await;
    assert_eq!(transitions(&e, i1).await, 1);
    assert_eq!(
        states(&e, i1).await,
        vec![
            ("applied".to_string(), None),
            ("suggested".to_string(), Some("direct_push".to_string()))
        ]
    );
    assert_eq!(api.calls(), 1);

    // T-M2-74: the undo route does not take an integration's execution.
    let execution: Uuid = sqlx::query_scalar(
        "SELECT execution_id FROM issue_fix_links WHERE issue_id = $1 AND state = 'applied'",
    )
    .bind(i1)
    .fetch_one(&pool)
    .await
    .unwrap();
    for who in [e.alice, e.admin] {
        let err = undo::apply(
            app,
            who,
            Uuid::now_v7(),
            execution,
            &serde_json::from_value(json!({})).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                code(&err),
                ErrorCode::UndoNotApplicable | ErrorCode::NotFound
            ),
            "{err:#}"
        );
    }
    assert_eq!(status_of(&e, i1).await, e.w.doing);

    // T-M2-74a: the sponsor did not "write" the issues her rule moved.
    let r = serde_json::to_value(
        views::run(
            app,
            e.alice,
            "builtin:involving_me",
            &views::RunInput::default(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let keys: Vec<&str> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert!(
        !keys.contains(&a1.as_str()) && !keys.contains(&a2.as_str()),
        "{keys:?}"
    );

    // A person can move it on by hand afterwards (the engine's ordinary
    // path): the rule's row is not in the way.
    let _ = (rule_id, i2, a3);
}

// ---------------------------------------------------------------- T-M2-68, 68a, 68b, 69 (forged), 75

#[tokio::test]
async fn only_confirmed_merges_into_the_default_branch_count() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    link_to(&e, "AUTH").await;
    link_to(&e, "WEB").await;
    standard_rule(&e, e.alice).await;
    let (i1, a1) = issue(&e, e.w.auth_p, false).await;
    let (w1, wk1) = issue(&e, e.w.web, false).await;
    let api = Fake::default();

    // T-M2-68: merged into another branch; pushed to a feature branch.
    let title = format!("fixes {a1}");
    assert_eq!(
        deliver(&e, &api, "pull_request", &merged(1, &title, "", "release")).await,
        "ok"
    );
    assert_eq!(
        deliver(
            &e,
            &api,
            "push",
            &push_to("feat/x", vec![commit(1, &title)])
        )
        .await,
        "ok"
    );
    // Closed without merging says nothing either.
    let mut closed = merged(2, &title, "", "main");
    closed["pull_request"]["merged"] = json!(false);
    closed["pull_request"]["merged_at"] = Value::Null;
    deliver(&e, &api, "pull_request", &closed).await;
    assert!(states(&e, i1).await.is_empty());

    // T-M2-68a: pushed straight to the default branch: a hint, no API.
    assert_eq!(
        deliver(&e, &api, "push", &push_to("main", vec![commit(2, &title)])).await,
        "ok"
    );
    work(&e, &api).await;
    assert_eq!(states(&e, i1).await, st("suggested", Some("direct_push")));
    assert_eq!(status_of(&e, i1).await, e.w.new);
    let f = &dev_fixes(&e, e.bob, &a1).await[0];
    assert_eq!(
        (f.sha.as_deref(), f.pr_number),
        (Some(sha(2).as_str()), None)
    );
    assert_eq!(
        f.target_status_id,
        Some(e.w.doing),
        "what the dialog preselects"
    );

    // T-M2-68b: WEB links the repository but has no rule.
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(3, &format!("Resolves {wk1}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(states(&e, w1).await, st("suggested", Some("no_rule")));
    assert_eq!(api.calls(), 0, "hints never ask GitHub");
    assert_eq!(api.compare_calls.lock().unwrap().to_owned(), 0);

    // T-M2-69: a payload that says "merged" while the API does not.
    let (i2, a2) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Open);
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(4, &format!("fixes {a2}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(
        states(&e, i2).await,
        st("skipped", Some("not_on_default_branch"))
    );
    // …or the API says it was merged into another branch.
    let (i3, a3) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Merged("release"));
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(5, &format!("fixes {a3}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(
        states(&e, i3).await,
        st("skipped", Some("not_on_default_branch"))
    );
    // …or does not know the pull request.
    let (i4, a4) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Missing);
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(6, &format!("fixes {a4}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(states(&e, i4).await, st("skipped", Some("unconfirmed")));
    for i in [i2, i3, i4] {
        assert_eq!(status_of(&e, i).await, e.w.new);
    }
    // A pull request hidden in the project moves nothing, and its number
    // stays out of the issue's history.
    let (i8, a8) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Merged("main"));
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(8, &format!("fixes {a8}"), "", "main"),
    )
    .await;
    assert_eq!(states(&e, i8).await, st("pending", None));
    sqlx::query(
        "INSERT INTO project_pr_hidden (project_id, pr_id, hidden_by)
         SELECT $1, p.id, $2 FROM github_pull_requests p WHERE p.number = 8",
    )
    .bind(e.w.auth_p)
    .bind(e.w.alice)
    .execute(&e.w.pool)
    .await
    .unwrap();
    work(&e, &api).await;
    assert_eq!(states(&e, i8).await, st("skipped", Some("pr_hidden")));
    assert_eq!(status_of(&e, i8).await, e.w.new);
    assert!(dev_fixes(&e, e.alice, &a8).await.is_empty());
    // R-F3: an old payload names an old default branch; the API's answer
    // decides which branch is the default one.
    let (i9, a9) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Merged("develop"));
    let mut old = merged(9, &format!("fixes {a9}"), "", "develop");
    old["repository"]["default_branch"] = json!("develop");
    deliver(&e, &api, "pull_request", &old).await;
    assert_eq!(states(&e, i9).await, st("pending", None));
    work(&e, &api).await;
    assert_eq!(
        states(&e, i9).await,
        st("skipped", Some("not_on_default_branch"))
    );
    assert_eq!(status_of(&e, i9).await, e.w.new);
    api.answer(Pull::Merged("main"));

    // T-M2-75: the API fails five times, with a growing wait in between.
    let (i5, a5) = issue(&e, e.w.auth_p, false).await;
    api.answer(Pull::Error);
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(7, &format!("fixes {a5}"), "", "main"),
    )
    .await;
    let before = api.calls();
    work(&e, &api).await;
    work(&e, &api).await;
    assert_eq!(api.calls(), before + 1, "not again before the wait is over");
    assert_eq!(states(&e, i5).await, st("pending", None));
    for n in 1..5 {
        fixes::run_pending_once(&e.w.app, &api, Utc::now() + Duration::days(n))
            .await
            .unwrap();
    }
    assert_eq!(api.calls(), before + 5);
    assert_eq!(states(&e, i5).await, st("skipped", Some("unconfirmed")));
    assert_eq!(status_of(&e, i5).await, e.w.new);
    // GitHub is back: a skipped link is not tried again.
    api.answer(Pull::Merged("main"));
    fixes::run_pending_once(&e.w.app, &api, Utc::now() + Duration::days(30))
        .await
        .unwrap();
    assert_eq!(api.calls(), before + 5);
}

// ---------------------------------------------------------------- T-M2-70, 70a, 70b

#[tokio::test]
async fn the_sponsor_decides_what_a_rule_may_do() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    link_to(&e, "AUTH").await;
    let api = Fake::default();
    // carol manages AUTH but has no workflow, and does not see private
    // issues (visibility `default`).
    let carols = standard_rule(&e, e.carol).await;
    let (i1, a1) = issue(&e, e.w.auth_p, false).await;
    let (i2, a2) = issue(&e, e.w.auth_p, true).await;
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(1, &format!("fixes {a1} and {a2}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    // T-M2-70a: no transition and no sight give one answer.
    assert_eq!(
        states(&e, i1).await,
        st("suggested", Some("workflow_disallowed"))
    );
    assert_eq!(
        states(&e, i2).await,
        st("suggested", Some("workflow_disallowed"))
    );
    let strip = |mut f: Vec<DevFix>| {
        let mut v = serde_json::to_value(f.remove(0)).unwrap();
        v["at"] = Value::Null;
        v
    };
    assert_eq!(
        strip(dev_fixes(&e, e.alice, &a1).await),
        strip(dev_fixes(&e, e.alice, &a2).await)
    );
    assert_eq!(
        (status_of(&e, i1).await, status_of(&e, i2).await),
        (e.w.new, e.w.new)
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM command_executions WHERE origin = 'integration'"
        )
        .await,
        0
    );
    let on: bool = sqlx::query_scalar("SELECT enabled FROM project_fix_rules WHERE id = $1")
        .bind(carols)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(on, "a refused transition does not turn the rule off");

    // T-M2-70a, the half that matters: with the transition, the sponsor
    // moves what she can see and nothing else.
    sqlx::query("INSERT INTO workflow_transitions VALUES ($1, $2, $3, $4)")
        .bind(e.w.bug)
        .bind(e.mgr2)
        .bind(e.w.new)
        .bind(e.w.doing)
        .execute(&pool)
        .await
        .unwrap();
    let (seen, s1) = issue(&e, e.w.auth_p, false).await;
    let (unseen, s2) = issue(&e, e.w.auth_p, true).await;
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(90, &format!("fixes {s1}, {s2}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(states(&e, seen).await, st("applied", None));
    assert_eq!(status_of(&e, seen).await, e.w.doing);
    assert_eq!(
        states(&e, unseen).await,
        st("suggested", Some("workflow_disallowed"))
    );
    assert_eq!(status_of(&e, unseen).await, e.w.new);

    // A closed project takes no writes; that is not the sponsor's doing.
    let (shut, s3) = issue(&e, e.w.auth_p, false).await;
    sqlx::query("UPDATE projects SET status = 'closed' WHERE id = $1")
        .bind(e.w.auth_p)
        .execute(&pool)
        .await
        .unwrap();
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(91, &format!("fixes {s3}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    sqlx::query("UPDATE projects SET status = 'active' WHERE id = $1")
        .bind(e.w.auth_p)
        .execute(&pool)
        .await
        .unwrap();
    let shut_state = states(&e, shut).await;
    assert!(
        shut_state == st("suggested", Some("project_inactive")) || shut_state.is_empty(),
        "{shut_state:?}"
    );
    assert_eq!(status_of(&e, shut).await, e.w.new);
    let on: bool = sqlx::query_scalar("SELECT enabled FROM project_fix_rules WHERE id = $1")
        .bind(carols)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(on, "a closed project does not turn the rule off");

    // T-M2-70: the target status needs a field that is empty.
    let alices = standard_rule_replacing(&e, e.alice).await;
    sqlx::query("INSERT INTO workflow_field_rules VALUES ($1, $2, $3, 'due_date', 'required')")
        .bind(e.w.bug)
        .bind(e.w.dev)
        .bind(e.w.doing)
        .execute(&pool)
        .await
        .unwrap();
    let (i3, a3) = issue(&e, e.w.auth_p, false).await;
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(2, &format!("fixes {a3}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(states(&e, i3).await, st("suggested", Some("form_rule")));
    exec(&pool, "DELETE FROM workflow_field_rules").await;

    // T-M2-70, 70b: the sponsor lost manage_project: the rule goes off,
    // once, by the system principal.
    let (i4, a4) = issue(&e, e.w.auth_p, false).await;
    let (i5, a5) = issue(&e, e.w.auth_p, false).await;
    exec(
        &pool,
        "DELETE FROM member_roles WHERE role_id IN (SELECT id FROM roles WHERE name = 'Mgr')",
    )
    .await;
    let event = merged(3, &format!("fixes {a4}, {a5}"), "", "main");
    deliver(&e, &api, "pull_request", &event).await;
    work(&e, &api).await;
    assert_eq!(
        states(&e, i4).await,
        st("suggested", Some("sponsor_lost_permission"))
    );
    // The second issue of the same PR finds the rule already off.
    assert_eq!(states(&e, i5).await, st("suggested", Some("no_rule")));
    let (enabled, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT enabled, disabled_reason FROM project_fix_rules WHERE id = $1")
            .bind(alices)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (enabled, reason.as_deref()),
        (false, Some("sponsor_lost_permission"))
    );
    let auto = "SELECT count(*) FROM project_setting_events e JOIN operation_requests o ON o.id = e.operation_request_id
                WHERE e.kind = 'fix_rule_changed' AND e.actor_id = '00000000-0000-7000-8000-000000000001'
                  AND o.action = 'fix_rule_auto_disable' AND e.payload ? 'reason'";
    assert_eq!(count(&pool, auto).await, 1);
    // A settings page opened before that cannot save the rule back on.
    let now_v: i64 = sqlx::query_scalar(
        "SELECT fix_rules_version FROM project_tab_settings WHERE project_id = $1",
    )
    .bind(e.w.auth_p)
    .fetch_one(&pool)
    .await
    .unwrap();
    let err = fixes::put(
        app,
        e.carol,
        Uuid::now_v7(),
        "AUTH",
        &FixRulesPut {
            rules: vec![rule(true, &["fixes"], e.w.doing)],
            lock_version: now_v - 1,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    deliver(&e, &api, "pull_request", &event).await;
    work(&e, &api).await;
    assert_eq!(
        count(&pool, auto).await,
        1,
        "a resent event disables nothing twice"
    );
    assert_eq!(status_of(&e, i4).await, e.w.new);

    // The sponsor is locked.
    let m: Uuid =
        sqlx::query_scalar("SELECT id FROM members WHERE project_id = $1 AND principal_id = $2")
            .bind(e.w.auth_p)
            .bind(e.w.carol)
            .fetch_one(&pool)
            .await
            .unwrap();
    let _ = (m, e.mgr2);
    let carols = standard_rule_replacing(&e, e.carol).await;
    sqlx::query("UPDATE principals SET status = 'locked' WHERE id = $1")
        .bind(e.w.carol)
        .execute(&pool)
        .await
        .unwrap();
    let (i6, a6) = issue(&e, e.w.auth_p, false).await;
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(4, &format!("fixes {a6}"), "", "main"),
    )
    .await;
    work(&e, &api).await;
    assert_eq!(
        states(&e, i6).await,
        st("suggested", Some("sponsor_inactive"))
    );
    let reason: Option<String> =
        sqlx::query_scalar("SELECT disabled_reason FROM project_fix_rules WHERE id = $1")
            .bind(carols)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("sponsor_inactive"));
    assert_eq!(count(&pool, auto).await, 2);
    let _ = app;
}

/// Replaces AUTH's rules by one `fixes` rule sponsored by `who` (whoever
/// saves must manage the project; the administrator restores alice first).
async fn standard_rule_replacing(e: &Env, who: AuthContext) -> Uuid {
    put_rules(e, who, "AUTH", vec![rule(true, &["fixes"], e.w.doing)])
        .await
        .unwrap()
        .rules[0]
        .id
}

// ---------------------------------------------------------------- T-M2-71, 71a, 77

#[tokio::test]
async fn limits_and_states_that_only_hint() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    link_to(&e, "AUTH").await;
    standard_rule(&e, e.alice).await;
    let api = Fake::default();

    // T-M2-71: closed; in triage; already in the target status.
    let (closed, k1) = issue(&e, e.w.auth_p, false).await;
    let (triage, k2) = issue(&e, e.w.auth_p, false).await;
    let (there, k3) = issue(&e, e.w.auth_p, false).await;
    let (private, k4) = issue(&e, e.w.auth_p, true).await;
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(closed)
        .bind(e.w.done)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(there)
        .bind(e.w.doing)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO issue_triage (issue_id, project_id, state, entered_at) VALUES ($1, $2, 'pending', now())",
    )
    .bind(triage)
    .bind(e.w.auth_p)
    .execute(&pool)
    .await
    .unwrap();
    // The private one is closed too, so it only hints.
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(private)
        .bind(e.w.done)
        .execute(&pool)
        .await
        .unwrap();
    let title = format!("fixes {k1}, {k2}, {k3}, {k4}");
    deliver(&e, &api, "pull_request", &merged(1, &title, "", "main")).await;
    work(&e, &api).await;
    assert_eq!(
        states(&e, closed).await,
        st("suggested", Some("already_closed"))
    );
    assert_eq!(states(&e, triage).await, st("suggested", Some("in_triage")));
    assert_eq!(
        states(&e, there).await,
        st("skipped", Some("already_in_target"))
    );
    assert!(
        dev_fixes(&e, e.bob, &k3).await.is_empty(),
        "nothing to do about it"
    );

    // T-M2-77: the development tab's filter, only for issues one can see.
    // (The PR needs an ordinary ref to be listed: its title names them.)
    let er = &e;
    let listed = |who| async move {
        read::project_dev(&er.w.app, who, "AUTH", "fix_skipped", false, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.key)
            .collect::<Vec<_>>()
    };
    let mut all = listed(e.alice).await;
    all.sort();
    let mut want = vec![k1.clone(), k2.clone(), k4.clone()];
    want.sort();
    assert_eq!(all, want);
    // carol manages AUTH but does not see the private issue.
    let hers = listed(e.carol).await;
    assert!(!hers.contains(&k4) && hers.len() == 2, "{hers:?}");
    let err = read::project_dev(&e.w.app, e.bob, "AUTH", "nope", false, None)
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);

    // T-M2-71 (R-F7): 51 issues in one pull request: none moves.
    let mut keys = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..51 {
        let (i, k) = issue(&e, e.w.auth_p, false).await;
        ids.push(i);
        keys.push(k);
    }
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(2, &format!("fixes {}", keys.join(", ")), "", "main"),
    )
    .await;
    work(&e, &api).await;
    for i in &ids {
        assert_eq!(states(&e, *i).await, st("suggested", Some("too_many")));
        assert_eq!(status_of(&e, *i).await, e.w.new);
    }
    // 50 do.
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(3, &format!("fixes {}", keys[..50].join(", ")), "", "main"),
    )
    .await;
    work(&e, &api).await;
    for i in &ids[..50] {
        assert_eq!(status_of(&e, *i).await, e.w.doing);
    }
    assert_eq!(status_of(&e, ids[50]).await, e.w.new);

    // T-M2-71a (R-F3): one push names 250 issues: 200 fix links.
    let mut more = Vec::new();
    for _ in 0..250 {
        more.push(issue(&e, e.w.auth_p, false).await.1);
    }
    let commits: Vec<Value> = more
        .chunks(25)
        .enumerate()
        .map(|(n, c)| commit(0x1000 + n as u64, &format!("fixes {}", c.join(", "))))
        .collect();
    let calls = api.calls();
    assert_eq!(
        deliver(&e, &api, "push", &push_to("main", commits)).await,
        "fix_links_truncated"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_fix_links WHERE sha IS NOT NULL"
        )
        .await,
        200
    );
    assert_eq!(api.calls(), calls);
    assert_eq!(*api.compare_calls.lock().unwrap(), 0);
}

// ---------------------------------------------------------------- links going away

#[tokio::test]
async fn unlinking_and_revoking_take_the_fix_links() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let l = link_to(&e, "AUTH").await;
    standard_rule(&e, e.alice).await;
    let api = Fake::default();
    let (i1, a1) = issue(&e, e.w.auth_p, false).await;
    let (i2, a2) = issue(&e, e.w.auth_p, false).await;
    deliver(
        &e,
        &api,
        "push",
        &push_to("main", vec![commit(7, &format!("fixes {a1}"))]),
    )
    .await;
    assert_eq!(states(&e, i1).await, st("suggested", Some("direct_push")));
    // Revoking the commit takes its hint; pushed again, nothing comes back.
    links::revoke(
        &e.w.app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        "AUTH",
        &links::RevokeInput {
            link_id: None,
            pr_ids: None,
            commits: Some(vec![links::CommitRef {
                repo_id: e.repo,
                sha: sha(7),
            }]),
        },
    )
    .await
    .unwrap();
    assert!(states(&e, i1).await.is_empty());
    deliver(
        &e,
        &api,
        "push",
        &push_to("main", vec![commit(7, &format!("fixes {a1}"))]),
    )
    .await;
    assert!(states(&e, i1).await.is_empty());

    // A pending link whose repository link is gone is dropped, not applied.
    deliver(
        &e,
        &api,
        "pull_request",
        &merged(1, &format!("fixes {a2}"), "", "main"),
    )
    .await;
    assert_eq!(states(&e, i2).await, st("pending", None));
    links::unlink(&e.w.app, e.alice, Uuid::now_v7(), "AUTH", l)
        .await
        .unwrap();
    assert!(states(&e, i2).await.is_empty());
    work(&e, &api).await;
    assert_eq!(status_of(&e, i2).await, e.w.new);
    assert_eq!(api.calls(), 0);
    let _ = commands::SYSTEM_PRINCIPAL;
}
