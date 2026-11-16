// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S4: the command engine and `human_direct` (§6, §7, §16.2,
//! §16.5, §17.3 R1/R4/R9). T-P2…T-P12, T-W1…T-W8, T-I1 (commands), T-R4,
//! T-R8, T-R9. Needs PostgreSQL 18; see `common::test_pool`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use ox_core::filter::FieldType;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::commands::{self, CommandResult};
use oxledger::custom_fields::{self, NewField};
use oxledger::error::{ErrorCode, classify};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://ledger.test";
const PW: &str = "correct horse battery";

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, issue_events, issue_notes, issue_duplicates,
                  issue_field_origins, execution_field_changes, execution_relation_changes,
                  command_executions, execution_refs, operation_requests,
                  workflow_transitions, workflow_field_rules,
                  project_trackers, member_roles, members, role_permission_trackers, role_permissions,
                  roles, enabled_modules, versions, projects, api_keys, group_members, aliases,
                  principals, trackers, issue_statuses, agent_clients, auth_events, login_challenges CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE org_settings SET issue_group_assignment = false")
        .execute(pool)
        .await
        .unwrap();
    custom_fields::reconcile_indexes(pool).await.unwrap();
}

async fn id(pool: &PgPool, sql: &str) -> Uuid {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

fn code(e: &anyhow::Error) -> ErrorCode {
    classify(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .code
}

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

async fn grant(pool: &PgPool, role: Uuid, perms: &[&str]) {
    for p in perms {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role)
            .bind(p)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn member(pool: &PgPool, project: Uuid, principal: Uuid, role: Uuid) {
    let m: Uuid = sqlx::query_scalar(
        "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
    )
    .bind(project)
    .bind(principal)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
}

async fn transition(pool: &PgPool, tracker: Uuid, role: Uuid, from: Uuid, to: Uuid, cond: &str) {
    sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,$5)")
        .bind(tracker)
        .bind(role)
        .bind(from)
        .bind(to)
        .bind(cond)
        .execute(pool)
        .await
        .unwrap();
}

async fn rule(pool: &PgPool, tracker: Uuid, role: Uuid, status: Uuid, key: &str, r: &str) {
    sqlx::query("INSERT INTO workflow_field_rules VALUES ($1,$2,$3,$4,$5)")
        .bind(tracker)
        .bind(role)
        .bind(status)
        .bind(key)
        .bind(r)
        .execute(pool)
        .await
        .unwrap();
}

async fn lock_version(pool: &PgPool, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn as_(u: Uuid) -> AuthContext {
    AuthContext::test_user(u)
}

fn op() -> Uuid {
    Uuid::now_v7()
}

/// A document from ops and preconditions.
fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "ops": ops, "preconditions": pre})
}

fn pre(issue: Uuid, lv: i32) -> Value {
    json!({"target": {"type": "issue", "id": issue}, "lock_version": lv})
}

fn tgt(issue: Uuid) -> Value {
    json!({"type": "issue", "id": issue})
}

async fn run(app: &App, who: Uuid, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(app, as_(who), Uuid::now_v7(), d).await
}

struct Resp {
    status: StatusCode,
    body: Value,
}

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
}

