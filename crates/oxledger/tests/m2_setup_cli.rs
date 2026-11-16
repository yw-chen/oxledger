// SPDX-License-Identifier: GPL-2.0-or-later
//! Site setup from the command line (`oxledger setup init-defaults`,
//! `project add`, `member add`; deploy/README.md): a fresh site becomes
//! usable — people work issues through the workflow, read and write the
//! wiki, and an agent may be delegated on the project. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::commands;
use oxledger::error::ErrorCode;
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::setup::{self, DEFAULT_MODULES, DEVELOPER, MANAGER, REPORTER};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;
use world::{PW, code, count, epoch, exec, id, op, reset, sign_in, tgt};

async fn fresh(pool: &PgPool) {
    reset(pool).await;
    exec(pool, "TRUNCATE wiki_pages, wiki_settings CASCADE").await;
}

fn app(pool: &PgPool) -> Arc<App> {
    Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
        wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
    })
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "setup test", "ops": ops, "preconditions": pre})
}

async fn status(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM issue_statuses WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn create(a: &App, who: &AuthContext, project: Uuid, subject: &str) -> (Uuid, i32) {
    let tracker = id(&a.pool, "SELECT id FROM trackers WHERE name = '缺陷'").await;
    let r = commands::submit(
        a,
        *who,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                        "tracker_id": tracker, "fields": {"subject": subject}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    (r.effects[0].issue_id, r.effects[0].lock_version)
}

async fn transition(
    a: &App,
    who: &AuthContext,
    issue: Uuid,
    lv: i32,
    to: &str,
) -> anyhow::Result<i32> {
    let to = status(&a.pool, to).await;
    let r = commands::submit(
        a,
        *who,
        Uuid::now_v7(),
        &doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(issue), "to_status_id": to})],
            vec![json!({"target": tgt(issue), "lock_version": lv})],
        ),
    )
    .await?;
    Ok(r.effects[0].lock_version)
}