async fn call(r: &Router, path: &str, body: Value, who: &Browser, key: Option<Uuid>) -> Resp {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", who.session))
        .header("x-csrf-token", &who.csrf)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = key {
        b = b.header("Idempotency-Key", k.to_string());
    }
    let resp = r
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn login(r: &Router, login: &str) -> Browser {
    let lc = r
        .clone()
        .oneshot(
            Request::get("/api/v1/auth/login-context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = lc
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{LOGIN_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={cookie}"))
                .header("x-csrf-token", token)
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

#[tokio::test]
async fn s4_commands() {
    let pool = test_pool().await;
    reset(&pool).await;

    // ---------------------------------------------- world
    let new = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let doing = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Doing','doing',2) RETURNING id").await;
    let done = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Done','done',3) RETURNING id").await;
    let bug = id(&pool, "INSERT INTO trackers (name, position, default_status_id) SELECT 'Bug', 1, id FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let feature = id(&pool, "INSERT INTO trackers (name, position, default_status_id, private_by_default) SELECT 'Feature', 2, id, true FROM issue_statuses WHERE name = 'New' RETURNING id").await;

    let mut users = std::collections::BTreeMap::new();
    for login in ["alice", "bob", "carol", "dan"] {
        users.insert(
            login,
            auth::create_user(&pool, login, login, PW, false)
                .await
                .unwrap(),
        );
    }
    let (alice, bob, carol, dan) = (users["alice"], users["bob"], users["carol"], users["dan"]);

    let web = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('WEB','Web') RETURNING id",
    )
    .await;
    let other = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OTH','Other') RETURNING id",
    )
    .await;
    let old = id(
        &pool,
        "INSERT INTO projects (key, name, status) VALUES ('OLD','Old','closed') RETURNING id",
    )
    .await;
    let arc = id(&pool, "INSERT INTO projects (key, name, status) VALUES ('ARC','Archived','archived') RETURNING id").await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    for p in [web, other, old, arc] {
        for t in [bug, feature] {
            sqlx::query("INSERT INTO project_trackers VALUES ($1,$2)")
                .bind(p)
                .bind(t)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    let dev = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    grant(
        &pool,
        dev,
        &[
            "view_issues",
            "add_issues",
            "edit_issues",
            "add_issue_notes",
            "set_notes_private",
            "manage_issue_relations",
            "set_issues_private",
        ],
    )
    .await;
    let reporter = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','all') RETURNING id",
    )
    .await;
    grant(
        &pool,
        reporter,
        &[
            "view_issues",
            "add_issues",
            "add_issue_notes",
            "edit_own_issues",
        ],
    )
    .await;
    let noter = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Noter','all') RETURNING id",
    )
    .await;
    grant(&pool, noter, &["view_issues", "add_issue_notes"]).await;
    for p in [web, other, old, arc] {
        member(&pool, p, alice, dev).await;
        member(&pool, p, dan, dev).await;
    }
    member(&pool, web, bob, reporter).await;
    member(&pool, web, carol, noter).await;

    for (r, from, to, c) in [
        (dev, new, doing, "always"),
        (dev, doing, done, "always"),
        (dev, new, done, "assignee"),
        (reporter, new, doing, "author"),
    ] {
        for t in [bug, feature] {
            transition(&pool, t, r, from, to, c).await;
        }
    }
    let browser = custom_fields::create_field(
        &pool,
        NewField {
            name: "Browser",
            ty: FieldType::List,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[bug, feature],
            projects: &[],
            options: &["Chrome", "Safari", "Edge"],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let budget = custom_fields::create_field(
        &pool,
        NewField {
            name: "Budget",
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let size = custom_fields::create_field(
        &pool,
        NewField {
            name: "Size",
            ty: FieldType::Float,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[bug, feature],
            projects: &[],
            options: &[],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let (cf_browser, cf_budget, cf_size) = (
        format!("cf:{browser}"),
        format!("cf:{budget}"),
        format!("cf:{size}"),
    );
    let opt = |label: &'static str| {
        let pool = pool.clone();
        async move {
            custom_fields::option_id(&pool, browser, label)
                .await
                .unwrap()
                .unwrap()
        }
    };
    let (chrome, safari, edge) = (opt("Chrome").await, opt("Safari").await, opt("Edge").await);
    // Doing: description read-only for Dev; Done: Browser required for Dev.
    rule(&pool, bug, dev, doing, "description", "readonly").await;
    rule(&pool, bug, dev, done, &cf_browser, "required").await;

    let mk = |project, author, subject: &'static str| NewIssue {
        project_id: project,
        tracker_id: bug,
        status_id: new,
        subject,
        description: "",
        author_id: author,
        assigned_to_id: None,
        is_private: false,
        parent_id: None,
    };
    let (w1, _) = create_issue(&pool, mk(web, alice, "one")).await.unwrap();
    let (w2, _) = create_issue(&pool, mk(web, alice, "two")).await.unwrap();
    let (w3, _) = create_issue(&pool, mk(web, bob, "bob's")).await.unwrap();
    let (w4, _) = create_issue(&pool, mk(web, alice, "four")).await.unwrap();
    let (o1, _) = create_issue(&pool, mk(other, alice, "other"))
        .await
        .unwrap();
    let (c1, _) = create_issue(&pool, mk(old, alice, "closed project"))
        .await
        .unwrap();
    let (a1, _) = create_issue(&pool, mk(arc, alice, "archived project"))
        .await
        .unwrap();

    let app = App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
        wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
    };
    let outbox = |issue: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM outbox WHERE entity_id = $1")
                .bind(issue)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };

    // ---------------------------------------------- T-W2: three ops on one issue
    let ob = outbox(w1).await;
    let r = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1),
                       "set": {"assigned_to_id": bob, &cf_browser: {"list": chrome}, &cf_size: {"float": "12.50"}}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w1), "to_status_id": doing}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(w1), "body": "先指給 bob"}),
            ],
            vec![json!({"target": {"type": "issue", "key": "web-1"}, "lock_version": 0,
                        "field_values": {"assigned_to_id": null, "status_id": new}})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.status, "succeeded");
    assert_eq!(r.effects.len(), 1);
    assert_eq!(
        (r.effects[0].key.as_str(), r.effects[0].lock_version),
        ("WEB-1", 1)
    );
    assert!(r.items.iter().all(|i| i.status == "applied"));
    assert_eq!(lock_version(&pool, w1).await, 1, "one bump for three ops");
    assert_eq!(outbox(w1).await, ob + 1, "one outbox message");
    let seqs: Vec<(i64, String)> =
        sqlx::query_as("SELECT seq, kind FROM issue_events WHERE issue_id = $1 ORDER BY seq")
            .bind(w1)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        seqs,
        vec![
            (1, "fields_changed".into()),
            (2, "status_changed".into()),
            (3, "note_added".into())
        ]
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT last_event_seq FROM issues WHERE id = '{w1}'")
        )
        .await,
        3
    );
    let size_text: String = sqlx::query_scalar(
        "SELECT v_num::text FROM issue_cf WHERE issue_id = $1 AND field_id = $2",
    )
    .bind(w1)
    .bind(size)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(size_text, "12.5", "decimal string kept exactly");
    // Token: c1.<22 chars base64url of the epoch>.<xid8>
    let t: Vec<&str> = r.search.consistent_after.split('.').collect();
    assert_eq!((t.len(), t[0], t[1].len()), (3, "c1", 22));
    assert!(t[2].parse::<u64>().is_ok());
    let stored: String =
        sqlx::query_scalar("SELECT consistent_after FROM command_executions WHERE id = $1")
            .bind(r.execution_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored, r.search.consistent_after,
        "the full token is stored"
    );
    // Field origins point at this execution.
    let origins = count(
        &pool,
        &format!("SELECT count(*) FROM issue_field_origins WHERE issue_id = '{w1}' AND origin_execution_id = '{}'", r.execution_id),
    )
    .await;
    assert_eq!(origins, 4, "assigned_to_id, two cfs, status_id");

    // ---------------------------------------------- T-W8: event identity (human_direct)
    let bad_rows = count(
        &pool,
        "SELECT count(*) FROM issue_events WHERE origin <> 'human_direct' OR actor_kind <> 'user'
           OR command_execution_id IS NULL OR operation_request_id IS NULL OR changeset_id IS NOT NULL
           OR on_behalf_of_id IS NOT NULL OR delegation_id IS NOT NULL",
    )
    .await;
    assert_eq!(bad_rows, 0);
    let ev: Uuid = id(
        &pool,
        &format!("SELECT id FROM issue_events WHERE issue_id = '{w1}' AND seq = 1"),
    )
    .await;
    let ce: Uuid = id(
        &pool,
        &format!("SELECT command_execution_id FROM issue_events WHERE id = '{ev}'"),
    )
    .await;
    let orq: Uuid = id(
        &pool,
        &format!("SELECT operation_request_id FROM issue_events WHERE id = '{ev}'"),
    )
    .await;
    // agent_direct without delegation, seed with an operation request, and
    // external_agent without changeset are all rejected by the CHECK.
    for (origin, kind, extra_cols, extra_vals) in [
        (
            "agent_direct",
            "agent",
            ", command_execution_id, operation_request_id",
            format!(", '{ce}', '{orq}'"),
        ),
        (
            "seed",
            "system",
            ", system_operation, operation_request_id",
            format!(", 'x', '{orq}'"),
        ),
        (
            "external_agent",
            "agent",
            ", operation_request_id",
            format!(", '{orq}'"),
        ),
        (
            "human_direct",
            "user",
            ", operation_request_id",
            format!(", '{orq}'"),
        ),
    ] {
        let sql = format!(
            "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind, payload{extra_cols})
             VALUES ('{w1}', 99, 'fields_changed', '{origin}', '{alice}', '{kind}', '{{}}'{extra_vals})"
        );
        let e = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("check"), "{origin}: {e}");
    }

    // ---------------------------------------------- T-W3: only a note, only a cf
    let ob = outbox(w2).await;
    run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "hi"})],
            vec![pre(w2, 0)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        (lock_version(&pool, w2).await, outbox(w2).await),
        (1, ob + 1)
    );
    run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {&cf_size: {"float": "3"}}})],
            vec![pre(w2, 1)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        (lock_version(&pool, w2).await, outbox(w2).await),
        (2, ob + 2)
    );

    // ---------------------------------------------- T-W4: all no_effect
    let events_before = count(&pool, "SELECT count(*) FROM issue_events").await;
    let ob = outbox(w2).await;
    let r = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2),
                       "set": {"subject": "two", &cf_size: {"float": "3.000"}, "assigned_to_id": null}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w2), "to_status_id": new}),
            ],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap();
    assert!(
        r.items.iter().all(|i| i.status == "no_effect"),
        "{:?}",
        r.items
    );
    assert!(!r.effects[0].changed);
    assert_eq!(r.effects[0].lock_version, 2);
    assert_eq!(lock_version(&pool, w2).await, 2);
    assert_eq!(outbox(w2).await, ob);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_events").await,
        events_before
    );
    assert!(r.search.consistent_after.starts_with("c1."));
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM command_executions WHERE id = '{}'",
                r.execution_id
            )
        )
        .await,
        1,
        "the execution is recorded"
    );

    // ---------------------------------------------- T-W1: the third op fails
    let snapshot = |pool: PgPool| async move {
        (
            count(&pool, "SELECT count(*) FROM issue_events").await,
            count(&pool, "SELECT count(*) FROM issue_notes").await,
            count(&pool, "SELECT count(*) FROM outbox").await,
            count(&pool, "SELECT count(*) FROM issue_cf").await,
            count(&pool, "SELECT count(*) FROM operation_requests").await,
            count(&pool, "SELECT count(*) FROM command_executions").await,
        )
    };
    let before = snapshot(pool.clone()).await;
    let e = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {&cf_size: {"float": "9"}}}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "lost"}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w2), "to_status_id": done}),
            ],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidTransition);
    assert_eq!(details(&e)["op_index"], json!(2));
    assert_eq!(
        details(&e)["allowed"].as_array().unwrap().len(),
        2,
        "New and Doing"
    );
    assert_eq!(
        snapshot(pool.clone()).await,
        before,
        "nothing of the first two ops remains"
    );
    assert_eq!(lock_version(&pool, w2).await, 2);

    // ---------------------------------------------- T-P2: unreadable field, no workflow rules
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {&cf_budget: {"int": "5"}}})],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let e2 = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"cf:999999": {"int": "5"}}})],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        classify(&e).unwrap().message.replace(&cf_budget, "X"),
        classify(&e2).unwrap().message.replace("cf:999999", "X"),
        "unreadable and missing look the same"
    );
    // A precondition on an unreadable field is the same error.
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "x"})],
            vec![json!({"target": tgt(w2), "lock_version": 2, "field_values": {&cf_budget: null}})],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);

    // ---------------------------------------------- T-P3: baseline assignee
    // w4 is unassigned: assigning alice first does not open the assignee-only New→Done.
    let e = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4),
                       "set": {"assigned_to_id": alice, &cf_browser: {"list": chrome}}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w4), "to_status_id": done}),
            ],
            vec![pre(w4, 0)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidTransition);
    run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {"assigned_to_id": alice}})],
            vec![pre(w4, 0)],
        ),
    )
    .await
    .unwrap();
    // Now alice is the assignee at the start: New→Done is allowed (Browser required at Done).
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(w4), "to_status_id": done})],
            vec![pre(w4, 1)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    assert_eq!(details(&e)["fields"], json!([cf_browser]));
    run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(w4), "to_status_id": done}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {&cf_browser: {"list": safari}}}),
            ],
            vec![pre(w4, 1)],
        ),
    )
    .await
    .unwrap();

    // ---------------------------------------------- T-P4: rules of the new status after a transition
    let e = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(w2), "to_status_id": doing}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"description": "late"}}),
            ],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReadonlyField);
    assert_eq!(details(&e)["field"], json!("description"));
    // Before the transition (status New) the same write is fine.
    run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"description": "early"}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w2), "to_status_id": doing}),
            ],
            vec![pre(w2, 2)],
        ),
    )
    .await
    .unwrap();

    // ---------------------------------------------- T-P5: notes only → no transition
    let e = run(
        &app,
        carol,
        &doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(w3), "to_status_id": doing})],
            vec![pre(w3, 0)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    run(
        &app,
        carol,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w3), "body": "seen"})],
            vec![pre(w3, 0)],
        ),
    )
    .await
    .unwrap();
    let e = run(
        &app,
        carol,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w3), "body": "psst", "is_private": true})],
            vec![pre(w3, 1)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::ForbiddenOp,
        "private notes need set_notes_private"
    );

    // edit_own: bob edits his own issue, not alice's; the author transition works for him.
    run(
        &app,
        bob,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w3), "set": {"description": "mine"}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(w3), "to_status_id": doing}),
            ],
            vec![pre(w3, 1)],
        ),
    )
    .await
    .unwrap();
    let e = run(
        &app,
        bob,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {"description": "not mine"}})],
            vec![pre(w4, lock_version(&pool, w4).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // ---------------------------------------------- T-P6: inactive options
    exec(
        &pool,
        &format!("UPDATE field_options SET active = false WHERE id IN ('{safari}', '{edge}')"),
    )
    .await;
    let lv4 = lock_version(&pool, w4).await;
    let r = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {&cf_browser: {"list": safari}}})],
            vec![pre(w4, lv4)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.items[0].status, "no_effect", "keeping an inactive option");
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {&cf_browser: {"list": edge}}})],
            vec![pre(w4, lv4)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["allowed"], json!([chrome]));
    // A wrong type wrapper is invalid_value; a bad number is invalid_request.
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {&cf_size: {"int": "1"}}})],
            vec![pre(w4, lv4)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w4), "set": {&cf_size: {"float": 1.5}}})],
            vec![pre(w4, lv4)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);

    // ---------------------------------------------- T-P7: closed and archived projects
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(c1), "body": "x"})],
            vec![pre(c1, 0)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["reason"].clone()),
        (ErrorCode::ForbiddenOp, json!("project_not_writable"))
    );
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(a1), "body": "x"})],
            vec![pre(a1, 0)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = run(
        &app,
        alice,
        &doc(vec![json!({"op_id": op(), "kind": "create_issue", "project_id": old, "tracker_id": bug, "fields": {"subject": "x"}})], vec![]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // ---------------------------------------------- T-P8, T-W5: duplicates
    let dup = |s: Uuid, t: Uuid, ls: i32, lt: i32| {
        doc(
            vec![
                json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(s), "target": tgt(t)}),
            ],
            if s == t {
                vec![pre(s, ls)]
            } else {
                vec![pre(s, ls), pre(t, lt)]
            },
        )
    };
    let (l1, l2) = (lock_version(&pool, w1).await, lock_version(&pool, w2).await);
    let e = run(&app, alice, &dup(w1, w1, l1, l1)).await.unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["reason"].clone()),
        (ErrorCode::RelationNotAllowed, json!("self"))
    );
    let e = run(&app, alice, &dup(w1, o1, l1, 0)).await.unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["reason"].clone()),
        (ErrorCode::RelationNotAllowed, json!("cross_project"))
    );
    let r = run(&app, alice, &dup(w1, w2, l1, l2)).await.unwrap();
    assert_eq!(
        r.effects
            .iter()
            .map(|e| e.lock_version)
            .collect::<Vec<_>>()
            .len(),
        2
    );
    assert_eq!(
        (lock_version(&pool, w1).await, lock_version(&pool, w2).await),
        (l1 + 1, l2 + 1)
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM issue_events WHERE kind = 'duplicate_added' AND issue_id IN ('{w1}','{w2}')")).await,
        2
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM execution_relation_changes").await,
        1
    );
    // Stale versions: conflict, never a silent no_effect.
    let e = run(&app, alice, &dup(w1, w2, l1, l2)).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    let r = run(&app, alice, &dup(w1, w2, l1 + 1, l2 + 1))
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "no_effect");
    assert_eq!(lock_version(&pool, w1).await, l1 + 1);
    run(&app, alice, &dup(w2, w1, l2 + 1, l1 + 1))
        .await
        .unwrap();
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_duplicates").await,
        2,
        "B→A allowed (no cycle check)"
    );
    // duplicate_of mixed with another op is a structure error.
    let e = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(w1), "target": tgt(w4)}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(w1), "body": "x"}),
            ],
            vec![pre(w1, l1 + 2), pre(w4, lock_version(&pool, w4).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);

    // ---------------------------------------------- T-P11, T-P12: structure
    let lw1 = lock_version(&pool, w1).await;
    let note = |t: Uuid| json!({"op_id": op(), "kind": "add_note", "target": tgt(t), "body": "x"});
    let structure = [
        // two transitions on one issue (one by key)
        doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(w1), "to_status_id": done}),
                json!({"op_id": op(), "kind": "transition", "target": {"type": "issue", "key": "WEB-1"}, "to_status_id": new}),
            ],
            vec![pre(w1, lw1)],
        ),
        // missing precondition
        doc(vec![note(w1)], vec![]),
        // duplicated precondition (key and id)
        doc(
            vec![note(w1)],
            vec![
                pre(w1, lw1),
                json!({"target": {"type": "issue", "key": "WEB-1"}, "lock_version": lw1}),
            ],
        ),
        // a precondition for an issue no op targets
        doc(vec![note(w1)], vec![pre(w1, lw1), pre(w2, 0)]),
        // a precondition without lock_version
        doc(vec![note(w1)], vec![json!({"target": tgt(w1)})]),
        // duplicate with one end missing
        doc(
            vec![
                json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(w1), "target": tgt(w4)}),
            ],
            vec![pre(w1, lw1)],
        ),
        // a temp target with a precondition
        doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": web, "tracker_id": bug, "fields": {"subject": "s"}}),
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "n"}, "body": "x"}),
            ],
            vec![json!({"target": {"type": "temp", "ref": "n"}, "lock_version": 0})],
        ),
        // the same field twice for one issue
        doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1), "set": {"subject": "a"}}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1), "set": {"subject": "b"}}),
            ],
            vec![pre(w1, lw1)],
        ),
        // repeated op_id
        {
            let o = op();
            doc(
                vec![
                    json!({"op_id": o, "kind": "add_note", "target": tgt(w1), "body": "x"}),
                    json!({"op_id": o, "kind": "add_note", "target": tgt(w1), "body": "y"}),
                ],
                vec![pre(w1, lw1)],
            )
        },
        // author_id cannot be given; status only through transition
        doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1), "set": {"author_id": alice}}),
            ],
            vec![pre(w1, lw1)],
        ),
        doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1), "set": {"status_id": done}}),
            ],
            vec![pre(w1, lw1)],
        ),
        // unknown top-level and op fields
        json!({"command_schema_version": 1, "ops": [note(w1)], "preconditions": [pre(w1, lw1)], "approved": true}),
        json!({"command_schema_version": 1, "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(w1), "body": "x", "origin": "human"}], "preconditions": [pre(w1, lw1)]}),
        json!({"command_schema_version": 2, "ops": [note(w1)], "preconditions": [pre(w1, lw1)]}),
        // 11 ops
        doc((0..11).map(|_| note(w1)).collect(), vec![pre(w1, lw1)]),
        // T-W6 generation: duplicate bind, forward ref, unknown ref
        doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": web, "tracker_id": bug, "fields": {"subject": "s"}}),
                json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": web, "tracker_id": bug, "fields": {"subject": "t"}}),
            ],
            vec![],
        ),
        doc(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "n"}, "body": "x"}),
                json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": web, "tracker_id": bug, "fields": {"subject": "s"}}),
            ],
            vec![],
        ),
        doc(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "zz"}, "body": "x"}),
            ],
            vec![],
        ),
        doc(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "issue", "id": w1, "key": "WEB-1"}, "body": "x"}),
            ],
            vec![pre(w1, lw1)],
        ),
    ];
    let before = snapshot(pool.clone()).await;
    for (n, d) in structure.iter().enumerate() {
        let e = run(&app, alice, d).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "case {n}: {e:#}");
    }
    assert_eq!(snapshot(pool.clone()).await, before);
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "delete_issue", "target": tgt(w1)})],
            vec![pre(w1, lw1)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UnsupportedOperation);
    let mut d = doc(vec![note(w1)], vec![pre(w1, lw1)]);
    d["exec_mode"] = json!("per_item");
    assert_eq!(
        code(&run(&app, alice, &d).await.unwrap_err()),
        ErrorCode::UnsupportedExecutionMode
    );
    let e = run(
        &app,
        alice,
        &doc(
            vec![note(w1)],
            vec![json!({"target": {"type": "issue", "key": "WEB-999"}, "lock_version": 0})],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);

    // ---------------------------------------------- T-W6: create + note through a temp ref
    let r = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "bind": "new1", "project_id": web, "tracker_id": bug,
                       "fields": {"subject": "Safari 登入後回到登入頁", "description": "…", &cf_browser: {"list": chrome}}}),
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "new1"}, "body": "需要 Safari 版本"}),
                json!({"op_id": op(), "kind": "transition", "target": {"type": "temp", "ref": "new1"}, "to_status_id": doing}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    let created = &r.effects[0];
    assert!(created.created && created.changed);
    assert_eq!((created.key.as_str(), created.lock_version), ("WEB-5", 0));
    assert_eq!(
        lock_version(&pool, created.issue_id).await,
        0,
        "later ops do not bump a new issue"
    );
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM issue_events WHERE issue_id = $1 ORDER BY seq")
            .bind(created.issue_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kinds, ["created", "note_added", "status_changed"]);
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM issue_events WHERE issue_id = $1 AND seq = 1")
            .bind(created.issue_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        payload["fields"]["status_id"],
        json!(new),
        "created holds the initial values"
    );
    assert_eq!(payload["fields"][&cf_browser], json!({"list": chrome}));
    let (author, prio): (Uuid, Uuid) =
        sqlx::query_as("SELECT author_id, priority_id FROM issues WHERE id = $1")
            .bind(created.issue_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let default_prio = id(&pool, "SELECT id FROM issue_priorities WHERE is_default").await;
    assert_eq!((author, prio), (alice, default_prio));

    // ---------------------------------------------- T-W7: private by default
    let r = run(
        &app,
        bob,
        &doc(vec![json!({"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": feature, "fields": {"subject": "f"}})], vec![]),
    )
    .await
    .unwrap();
    let private: bool = sqlx::query_scalar("SELECT is_private FROM issues WHERE id = $1")
        .bind(r.effects[0].issue_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(private, "the tracker default applies");
    let e = run(
        &app,
        bob,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": bug,
                         "fields": {"subject": "f", "is_private": true}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::ForbiddenOp,
        "an explicit private needs the permission"
    );
    let e = run(
        &app,
        alice,
        &doc(vec![json!({"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": bug, "fields": {"description": "no subject"}})], vec![]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);

    // ---------------------------------------------- assignees, priorities, versions, dates (§6.5, R1, R4, R9)
    let lw = lock_version(&pool, w2).await;
    let upd = |set: Value| {
        doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": set})],
            vec![pre(w2, lw)],
        )
    };
    let e = run(&app, alice, &upd(json!({"assigned_to_id": carol}))).await;
    assert!(
        e.is_ok(),
        "carol is a member with an assignable role: {e:?}"
    );
    let lw = lock_version(&pool, w2).await;
    let upd = |set: Value| {
        doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": set})],
            vec![pre(w2, lw)],
        )
    };
    let outsider = auth::create_user(&pool, "olive", "olive", PW, false)
        .await
        .unwrap();
    assert_eq!(
        code(
            &run(&app, alice, &upd(json!({"assigned_to_id": outsider})))
                .await
                .unwrap_err()
        ),
        ErrorCode::InvalidValue
    );
    let low = id(&pool, "SELECT id FROM issue_priorities WHERE name = 'Low'").await;
    exec(
        &pool,
        "UPDATE issue_priorities SET active = false WHERE name = 'Immediate'",
    )
    .await;
    let immediate = id(
        &pool,
        "SELECT id FROM issue_priorities WHERE name = 'Immediate'",
    )
    .await;
    assert_eq!(
        code(
            &run(&app, alice, &upd(json!({"priority_id": immediate})))
                .await
                .unwrap_err()
        ),
        ErrorCode::InvalidValue
    );
    assert_eq!(
        code(
            &run(&app, alice, &upd(json!({"priority_id": null})))
                .await
                .unwrap_err()
        ),
        ErrorCode::InvalidValue
    );
    let mkv = |project: Uuid, name: &str, status: &str| {
        format!(
            "INSERT INTO versions (project_id, name, name_key, status, created_by) VALUES ('{project}', '{name}', lower('{name}'), '{status}', '{alice}') RETURNING id"
        )
    };
    let v_open = id(&pool, &mkv(web, "v1", "open")).await;
    let v_locked = id(&pool, &mkv(web, "v2", "locked")).await;
    let v_other = id(&pool, &mkv(other, "o1", "open")).await;
    let v_gone = id(&pool, &mkv(web, "v0", "open")).await;
    exec(
        &pool,
        &format!("UPDATE versions SET deleted_at = now() WHERE id = '{v_gone}'"),
    )
    .await;
    let e = run(&app, alice, &upd(json!({"fixed_version_id": v_locked})))
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["status"].clone()),
        (ErrorCode::VersionNotOpen, json!("locked"))
    );
    let mut shapes: Vec<String> = Vec::new();
    for v in [v_other, v_gone, Uuid::now_v7()] {
        let e = run(&app, alice, &upd(json!({"fixed_version_id": v})))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue);
        shapes.push(classify(&e).unwrap().message.replace(&v.to_string(), "V"));
    }
    assert!(
        shapes.windows(2).all(|w| w[0] == w[1]),
        "R1: one shape {shapes:?}"
    );
    let e = run(
        &app,
        alice,
        &upd(json!({"start_date": "2026-12-01", "due_date": "2026-11-01"})),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["kind"].clone()),
        (ErrorCode::InvalidValue, json!("date_order"))
    );
    let e = run(&app, alice, &upd(json!({"due_date": "2200-01-01"})))
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["kind"].clone()),
        (ErrorCode::InvalidValue, json!("date_out_of_range"))
    );
    assert_eq!(
        code(
            &run(&app, alice, &upd(json!({"due_date": "2026-2-1"})))
                .await
                .unwrap_err()
        ),
        ErrorCode::InvalidRequest
    );
    run(
        &app,
        alice,
        &upd(json!({"fixed_version_id": v_open, "priority_id": low, "start_date": "2026-11-01", "due_date": "2026-11-30"})),
    )
    .await
    .unwrap();
    // R2: an issue already in a version that gets locked keeps working; setting the same version is no_effect.
    exec(
        &pool,
        &format!("UPDATE versions SET status = 'locked' WHERE id = '{v_open}'"),
    )
    .await;
    let lw = lock_version(&pool, w2).await;
    let r = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"fixed_version_id": v_open}}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "still fine"}),
            ],
            vec![pre(w2, lw)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.items[0].status, "no_effect");
    // R3: clearing is always allowed.
    run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"fixed_version_id": null}})],
            vec![pre(w2, lw + 1)],
        ),
    )
    .await
    .unwrap();

    // ---------------------------------------------- T-R4: stale lock_version
    let e = run(&app, alice, &doc(vec![note(w2)], vec![pre(w2, lw)]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    assert_eq!(details(&e)["lock_version"], json!(lw + 2));
    let e = run(
        &app,
        alice,
        &doc(vec![note(w2)], vec![json!({"target": tgt(w2), "lock_version": lw + 2, "field_values": {"subject": "other"}})]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PreconditionFailed);

    // ---------------------------------------------- T-I1: 100 concurrent sends of one key
    let lw = lock_version(&pool, w2).await;
    let key = Uuid::now_v7();
    let d = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "once"})],
        vec![pre(w2, lw)],
    );
    let app = Arc::new(app);
    let mut handles = Vec::new();
    for _ in 0..100 {
        let (app, d) = (app.clone(), d.clone());
        handles.push(tokio::spawn(async move {
            commands::submit(&app, as_(alice), key, &d).await
        }));
    }
    let mut execs = std::collections::BTreeSet::new();
    for h in handles {
        let r = common::join("a concurrent submit", h).await.unwrap();
        execs.insert(r.execution_id);
    }
    assert_eq!(execs.len(), 1, "one execution");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE body = 'once' AND issue_id = '{w2}'")
        )
        .await,
        1
    );
    assert_eq!(lock_version(&pool, w2).await, lw + 1);
    // Same key, other content; another caller's key space is separate.
    let other_doc = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "twice"})],
        vec![pre(w2, lw + 1)],
    );
    let e = commands::submit(&app, as_(alice), key, &other_doc)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::IdempotencyKeyReused);
    assert!(
        commands::submit(&app, as_(dan), key, &other_doc)
            .await
            .is_ok()
    );
    // A failed request leaves no record: the same key evaluates again after the fix.
    let key2 = Uuid::now_v7();
    let stale = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "retry"})],
        vec![pre(w2, 0)],
    );
    assert_eq!(
        code(
            &commands::submit(&app, as_(alice), key2, &stale)
                .await
                .unwrap_err()
        ),
        ErrorCode::Conflict
    );
    let lw = lock_version(&pool, w2).await;
    let fixed = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "retry"})],
        vec![pre(w2, lw)],
    );
    assert!(
        commands::submit(&app, as_(alice), key2, &fixed)
            .await
            .is_ok()
    );
    // Replay after the issue became invisible: no effect is shown.
    let r1 = commands::submit(&app, as_(alice), key2, &fixed)
        .await
        .unwrap();
    assert_eq!(r1.effects.len(), 1);
    exec(&pool, &format!("UPDATE operation_requests SET result_expires_at = now() - interval '1 day' WHERE operation_id = '{key2}'")).await;
    assert_eq!(
        code(
            &commands::submit(&app, as_(alice), key2, &fixed)
                .await
                .unwrap_err()
        ),
        ErrorCode::OperationResultExpired
    );

    // ---------------------------------------------- T-R8: the same edge twice, concurrently
    let (l3, l4) = (lock_version(&pool, w3).await, lock_version(&pool, w4).await);
    let d1 = dup(w3, w4, l3, l4);
    let d2 = dup(w3, w4, l3, l4);
    let (a, b) = common::wait_for("two racing submits", common::WAIT_TIMEOUT, async {
        tokio::join!(
            commands::submit(&app, as_(alice), Uuid::now_v7(), &d1),
            commands::submit(&app, as_(dan), Uuid::now_v7(), &d2)
        )
    })
    .await;
    let codes: Vec<Option<ErrorCode>> = [&a, &b]
        .iter()
        .map(|r| r.as_ref().err().map(code))
        .collect();
    assert!(
        codes.contains(&None) && codes.contains(&Some(ErrorCode::Conflict)),
        "{codes:?}"
    );
    let r = run(&app, alice, &dup(w3, w4, l3 + 1, l4 + 1))
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "no_effect");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_duplicates WHERE source_issue_id = '{w3}'")
        )
        .await,
        1
    );

    // ---------------------------------------------- REST, sessions, T-R9
    let router = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let br = login(&router, "alice").await;
    let lw = lock_version(&pool, w2).await;
    let body = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "rest"})],
        vec![pre(w2, lw)],
    );
    let r = call(&router, "/commands", body.clone(), &br, None).await;
    assert_eq!(
        (r.status, r.body["error"]["details"]["kind"].clone()),
        (StatusCode::BAD_REQUEST, json!("idempotency_key"))
    );
    let k = Uuid::now_v7();
    let r = call(&router, "/commands", body.clone(), &br, Some(k)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["status"], json!("succeeded"));
    let again = call(&router, "/commands", body.clone(), &br, Some(k)).await;
    assert_eq!(
        again.body["data"]["execution_id"],
        r.body["data"]["execution_id"]
    );
    assert_eq!(
        again.body["data"]["search"], r.body["data"]["search"],
        "the original token"
    );
    let r = call(
        &router,
        "/commands",
        json!({"command_schema_version": 1, "ops": "x"}),
        &br,
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // An agent or legacy key never reaches the service as a person.
    let key_ctx = AuthContext::anonymous();
    assert!(
        commands::submit(&app, key_ctx, Uuid::now_v7(), &body)
            .await
            .is_err()
    );

    // T-R9: the field definition is locked elsewhere until the session expired.
    let lw = lock_version(&pool, w2).await;
    let body = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {&cf_size: {"float": "77"}}}),
        ],
        vec![pre(w2, lw)],
    );
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM field_defs WHERE id = $1 FOR UPDATE")
        .bind(size)
        .execute(&mut *holder)
        .await
        .unwrap();
    exec(
        &pool,
        &format!("UPDATE browser_sessions SET idle_expires_at = now() + interval '2 seconds' WHERE user_id = '{alice}' AND revoked_at IS NULL"),
    )
    .await;
    let req = {
        let (router, br, body) = (router.clone(), br.clone(), body.clone());
        tokio::spawn(
            async move { call(&router, "/commands", body, &br, Some(Uuid::now_v7())).await },
        )
    };
    tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
    holder.rollback().await.unwrap();
    let r = common::join("the HTTP request", req).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
    assert_eq!(lock_version(&pool, w2).await, lw, "zero effect");

    // ---------------------------------------------- verifier follow-ups (S4 review)
    // Temp targets get the op-level check too: add_issues alone cannot
    // update_fields or add a note on the issue it just created.
    let creator = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Creator','all') RETURNING id",
    )
    .await;
    grant(&pool, creator, &["view_issues", "add_issues"]).await;
    let erin = auth::create_user(&pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    member(&pool, web, erin, creator).await;
    let create_then = |second: Value| {
        doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": web, "tracker_id": bug,
                       "fields": {"subject": "by erin"}}),
                second,
            ],
            vec![],
        )
    };
    for second in [
        json!({"op_id": op(), "kind": "update_fields", "target": {"type": "temp", "ref": "n"}, "set": {"description": "x"}}),
        json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "n"}, "body": "x"}),
    ] {
        let e = run(&app, erin, &create_then(second)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
        assert_eq!(details(&e)["op_index"], json!(1));
    }
    // edit_own suffices for the creator (the author of the temp issue).
    run(
        &app,
        bob,
        &create_then(json!({"op_id": op(), "kind": "update_fields", "target": {"type": "temp", "ref": "n"}, "set": {"description": "x"}})),
    )
    .await
    .unwrap();

    // duplicate_of needs manage_issue_relations on the source (edit is not enough).
    let editor = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Editor','all') RETURNING id",
    )
    .await;
    grant(&pool, editor, &["view_issues", "edit_issues"]).await;
    let fay = auth::create_user(&pool, "fay", "fay", PW, false)
        .await
        .unwrap();
    member(&pool, web, fay, editor).await;
    let (l1, l4) = (lock_version(&pool, w1).await, lock_version(&pool, w4).await);
    let e = run(&app, fay, &dup(w1, w4, l1, l4)).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // A note-only command does not trigger the required check (decisions S4):
    // an issue at Done without the required Browser still takes a note.
    let (old_done, _) = create_issue(
        &pool,
        NewIssue {
            status_id: done,
            ..mk(web, alice, "legacy done")
        },
    )
    .await
    .unwrap();
    run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": tgt(old_done), "body": "fine"}),
            ],
            vec![pre(old_done, 0)],
        ),
    )
    .await
    .unwrap();
    // …but any field change on it does.
    let e = run(
        &app,
        alice,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(old_done), "set": {"subject": "touched"}})],
            vec![pre(old_done, 1)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);

    // Each identity CHECK fails on one missing column alone.
    let lone = format!(
        "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind, payload,
                                   command_execution_id, operation_request_id, on_behalf_of_id)
         VALUES ('{w1}', 98, 'fields_changed', 'agent_direct', '{alice}', 'agent', '{{}}', '{ce}', '{orq}', '{alice}')"
    );
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(lone))
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("check"),
        "agent_direct without delegation_id: {e}"
    );
    let lone = format!(
        "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind, payload,
                                   command_execution_id, operation_request_id)
         VALUES ('{w1}', 98, 'fields_changed', 'human_direct', '{alice}', 'agent', '{{}}', '{ce}', '{orq}')"
    );
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(lone))
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("check"),
        "human_direct by an agent: {e}"
    );

    // M27: the receipt waits for an epoch rotation holding search_settings
    // and carries the new epoch.
    let lw = lock_version(&pool, w2).await;
    let mut rot = pool.begin().await.unwrap();
    let new_epoch: Uuid =
        sqlx::query_scalar("UPDATE search_settings SET data_epoch = uuidv7() RETURNING data_epoch")
            .fetch_one(&mut *rot)
            .await
            .unwrap();
    let pending = {
        let (app, d) = (
            app.clone(),
            doc(
                vec![
                    json!({"op_id": op(), "kind": "add_note", "target": tgt(w2), "body": "epoch"}),
                ],
                vec![pre(w2, lw)],
            ),
        );
        tokio::spawn(async move { commands::submit(&app, as_(alice), Uuid::now_v7(), &d).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "the write waits for the rotation");
    rot.commit().await.unwrap();
    let r = common::join("the pending submit", pending).await.unwrap();
    assert_eq!(
        r.search.consistent_after,
        commands::token(
            new_epoch,
            r.search.consistent_after.rsplit('.').next().unwrap()
        )
    );

    // M02: assigning a version waits for a transaction holding it FOR UPDATE
    // (a close in progress) and then sees the close.
    let v_race = id(&pool, &mkv(web, "race", "open")).await;
    let lw = lock_version(&pool, w2).await;
    let mut closing = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM versions WHERE id = $1 FOR UPDATE")
        .bind(v_race)
        .execute(&mut *closing)
        .await
        .unwrap();
    let pending = {
        let app = app.clone();
        let d = doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(w2), "set": {"fixed_version_id": v_race}}),
            ],
            vec![pre(w2, lw)],
        );
        tokio::spawn(async move { commands::submit(&app, as_(alice), Uuid::now_v7(), &d).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "step 7b waits for the version lock");
    sqlx::query("UPDATE versions SET status = 'closed' WHERE id = $1")
        .bind(v_race)
        .execute(&mut *closing)
        .await
        .unwrap();
    closing.commit().await.unwrap();
    let e = common::join("the pending submit", pending)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::VersionNotOpen);

    // M05: a replay after the issue became invisible shows no effect.
    let (w9, _) = create_issue(&pool, mk(web, alice, "soon private"))
        .await
        .unwrap();
    let key9 = Uuid::now_v7();
    let d9 = doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(w9), "body": "seen"})],
        vec![pre(w9, 0)],
    );
    let r = commands::submit(&app, as_(carol), key9, &d9).await.unwrap();
    assert_eq!((r.effects.len(), r.items.len()), (1, 1));
    exec(&pool, &format!("UPDATE issues SET is_private = true WHERE id = '{w9}'; UPDATE roles SET issues_visibility = 'default' WHERE id = '{noter}'")).await;
    let r = commands::submit(&app, as_(carol), key9, &d9).await.unwrap();
    assert_eq!(
        (r.effects.len(), r.items.len()),
        (0, 0),
        "replay re-checks visibility"
    );

    // Numbers are normalized before the fingerprint: "12.50" and "12.5" are one request.
    let lw = lock_version(&pool, w2).await;
    let k = Uuid::now_v7();
    let num = |s: &str| {
        doc(
            vec![
                json!({"op_id": "01a00000-0000-7000-8000-000000000001", "kind": "update_fields", "target": tgt(w2), "set": {&cf_size: {"float": s}}}),
            ],
            vec![pre(w2, lw)],
        )
    };
    let first = commands::submit(&app, as_(alice), k, &num("12.50"))
        .await
        .unwrap();
    let again = commands::submit(&app, as_(alice), k, &num("12.5"))
        .await
        .unwrap();
    assert_eq!(first.execution_id, again.execution_id);

    // A new issue writes exactly one outbox message.
    let r = run(
        &app,
        alice,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "bind": "o", "project_id": web, "tracker_id": bug, "fields": {"subject": "outbox"}}),
                json!({"op_id": op(), "kind": "add_note", "target": {"type": "temp", "ref": "o"}, "body": "x"}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(outbox(r.effects[0].issue_id).await, 1);
}