/// init-defaults → project add → member add: the whole path a fresh
/// deployment takes, then real work through the command engine.
#[tokio::test]
async fn a_fresh_site_becomes_usable() {
    let pool = test_pool().await;
    fresh(&pool).await;
    let a = app(&pool);

    // Before the defaults there is nothing to put a project on.
    let e = setup::add_project(&pool, "APP", "App", false, &DEFAULT_MODULES)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("init-defaults"), "{e:#}");

    setup::init_defaults(&pool).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_statuses").await, 6);
    assert_eq!(count(&pool, "SELECT count(*) FROM trackers").await, 3);
    assert_eq!(count(&pool, "SELECT count(*) FROM roles").await, 5);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM roles WHERE issues_visibility = 'own'"
        )
        .await,
        0,
        "no own role: an agent may be delegated on a project with the default roles"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM trackers WHERE default_status_id IS NULL"
        )
        .await,
        0
    );
    // A second run changes nothing.
    let before = count(&pool, "SELECT count(*) FROM role_permissions").await;
    let e = setup::init_defaults(&pool).await.unwrap_err();
    assert!(format!("{e:#}").contains("already"), "{e:#}");
    assert_eq!(count(&pool, "SELECT count(*) FROM roles").await, 5);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM role_permissions").await,
        before
    );

    let app_p = setup::add_project(&pool, "APP", "應用程式", false, &DEFAULT_MODULES)
        .await
        .unwrap();
    // The triggers made the project's setting rows.
    for t in [
        "project_schedule_settings",
        "project_tab_settings",
        "project_triage_settings",
        "project_time_settings",
        "wiki_settings",
    ] {
        assert_eq!(
            count(
                &pool,
                &format!("SELECT count(*) FROM {t} WHERE project_id = '{app_p}'")
            )
            .await,
            1,
            "{t}"
        );
    }
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM project_trackers WHERE project_id = '{app_p}'")
        )
        .await,
        3
    );

    for (login, role) in [("mia", MANAGER), ("dan", DEVELOPER), ("rae", REPORTER)] {
        auth::create_user(&pool, login, login, PW, false)
            .await
            .unwrap();
        let e0 = epoch(&pool, app_p).await;
        setup::add_member(&pool, "APP", login, &[role])
            .await
            .unwrap();
        assert!(
            epoch(&pool, app_p).await > e0,
            "a new member moves the project's security epoch"
        );
    }
    let policy = SessionPolicy::default();
    let mia = sign_in(&pool, &policy, "mia").await;
    let dan = sign_in(&pool, &policy, "dan").await;
    let rae = sign_in(&pool, &policy, "rae").await;

    // A developer works an issue up to 已解決 but does not close it.
    let (i, lv) = create(&a, &dan, app_p, "登入頁跑版").await;
    let lv = transition(&a, &dan, i, lv, "進行中").await.unwrap();
    let lv = transition(&a, &dan, i, lv, "已解決").await.unwrap();
    let e = transition(&a, &dan, i, lv, "已關閉").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidTransition);
    // A reporter does not change statuses (no edit_issues).
    let e = transition(&a, &rae, i, lv, "已關閉").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // The manager closes, and may reopen anything.
    let lv = transition(&a, &mia, i, lv, "已關閉").await.unwrap();
    transition(&a, &mia, i, lv, "新建").await.unwrap();
    // A reporter files issues too.
    create(&a, &rae, app_p, "回報者開的單").await;

    // The wiki: the developer writes a page, the reporter reads it.
    let r = commands::submit(
        &a,
        dan,
        Uuid::now_v7(),
        &json!({"command_schema_version": 1, "title": "wiki",
                "ops": [{"op_id": op(), "kind": "wiki_put",
                         "target": {"project": "APP", "page": "部署流程"}, "body": "先備份"}],
                "preconditions": []}),
    )
    .await
    .unwrap();
    assert_eq!(r.wiki.len(), 1);
    let mut conn = pool.acquire().await.unwrap();
    let readable = oxledger::wiki::readable_wiki_ids(&mut conn, rae)
        .await
        .unwrap();
    drop(conn);
    assert!(readable.contains(&app_p));

    // An agent may be delegated on the project (contract 02 §3.8).
    agents::create_agent_client(&pool, "Claude Code", "policy-2026-01")
        .await
        .unwrap();
    agents::set_agent_client_enabled(&pool, "Claude Code", true)
        .await
        .unwrap();
    let client = id(
        &pool,
        "SELECT id FROM agent_clients WHERE name = 'Claude Code'",
    )
    .await;
    agents::create_delegation(
        &pool,
        &policy,
        &dan,
        DelegationInput {
            agent_client_id: client,
            project_id: app_p,
            proposal_ops: ["create_issue", "add_note"].map(String::from).to_vec(),
            writable_fields: ["subject", "description"].map(String::from).to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn project_and_member_input_is_checked() {
    let pool = test_pool().await;
    fresh(&pool).await;
    setup::init_defaults(&pool).await.unwrap();
    for bad in ["A", "app", "1APP", "TOOLONGKEY1", "AP-P", ""] {
        assert!(
            setup::add_project(&pool, bad, "x", false, &DEFAULT_MODULES)
                .await
                .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        setup::add_project(&pool, "APP", "  ", false, &DEFAULT_MODULES)
            .await
            .is_err()
    );
    assert!(
        setup::add_project(&pool, "APP", "x", false, &["boards"])
            .await
            .is_err()
    );
    assert!(
        setup::add_project(&pool, "APP", "x", false, &[])
            .await
            .is_err(),
        "a project without modules"
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM projects").await, 0);

    // A public project with the wiki only (a repeated module is one).
    let p = setup::add_project(&pool, "DOCS", "文件", true, &["wiki", "wiki"])
        .await
        .unwrap();
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM enabled_modules WHERE project_id = '{p}'")
        )
        .await,
        1
    );
    let e = setup::add_project(&pool, "DOCS", "again", false, &DEFAULT_MODULES)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("exists"), "{e:#}");
    let lines = setup::list_projects(&pool).await.unwrap();
    assert_eq!(lines, vec!["DOCS\t文件\tpublic\tactive\twiki".to_string()]);

    auth::create_user(&pool, "dan", "dan", PW, false)
        .await
        .unwrap();
    assert!(
        setup::add_member(&pool, "NOPE", "dan", &[DEVELOPER])
            .await
            .is_err()
    );
    assert!(
        setup::add_member(&pool, "DOCS", "nobody", &[DEVELOPER])
            .await
            .is_err()
    );
    assert!(
        setup::add_member(&pool, "DOCS", "dan", &["老闆"])
            .await
            .is_err()
    );
    assert!(
        setup::add_member(&pool, "DOCS", "dan", &["非成員"])
            .await
            .is_err(),
        "built-in roles are not member roles"
    );
    assert!(setup::add_member(&pool, "DOCS", "dan", &[]).await.is_err());
    assert_eq!(count(&pool, "SELECT count(*) FROM members").await, 0);
    // Adding again adds the role, not a second membership.
    setup::add_member(&pool, "DOCS", "dan", &[REPORTER])
        .await
        .unwrap();
    setup::add_member(&pool, "DOCS", "dan", &[REPORTER, DEVELOPER])
        .await
        .unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM members").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM member_roles").await, 2);

    let roles = setup::list_roles(&pool).await.unwrap();
    assert_eq!(roles[0], MANAGER);
    assert!(roles.iter().any(|r| r == "匿名\t(built-in: anonymous)"));
}

/// Two init-defaults side by side: one loads, the other refuses; never a
/// mix or a unique violation.
#[tokio::test]
async fn init_defaults_runs_once_under_concurrency() {
    let pool = test_pool().await;
    fresh(&pool).await;
    let (a, b) = tokio::join!(setup::init_defaults(&pool), setup::init_defaults(&pool));
    let errs: Vec<String> = [a, b]
        .into_iter()
        .filter_map(|r| r.err().map(|e| format!("{e:#}")))
        .collect();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("already"), "{errs:?}");
    assert_eq!(count(&pool, "SELECT count(*) FROM roles").await, 5);
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_statuses").await, 6);
}

/// Two `member add` of the same person side by side: one membership with
/// both roles, never a missing row.
#[tokio::test]
async fn member_add_is_safe_under_concurrency() {
    let pool = test_pool().await;
    fresh(&pool).await;
    setup::init_defaults(&pool).await.unwrap();
    setup::add_project(&pool, "APP", "App", false, &DEFAULT_MODULES)
        .await
        .unwrap();
    auth::create_user(&pool, "dan", "dan", PW, false)
        .await
        .unwrap();
    for _ in 0..5 {
        exec(&pool, "TRUNCATE member_roles, members").await;
        let (a, b) = tokio::join!(
            setup::add_member(&pool, "APP", "dan", &[DEVELOPER]),
            setup::add_member(&pool, "APP", "dan", &[REPORTER])
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(count(&pool, "SELECT count(*) FROM members").await, 1);
        assert_eq!(count(&pool, "SELECT count(*) FROM member_roles").await, 2);
    }
}
