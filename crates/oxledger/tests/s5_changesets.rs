// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S5: ChangeSets (§8, §6.7 propose/apply). T-I1…T-I6, C-1…C-6,
//! T-R1…T-R3, T-R5, T-R7, T-R10, T-R12, T-R13, T-R14, T-R15, T-S7, T-S10,
//! T-S11, T-S12, T-A7 (the proposal part). Needs PostgreSQL 18; see
//! `common::test_pool`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents::{self, DelegationInput};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::{
    self, ApplyOutcome, ApproveInput, DecisionInput, ProposeResult, ValidateInput,
    faults::{self, Action, Gate},
};
use oxledger::custom_fields;
use oxledger::error::{ErrorCode, classify};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://ledger.test";
const PW: &str = "correct horse battery";

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE changeset_applications, changeset_executions, changeset_decisions, changeset_version_ops,
                  changeset_version_source_projects, changeset_versions, changesets, changeset_groups,
                  proposal_contents,
                  issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
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

async fn text(pool: &PgPool, sql: &str) -> String {
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

async fn lock_version(pool: &PgPool, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn notes(pool: &PgPool, issue: Uuid) -> i64 {
    count(
        pool,
        &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{issue}'"),
    )
    .await
}

/// Moves a proposal's expiry (immutable for the application) for timing tests.
async fn set_expiry(pool: &PgPool, cs: Uuid, secs: f64) {
    let mut c = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET session_replication_role = replica;
         UPDATE changesets SET expires_at = clock_timestamp() + make_interval(secs => {secs}) WHERE id = '{cs}';
         SET session_replication_role = origin;"
    )))
    .execute(&mut *c)
    .await
    .unwrap();
}

fn op() -> String {
    Uuid::now_v7().to_string()
}

fn tgt(i: Uuid) -> Value {
    json!({"type": "issue", "id": i})
}

fn pre(i: Uuid, lv: i32) -> Value {
    json!({"target": tgt(i), "lock_version": lv})
}

fn doc(ops: Vec<Value>, pres: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal", "ops": ops, "preconditions": pres})
}

fn note_doc(i: Uuid, lv: i32, body: &str) -> Value {
    doc(
        vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body})],
        vec![pre(i, lv)],
    )
}

// ---------------------------------------------------------------- browser

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
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

struct Resp {
    status: StatusCode,
    body: Value,
    headers: HeaderMap,
}

impl Resp {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

async fn call(
    r: &Router,
    b: &Browser,
    method: &str,
    path: &str,
    body: Option<Value>,
    key: Option<Uuid>,
) -> Resp {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
        .header("x-csrf-token", &b.csrf)
        .header(header::ORIGIN, ORIGIN);
    if let Some(k) = key {
        req = req.header("idempotency-key", k.to_string());
    }
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Resp {
        status,
        body,
        headers,
    }
}

// ---------------------------------------------------------------- service shorthands

struct T {
    app: Arc<App>,
    alice: AuthContext,
    agent: AuthContext,
}

impl T {
    async fn propose(&self, d: &Value) -> anyhow::Result<ProposeResult> {
        changesets::propose(&self.app, self.agent, Uuid::now_v7(), d, ORIGIN).await
    }
    async fn approve(&self, p: &ProposeResult, key: Uuid) -> anyhow::Result<ApplyOutcome> {
        self.approve_v(p.changeset_id, p.version, &p.proposal_hash, key)
            .await
    }
    async fn approve_v(
        &self,
        cs: Uuid,
        version: i32,
        hash: &str,
        key: Uuid,
    ) -> anyhow::Result<ApplyOutcome> {
        changesets::approve_and_apply(
            &self.app,
            self.alice,
            key,
            cs,
            &ApproveInput {
                version,
                proposal_hash: hash.to_string(),
                review_mode: None,
            },
        )
        .await
    }
}

fn status(o: &ApplyOutcome) -> &str {
    o.execution
        .as_ref()
        .map_or("pending", |e| e.status.as_str())
}

#[tokio::test]
async fn s5_changesets() {
    let pool = test_pool().await;
    reset(&pool).await;
    faults::clear();

    // ---------------------------------------------- world
    let new = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let doing = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Doing','doing',2) RETURNING id").await;
    let done = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Done','done',3) RETURNING id").await;
    let bug = id(&pool, "INSERT INTO trackers (name, position, default_status_id) SELECT 'Bug', 1, id FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let feature = id(&pool, "INSERT INTO trackers (name, position, default_status_id, private_by_default) SELECT 'Feature', 2, id, true FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let alice_id = auth::create_user(&pool, "alice", "alice", PW, false)
        .await
        .unwrap();
    let bob = auth::create_user(&pool, "bob", "bob", PW, false)
        .await
        .unwrap();
    let web = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('WEB','Web') RETURNING id",
    )
    .await;
    let oth = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OTH','Other') RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    for p in [web, oth] {
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
            "set_issues_private",
            "manage_issue_relations",
        ],
    )
    .await;
    for p in [web, oth] {
        member(&pool, p, alice_id, dev).await;
        member(&pool, p, bob, dev).await;
    }
    for t in [bug, feature] {
        sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
            .bind(t)
            .bind(dev)
            .bind(new)
            .bind(doing)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mk = |subject: &'static str| NewIssue {
        project_id: web,
        tracker_id: bug,
        status_id: new,
        subject,
        description: "",
        author_id: bob,
        assigned_to_id: None,
        is_private: false,
        parent_id: None,
    };
    let mut issues = Vec::new();
    for s in [
        "one", "two", "three", "four", "five", "six", "seven", "eight",
    ] {
        issues.push(create_issue(&pool, mk(s)).await.unwrap().0);
    }
    let (o1, _) = create_issue(
        &pool,
        NewIssue {
            project_id: oth,
            ..mk("other project")
        },
    )
    .await
    .unwrap();

    let client = agents::create_agent_client(&pool, "Claude Code", "policy-2026-01")
        .await
        .unwrap();
    agents::set_agent_client_enabled(&pool, "Claude Code", true)
        .await
        .unwrap();
    let app = Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    });
    let policy = SessionPolicy::default();
    let router = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: policy.clone(),
        },
    );
    let ab = login(&router, "alice").await;
    let alice = auth::session_from_cookie(&pool, &policy, &ab.session)
        .await
        .unwrap()
        .unwrap()
        .ctx;
    let deleg = agents::create_delegation(
        &pool,
        &policy,
        &alice,
        DelegationInput {
            agent_client_id: client,
            project_id: web,
            proposal_ops: [
                "create_issue",
                "update_fields",
                "transition",
                "add_note",
                "duplicate_of",
            ]
            .map(String::from)
            .to_vec(),
            writable_fields: ["subject", "description", "assigned_to_id"]
                .map(String::from)
                .to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
        },
    )
    .await
    .unwrap();
    let cred = agents::issue_credential(&pool, &policy, &alice, deleg.context_id, "t", None)
        .await
        .unwrap();
    let agent = auth::authenticate_bearer(&pool, &cred.token)
        .await
        .unwrap()
        .unwrap();
    let t = T {
        app: app.clone(),
        alice,
        agent,
    };
    let agent_id = agent.principal_id().unwrap();

    // ---------------------------------------------- propose (§8.4)
    let w1 = issues[0];
    let d1 = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(w1), "set": {"subject": "one (triaged)"}}),
            json!({"op_id": op(), "kind": "add_note", "target": tgt(w1), "body": "seen by the agent"}),
        ],
        vec![pre(w1, 0)],
    );
    let key1 = Uuid::now_v7();
    let p1 = changesets::propose(&app, agent, key1, &d1, ORIGIN)
        .await
        .unwrap();
    assert_eq!((p1.version, p1.state.as_str()), (1, "proposed"));
    assert_eq!(p1.validation.as_ref().unwrap().status, "passed");
    assert!(p1.proposal_hash.starts_with("sha256:") && p1.proposal_hash.len() == 71);
    assert_eq!(p1.targets.len(), 1);
    assert_eq!(p1.targets[0].key, "WEB-1");
    assert_eq!(
        p1.review_url,
        format!("{ORIGIN}/changesets/{}?version=1", p1.changeset_id)
    );
    assert_eq!(
        serde_json::to_value(&p1.next_actions).unwrap(),
        json!(["ask_human"])
    );
    let cs1 = p1.changeset_id;
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM changeset_version_ops WHERE changeset_id = '{cs1}'")
        )
        .await,
        2
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM changeset_version_source_projects WHERE changeset_id = '{cs1}'")).await,
        1
    );
    assert_eq!(lock_version(&pool, w1).await, 0, "proposing writes nothing");
    // The ceiling and the stored body.
    assert_eq!(
        text(&pool, &format!("SELECT authority_ceiling->'projects'->0->>'project_id' FROM changeset_versions WHERE changeset_id = '{cs1}'")).await,
        web.to_string()
    );

    // T-I2: the same key and document replays; another document or action is rejected.
    let again = changesets::propose(&app, agent, key1, &d1, ORIGIN)
        .await
        .unwrap();
    assert_eq!(
        (again.changeset_id, again.proposal_hash.clone()),
        (cs1, p1.proposal_hash.clone())
    );
    let e = changesets::propose(&app, agent, key1, &note_doc(w1, 0, "other"), ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::IdempotencyKeyReused);
    let e = changesets::cancel(&app, agent, key1, cs1)
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::IdempotencyKeyReused,
        "another action, same key"
    );

    // T-I1 (propose): 100 concurrent sends of one key → one proposal.
    let k = Uuid::now_v7();
    let d = note_doc(issues[1], 0, "concurrent");
    let mut hs = Vec::new();
    for _ in 0..100 {
        let (app, d) = (app.clone(), d.clone());
        hs.push(tokio::spawn(async move {
            changesets::propose(&app, agent, k, &d, ORIGIN).await
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    for h in hs {
        match h.await.unwrap() {
            Ok(r) => {
                ids.insert(r.changeset_id);
            }
            Err(e) => assert_eq!(code(&e), ErrorCode::TemporarilyUnavailable, "{e:#}"),
        }
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM changeset_versions v JOIN changesets c ON c.id = v.changeset_id
                               WHERE c.id = '{}'", ids.iter().next().unwrap())).await,
        1
    );
    let proposals_before = count(&pool, "SELECT count(*) FROM changesets").await;

    // Step 9 problems are stored as blocked / conflict; steps 1–8 are not stored.
    let blocked = t
        .propose(&doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(issues[2]), "to_status_id": done})],
            vec![pre(issues[2], 0)],
        ))
        .await
        .unwrap();
    assert_eq!(blocked.validation.as_ref().unwrap().status, "blocked");
    assert_eq!(
        blocked.validation.as_ref().unwrap().blocking[0].code,
        "invalid_transition"
    );
    assert!(
        blocked.validation.as_ref().unwrap().blocking[0]
            .op_id
            .is_some()
    );
    assert_eq!(
        serde_json::to_value(&blocked.next_actions).unwrap(),
        json!(["fix_input"])
    );
    let stale = t.propose(&note_doc(issues[2], 5, "stale")).await.unwrap();
    assert_eq!(stale.validation.as_ref().unwrap().status, "conflict");
    assert_eq!(
        stale.validation.as_ref().unwrap().blocking[0].code,
        "conflict"
    );
    let n = count(&pool, "SELECT count(*) FROM changesets").await;
    // Not delegated: the field (priority_id is not writable), a project without delegation.
    let e = t
        .propose(&doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(issues[2]), "set": {"due_date": "2026-12-01"}})],
            vec![pre(issues[2], 0)],
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    let e = t.propose(&note_doc(o1, 0, "elsewhere")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = t
        .propose(&note_doc(Uuid::now_v7(), 0, "nothing"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changesets").await,
        n,
        "rejections store nothing"
    );
    assert_eq!(n, proposals_before + 2);

    // ---------------------------------------------- owner reads (REST)
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/changesets/{cs1}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["proposal"]["title"], "agent proposal");
    assert_eq!(r.body["data"]["proposal"]["exec_mode"], "atomic");
    assert_eq!(r.body["data"]["edit_method"], "agent_propose");
    assert_eq!(r.body["data"]["targets"][0]["key"], "WEB-1");
    let r = call(&router, &ab, "GET", "/changesets", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body["data"]["items"].as_array().unwrap().len() >= 4);
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/changesets/{cs1}/versions/1"),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["data"]["version"], 1);
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/changesets/{cs1}/versions/9"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Someone else's proposal does not exist for bob.
    let bb = login(&router, "bob").await;
    let r = call(
        &router,
        &bb,
        "GET",
        &format!("/changesets/{cs1}"),
        None,
        None,
    )
    .await;
    assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "not_found"));
    let r = call(
        &router,
        &bb,
        "POST",
        &format!("/changesets/{cs1}/approve-and-apply"),
        Some(json!({"version": 1, "proposal_hash": p1.proposal_hash})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Validate (§8.5): read only.
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{cs1}/validations"),
        Some(json!({"version": 1})),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["status"], "passed");
    assert_eq!(r.body["data"]["effects"][0]["status"], "would_change");
    assert_eq!(r.body["data"]["checks"]["workflow"], "passed");
    let v = changesets::validate(
        &app,
        alice,
        stale.changeset_id,
        &ValidateInput { version: 1 },
    )
    .await
    .unwrap();
    assert_eq!(v.status, "conflict");
    assert_eq!(v.checks["preconditions"], "failed");

    // T-S12: no free-text comment on a decision.
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{cs1}/approve-and-apply"),
        Some(json!({"version": 1, "proposal_hash": p1.proposal_hash, "comment": "lgtm"})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{cs1}/approve-and-apply"),
        Some(json!({"version": 1, "proposal_hash": p1.proposal_hash})),
        None,
    )
    .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "invalid_request"),
        "Idempotency-Key"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changeset_decisions").await,
        0
    );

    // ---------------------------------------------- approve and apply (§8.6)
    let ka = Uuid::now_v7();
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{cs1}/approve-and-apply"),
        Some(json!({"version": 1, "proposal_hash": p1.proposal_hash})),
        Some(ka),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let ex = &r.body["data"];
    assert_eq!(ex["status"], "succeeded");
    assert_eq!(ex["attempt_no"], 1);
    assert_eq!(ex["effects"][0]["key"], "WEB-1");
    assert_eq!(ex["effects"][0]["lock_version"], 1);
    let token = ex["search"]["consistent_after"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(token.starts_with("c1."));
    let exec1: Uuid = ex["execution_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(lock_version(&pool, w1).await, 1, "one bump for two ops");
    assert_eq!(
        text(
            &pool,
            &format!("SELECT subject FROM issues WHERE id = '{w1}'")
        )
        .await,
        "one (triaged)"
    );
    // Attribution (§16.2): actor = agent, approver = alice, on behalf of alice.
    let (origin, actor_kind, actor, approver, obo, proposer, cs_ref, ex_ref): (
        String,
        String,
        Uuid,
        Uuid,
        Uuid,
        Uuid,
        Uuid,
        Uuid,
    ) = sqlx::query_as(
        "SELECT origin, actor_kind, actor_id, approver_id, on_behalf_of_id, proposer_id, changeset_id, execution_id
         FROM issue_events WHERE issue_id = $1 AND kind = 'note_added'",
    )
    .bind(w1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (
            origin.as_str(),
            actor_kind.as_str(),
            actor,
            approver,
            obo,
            proposer,
            cs_ref,
            ex_ref
        ),
        (
            "external_agent",
            "agent",
            agent_id,
            alice_id,
            alice_id,
            agent_id,
            cs1,
            exec1
        )
    );
    let (author, author_obo): (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT author_id, created_on_behalf_of_id FROM issue_notes WHERE issue_id = $1",
    )
    .bind(w1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((author, author_obo), (agent_id, Some(alice_id)));
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM execution_field_changes WHERE execution_id = '{exec1}'")
        )
        .await,
        1,
        "agent writes keep the before/after copy"
    );
    assert_eq!(
        text(
            &pool,
            &format!("SELECT state FROM changesets WHERE id = '{cs1}'")
        )
        .await,
        "succeeded"
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM changeset_applications WHERE changeset_id = '{cs1}'")
        )
        .await,
        1
    );
    assert_eq!(
        text(
            &pool,
            &format!("SELECT status FROM operation_requests WHERE operation_id = '{ka}'")
        )
        .await,
        "completed"
    );

    // Replays (§8.7): the same key → the same execution; a new key → already_applied.
    let again = t.approve(&p1, ka).await.unwrap();
    assert_eq!(again.execution_id, exec1);
    assert_eq!(status(&again), "succeeded");
    // T-I2 (approve): the same key with another payload is not a replay.
    let other = changesets::get(&app, alice, *ids.iter().next().unwrap(), None)
        .await
        .unwrap();
    let e = t
        .approve_v(cs1, 1, &other.proposal_hash, ka)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::IdempotencyKeyReused);
    let e = t.approve(&p1, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyApplied);
    assert_eq!(lock_version(&pool, w1).await, 1);
    // §8.5: a finished proposal is not validated again.
    let e = changesets::validate(&app, alice, cs1, &ValidateInput { version: 1 })
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AlreadyApplied);
    let e = changesets::cancel(&app, alice, Uuid::now_v7(), cs1)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetNotExecutable);

    // T-S10: GET changeset, GET operation and GET execution give the same token.
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/changesets/{cs1}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        r.body["data"]["last_execution"]["search"]["consistent_after"],
        token
    );
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/operations/{ka}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.body["data"]["execution"]["search"]["consistent_after"],
        token
    );
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/executions/{exec1}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["data"]["search"]["consistent_after"], token);
    // T-S11: after an epoch rotation the stored token is returned, never rebuilt.
    exec(&pool, "UPDATE search_settings SET data_epoch = uuidv7()").await;
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/executions/{exec1}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["data"]["search"]["consistent_after"], token);
    let again = t.approve(&p1, ka).await.unwrap();
    assert_eq!(
        again.execution.unwrap().search.unwrap().consistent_after,
        token
    );

    // T-I1 (approve): 100 concurrent sends of one key → one execution, one note.
    let w2 = issues[1];
    let p2 = changesets::get(&app, alice, *ids.iter().next().unwrap(), None)
        .await
        .unwrap();
    let k2 = Uuid::now_v7();
    let mut hs = Vec::new();
    for _ in 0..100 {
        let (app, h, cs) = (app.clone(), p2.proposal_hash.clone(), p2.changeset_id);
        hs.push(tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                k2,
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        }));
    }
    let mut execs = std::collections::BTreeSet::new();
    for h in hs {
        match h.await.unwrap() {
            Ok(o) => {
                execs.insert(o.execution_id);
            }
            Err(e) => assert_eq!(code(&e), ErrorCode::TemporarilyUnavailable, "{e:#}"),
        }
    }
    assert_eq!(execs.len(), 1);
    let o = t
        .approve_v(p2.changeset_id, 1, &p2.proposal_hash, k2)
        .await
        .unwrap();
    assert_eq!(status(&o), "succeeded");
    assert_eq!(notes(&pool, w2).await, 1, "one note");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM changeset_executions WHERE changeset_id = '{}'",
                p2.changeset_id
            )
        )
        .await,
        1
    );

    // ---------------------------------------------- failure, retry, revise (§8.2)
    let w3 = issues[2];
    let p3 = t.propose(&note_doc(w3, 0, "after triage")).await.unwrap();
    assert_eq!(p3.validation.as_ref().unwrap().status, "passed");
    // Someone changes the issue after the proposal: the apply fails with a conflict.
    exec(
        &pool,
        &format!("UPDATE issues SET lock_version = 1 WHERE id = '{w3}'"),
    )
    .await;
    let kf = Uuid::now_v7();
    let o = t.approve(&p3, kf).await.unwrap();
    assert!(!o.pending);
    let ex = o.execution.unwrap();
    assert_eq!(ex.status, "failed");
    assert_eq!(ex.error.as_ref().unwrap().code, "conflict");
    assert_eq!(ex.items[0].status, "not_run");
    assert_eq!(notes(&pool, w3).await, 0);
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT state FROM changesets WHERE id = '{}'",
                p3.changeset_id
            )
        )
        .await,
        "failed"
    );
    // The same key replays the failed execution; a new key is attempt 2.
    let o = t.approve(&p3, kf).await.unwrap();
    assert_eq!((o.execution_id, status(&o)), (ex.execution_id, "failed"));
    let o = t.approve(&p3, Uuid::now_v7()).await.unwrap();
    assert_eq!(o.execution.as_ref().unwrap().attempt_no, 2);
    assert_eq!(status(&o), "failed");
    // The agent revises on the current lock_version (T-R1: the old version is gone).
    let rv = changesets::revise(
        &app,
        agent,
        Uuid::now_v7(),
        p3.changeset_id,
        1,
        &note_doc(w3, 1, "after triage (v2)"),
        ORIGIN,
    )
    .await
    .unwrap();
    assert_eq!(
        (rv.version, rv.validation.as_ref().unwrap().status.as_str()),
        (2, "passed")
    );
    let e = t.approve(&p3, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::VersionSuperseded);
    let e = t
        .approve_v(p3.changeset_id, 2, &p1.proposal_hash, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::ApprovalInvalidated,
        "the hash binds the approval"
    );
    let e = changesets::revise(
        &app,
        agent,
        Uuid::now_v7(),
        p3.changeset_id,
        1,
        &note_doc(w3, 1, "stale base"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::VersionSuperseded);
    let o = t.approve(&rv, Uuid::now_v7()).await.unwrap();
    assert_eq!(status(&o), "succeeded");
    assert_eq!(notes(&pool, w3).await, 1);

    // ---------------------------------------------- reject, cancel, owner revise
    let w4 = issues[3];
    let pr = t.propose(&note_doc(w4, 0, "reject me")).await.unwrap();
    let e = changesets::reject(
        &app,
        alice,
        Uuid::now_v7(),
        pr.changeset_id,
        &DecisionInput {
            version: 1,
            proposal_hash: p1.proposal_hash.clone(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ApprovalInvalidated);
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/reject", pr.changeset_id),
        Some(json!({"version": 1, "proposal_hash": pr.proposal_hash})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["state"], "rejected");
    let e = t.approve(&pr, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetNotExecutable);
    let e = changesets::revise(
        &app,
        agent,
        Uuid::now_v7(),
        pr.changeset_id,
        1,
        &note_doc(w4, 0, "x"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetNotExecutable);

    // The agent cancels its own proposal; the owner too (REST).
    let pc = t.propose(&note_doc(w4, 0, "cancel me")).await.unwrap();
    let s = changesets::cancel(&app, agent, Uuid::now_v7(), pc.changeset_id)
        .await
        .unwrap();
    assert_eq!(s.state, "canceled");
    let pc2 = t.propose(&note_doc(w4, 0, "cancel me too")).await.unwrap();
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/cancel", pc2.changeset_id),
        Some(json!({})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(r.body["data"]["state"], "canceled", "{}", r.body);
    let e = t.approve(&pc2, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetNotExecutable);

    // Owner revise (§8.5): a conflicted proposal fixed by the owner within the ceiling.
    let w5 = issues[4];
    exec(
        &pool,
        &format!("UPDATE issues SET lock_version = 3 WHERE id = '{w5}'"),
    )
    .await;
    let po = t.propose(&note_doc(w5, 0, "needs a fix")).await.unwrap();
    assert_eq!(po.validation.as_ref().unwrap().status, "conflict");
    let ko = Uuid::now_v7();
    let rev = json!({"expected_current_version": 1, "proposal": note_doc(w5, 3, "fixed by alice")});
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/revisions", po.changeset_id),
        Some(rev.clone()),
        Some(ko),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["version"], 2);
    assert_eq!(r.body["data"]["edit_method"], "owner_revise");
    assert_eq!(r.body["data"]["validation"]["status"], "passed");
    let h2 = r.body["data"]["proposal_hash"]
        .as_str()
        .unwrap()
        .to_string();
    // Resending the revision is the same version.
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/revisions", po.changeset_id),
        Some(rev),
        Some(ko),
    )
    .await;
    assert_eq!(r.body["data"]["version"], 2);
    // Beyond the ceiling (due_date is not writable for the agent): forbidden.
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/revisions", po.changeset_id),
        Some(json!({"expected_current_version": 2, "proposal": doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(w5), "set": {"due_date": "2026-12-01"}})],
            vec![pre(w5, 3)])})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::FORBIDDEN, "forbidden_op"),
        "{}",
        r.body
    );
    let o = t
        .approve_v(po.changeset_id, 2, &h2, Uuid::now_v7())
        .await
        .unwrap();
    assert_eq!(status(&o), "succeeded");
    let (body, obo): (String, Option<Uuid>) =
        sqlx::query_as("SELECT body, created_on_behalf_of_id FROM issue_notes WHERE issue_id = $1")
            .bind(w5)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((body.as_str(), obo), ("fixed by alice", Some(alice_id)));

    // T-W7: is_private is fixed at proposal time from the tracker default.
    let pw = t
        .propose(&doc(
            vec![json!({"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": feature,
                        "fields": {"subject": "private by default"}})],
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(
        pw.validation.as_ref().unwrap().status,
        "passed",
        "{:?}",
        pw.validation
    );
    let stored = changesets::get(&app, alice, pw.changeset_id, None)
        .await
        .unwrap();
    assert_eq!(
        stored.proposal.unwrap()["ops"][0]["fields"]["is_private"],
        json!(true)
    );
    exec(
        &pool,
        &format!("UPDATE trackers SET private_by_default = false WHERE id = '{feature}'"),
    )
    .await;
    let o = t.approve(&pw, Uuid::now_v7()).await.unwrap();
    assert_eq!(status(&o), "succeeded");
    let created = o.execution.unwrap().effects[0].issue_id;
    let (private, author, cobo): (bool, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT is_private, author_id, created_on_behalf_of_id FROM issues WHERE id = $1",
    )
    .bind(created)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((private, author, cobo), (true, agent_id, Some(alice_id)));

    // ---------------------------------------------- crash injection (C-1…C-6, T-I5)
    // Each: cut the request off at one point, resend the same key → zero or
    // exactly one effect, one attempt.
    for point in [
        "approve.before_request",
        "approve.after_a",
        "apply.after_locks",
        "apply.before_commit",
        "apply.after_commit",
    ] {
        let w = issues[5];
        let lv = lock_version(&pool, w).await;
        let before = notes(&pool, w).await;
        let p = t.propose(&note_doc(w, lv, point)).await.unwrap();
        let k = Uuid::now_v7();
        faults::arm(point, Action::Fail);
        match t.approve(&p, k).await {
            Err(e) => assert_eq!(point, "approve.before_request", "{e:#}"),
            Ok(o) => {
                assert!(o.pending, "{point}: {}", status(&o));
                if point != "apply.after_commit" {
                    // T-I5: reading a queued execution starts nothing.
                    let v = changesets::get_execution(&app, alice, o.execution_id)
                        .await
                        .unwrap();
                    assert_eq!(v.status, "queued", "{point}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    assert_eq!(notes(&pool, w).await, before, "{point}: no effect yet");
                }
            }
        }
        let o = t.approve(&p, k).await.unwrap();
        assert_eq!(status(&o), "succeeded", "{point}");
        assert_eq!(o.execution.as_ref().unwrap().attempt_no, 1, "{point}");
        assert_eq!(notes(&pool, w).await, before + 1, "{point}: exactly once");
        assert_eq!(
            count(
                &pool,
                &format!(
                    "SELECT count(*) FROM changeset_executions WHERE changeset_id = '{}'",
                    p.changeset_id
                )
            )
            .await,
            1,
            "{point}"
        );
    }
    // C-6: a domain failure cut off before it is recorded.
    let w = issues[5];
    let lv = lock_version(&pool, w).await;
    let p = t.propose(&note_doc(w, lv, "c6")).await.unwrap();
    exec(
        &pool,
        &format!("UPDATE issues SET lock_version = lock_version + 1 WHERE id = '{w}'"),
    )
    .await;
    let k = Uuid::now_v7();
    faults::arm("apply.before_fail_record", Action::Fail);
    let o = t.approve(&p, k).await.unwrap();
    assert!(o.pending);
    let o = t.approve(&p, k).await.unwrap();
    assert_eq!(status(&o), "failed");
    assert_eq!(o.execution.unwrap().attempt_no, 1);

    // T-I6: a deadlock inside B retries the same execution.
    let lv = lock_version(&pool, w).await;
    let before = notes(&pool, w).await;
    let p = t.propose(&note_doc(w, lv, "deadlock")).await.unwrap();
    faults::arm("apply.after_locks", Action::Deadlock);
    let o = t.approve(&p, Uuid::now_v7()).await.unwrap();
    assert_eq!(status(&o), "succeeded");
    assert_eq!(o.execution.unwrap().attempt_no, 1);
    assert_eq!(notes(&pool, w).await, before + 1);

    // §8.6 over REST: an unknown result is 202 with where to look and what to do.
    let lv = lock_version(&pool, w).await;
    let p = t.propose(&note_doc(w, lv, "rest 202")).await.unwrap();
    let k = Uuid::now_v7();
    let body = json!({"version": 1, "proposal_hash": p.proposal_hash});
    faults::arm("apply.before_commit", Action::Fail);
    let path = format!("/changesets/{}/approve-and-apply", p.changeset_id);
    let r = call(&router, &ab, "POST", &path, Some(body.clone()), Some(k)).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    let eid = r.body["data"]["execution_id"].as_str().unwrap().to_string();
    assert_eq!(
        r.headers[header::LOCATION].to_str().unwrap(),
        format!("/api/v1/executions/{eid}")
    );
    assert_eq!(r.headers[header::RETRY_AFTER].to_str().unwrap(), "2");
    assert_eq!(r.body["data"]["client_action"], "retry_same_request");
    assert_eq!(r.body["data"]["can_cancel_apply"], false);
    let r = call(
        &router,
        &ab,
        "GET",
        &format!("/executions/{eid}"),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["data"]["status"], "queued");
    let r = call(&router, &ab, "POST", &path, Some(body), Some(k)).await;
    assert_eq!(
        (r.status, r.body["data"]["status"].as_str()),
        (StatusCode::OK, Some("succeeded"))
    );
    assert_eq!(r.body["data"]["execution_id"].as_str().unwrap(), eid);

    // T-I4: an expired result is not run again.
    let lv = lock_version(&pool, w).await;
    let p = t.propose(&note_doc(w, lv, "tombstone")).await.unwrap();
    let k = Uuid::now_v7();
    t.approve(&p, k).await.unwrap();
    exec(&pool, &format!("UPDATE operation_requests SET result_expires_at = now() - interval '1 second' WHERE operation_id = '{k}'")).await;
    let e = t.approve(&p, k).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::OperationResultExpired);
    let r = call(&router, &ab, "GET", &format!("/operations/{k}"), None, None).await;
    assert_eq!(r.code(), "operation_result_expired");

    // ---------------------------------------------- races (§9.5)
    let w6 = issues[6];
    // T-R10: B fails, the same key succeeds in the gap, then the first request's C.
    let pt = t
        .propose(&doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(w6), "to_status_id": done})],
            vec![pre(w6, 0)],
        ))
        .await
        .unwrap();
    assert_eq!(pt.validation.as_ref().unwrap().status, "blocked");
    let k = Uuid::now_v7();
    let g = Arc::new(Gate::default());
    faults::arm("apply.before_fail_record", Action::Block(g.clone()));
    let first = {
        let (app, h, cs) = (app.clone(), pt.proposal_hash.clone(), pt.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                k,
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    // The workflow now allows New → Done; the same key runs B again and succeeds.
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_transitions VALUES ('{bug}','{dev}','{new}','{done}','always')"
        ),
    )
    .await;
    let o = t.approve(&pt, k).await.unwrap();
    assert_eq!(
        status(&o),
        "succeeded",
        "{:?}",
        o.execution.as_ref().map(|e| (&e.error, &e.items))
    );
    g.release.notify_one();
    let o1 = first.await.unwrap().unwrap();
    assert_eq!(status(&o1), "succeeded", "C found the application");
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT state FROM changesets WHERE id = '{}'",
                pt.changeset_id
            )
        )
        .await,
        "succeeded"
    );
    assert_eq!(
        text(
            &pool,
            &format!("SELECT status FROM operation_requests WHERE operation_id = '{k}'")
        )
        .await,
        "completed"
    );
    exec(
        &pool,
        &format!("DELETE FROM workflow_transitions WHERE new_status_id = '{done}'"),
    )
    .await;

    // T-R12: B fails, the owner cancels in the gap, then C keeps canceled.
    let w7 = issues[7];
    let pt = t
        .propose(&doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(w7), "to_status_id": done})],
            vec![pre(w7, 0)],
        ))
        .await
        .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.before_fail_record", Action::Block(g.clone()));
    let first = {
        let (app, h, cs) = (app.clone(), pt.proposal_hash.clone(), pt.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    let s = changesets::cancel(&app, alice, Uuid::now_v7(), pt.changeset_id)
        .await
        .unwrap();
    assert_eq!(s.state, "canceled");
    g.release.notify_one();
    let o1 = first.await.unwrap().unwrap();
    let ex = o1.execution.unwrap();
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "changeset_canceled")
    );
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT state FROM changesets WHERE id = '{}'",
                pt.changeset_id
            )
        )
        .await,
        "canceled"
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM changeset_executions WHERE changeset_id = '{}' AND status IN ('queued','running')", pt.changeset_id)).await,
        0
    );

    // T-R2: cancel waits for an apply holding the ChangeSet lock, then sees it applied.
    let lv = lock_version(&pool, w7).await;
    let p = t.propose(&note_doc(w7, lv, "r2")).await.unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.after_locks", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), p.proposal_hash.clone(), p.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    let canceling = {
        let (app, cs) = (app.clone(), p.changeset_id);
        tokio::spawn(async move { changesets::cancel(&app, alice, Uuid::now_v7(), cs).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !canceling.is_finished(),
        "cancel waits for the ChangeSet lock"
    );
    g.release.notify_one();
    assert_eq!(status(&applying.await.unwrap().unwrap()), "succeeded");
    let e = canceling.await.unwrap().unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetNotExecutable);

    // T-R3: two keys for one version at once → one application.
    let lv = lock_version(&pool, w7).await;
    let p = t.propose(&note_doc(w7, lv, "r3")).await.unwrap();
    let before = notes(&pool, w7).await;
    let mut hs = Vec::new();
    for _ in 0..2 {
        let (app, h, cs) = (app.clone(), p.proposal_hash.clone(), p.changeset_id);
        hs.push(tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        }));
    }
    let mut ok = 0;
    for h in hs {
        match h.await.unwrap() {
            Ok(o) => {
                assert_eq!(status(&o), "succeeded");
                ok += 1;
            }
            Err(e) => assert!(
                matches!(
                    code(&e),
                    ErrorCode::AlreadyApplied | ErrorCode::ExecutionInProgress
                ),
                "{e:#}"
            ),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(notes(&pool, w7).await, before + 1);

    // T-R5: a delegation revoked before step 5 → zero effect; after step 5 → waits.
    for (point, expect_ok) in [("apply.before_step5", false), ("apply.after_step5", true)] {
        // A fresh delegation for this round (the previous one is revoked below).
        let d = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM delegations WHERE revoked_at IS NULL AND project_id = $1",
        )
        .bind(web)
        .fetch_one(&pool)
        .await
        .unwrap();
        let lv = lock_version(&pool, w7).await;
        let before = notes(&pool, w7).await;
        let p = t.propose(&note_doc(w7, lv, point)).await.unwrap();
        let g = Arc::new(Gate::default());
        faults::arm(point, Action::Block(g.clone()));
        let applying = {
            let (app, h, cs) = (app.clone(), p.proposal_hash.clone(), p.changeset_id);
            tokio::spawn(async move {
                changesets::approve_and_apply(
                    &app,
                    alice,
                    Uuid::now_v7(),
                    cs,
                    &ApproveInput {
                        version: 1,
                        proposal_hash: h,
                        review_mode: None,
                    },
                )
                .await
            })
        };
        g.reached.notified().await;
        let revoking = {
            let pool = pool.clone();
            tokio::spawn(async move {
                sqlx::query("UPDATE delegations SET revoked_at = now(), revoked_reason = 'test' WHERE id = $1")
                    .bind(d)
                    .execute(&pool)
                    .await
                    .unwrap();
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(revoking.is_finished(), !expect_ok, "{point}");
        g.release.notify_one();
        let o = applying.await.unwrap().unwrap();
        revoking.await.unwrap();
        if expect_ok {
            assert_eq!(status(&o), "succeeded", "{point}");
            assert_eq!(notes(&pool, w7).await, before + 1);
        } else {
            let ex = o.execution.unwrap();
            assert_eq!(
                (ex.status.as_str(), ex.error.unwrap().code.as_str()),
                ("failed", "delegation_unavailable"),
                "{point}"
            );
            assert_eq!(notes(&pool, w7).await, before, "zero effect");
        }
        // A new delegation for the same context and project (§3.5).
        clone_delegation(&pool, d).await;
    }

    // T-R7: waiting for the issue lock until the proposal expired → zero effect.
    let lv = lock_version(&pool, w7).await;
    let before = notes(&pool, w7).await;
    let p = t.propose(&note_doc(w7, lv, "r7")).await.unwrap();
    set_expiry(&pool, p.changeset_id, 1.5).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM issues WHERE id = $1 FOR UPDATE")
        .bind(w7)
        .execute(&mut *holder)
        .await
        .unwrap();
    let applying = {
        let (app, h, cs) = (app.clone(), p.proposal_hash.clone(), p.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(2000)).await;
    holder.commit().await.unwrap();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "changeset_expired")
    );
    assert_eq!(notes(&pool, w7).await, before);
    // Before acceptance an expired proposal is 410 and creates nothing.
    let e = t.approve(&p, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ChangesetExpired);
    assert_eq!(changesets::expire_due(&app).await.unwrap(), 1);
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT state FROM changesets WHERE id = '{}'",
                p.changeset_id
            )
        )
        .await,
        "expired"
    );

    // T-R13: a creating proposal waits for the project FOR UPDATE until expired.
    let pcreate = t
        .propose(&doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": bug,
                        "fields": {"subject": "late"}}),
            ],
            vec![],
        ))
        .await
        .unwrap();
    set_expiry(&pool, pcreate.changeset_id, 1.5).await;
    let issues_before = count(&pool, "SELECT count(*) FROM issues").await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(web)
        .execute(&mut *holder)
        .await
        .unwrap();
    let applying = {
        let (app, h, cs) = (
            app.clone(),
            pcreate.proposal_hash.clone(),
            pcreate.changeset_id,
        );
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(2000)).await;
    holder.commit().await.unwrap();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    assert_eq!(ex.error.unwrap().code, "changeset_expired");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issues").await,
        issues_before
    );

    // T-R15: a rotation waits for an apply past step 8b; the receipt keeps the old epoch.
    let lv = lock_version(&pool, w7).await;
    let p = t.propose(&note_doc(w7, lv, "r15")).await.unwrap();
    let old_epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.after_locks", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), p.proposal_hash.clone(), p.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    // With a lock_timeout the rotation gives up and the write is unaffected.
    let mut rot = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '200ms'")
        .execute(&mut *rot)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE search_settings SET data_epoch = uuidv7()")
            .execute(&mut *rot)
            .await
            .is_err()
    );
    drop(rot);
    let rotating = {
        let pool = pool.clone();
        tokio::spawn(async move {
            sqlx::query_scalar::<_, Uuid>(
                "UPDATE search_settings SET data_epoch = uuidv7() RETURNING data_epoch",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rotating.is_finished(), "the rotation waits for the write");
    g.release.notify_one();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    let new_epoch = rotating.await.unwrap();
    let tok = ex.search.unwrap().consistent_after;
    let xid = tok.rsplit('.').next().unwrap();
    assert_eq!(tok, oxledger::commands::token(old_epoch, xid));
    assert_ne!(old_epoch, new_epoch);

    // ---------------------------------------------- T-A7, T-R14: revocation and closing
    // Token rotation: a new credential does not touch existing proposals.
    let lv = lock_version(&pool, w7).await;
    let keep = t
        .propose(&note_doc(w7, lv, "kept across rotation"))
        .await
        .unwrap();
    let cred2 = agents::issue_credential(&pool, &policy, &alice, deleg.context_id, "t2", None)
        .await
        .unwrap();
    agents::revoke_credential(&pool, &alice, cred.id)
        .await
        .unwrap();
    assert!(
        auth::authenticate_bearer(&pool, &cred.token)
            .await
            .unwrap()
            .is_none()
    );
    let agent2 = auth::authenticate_bearer(&pool, &cred2.token)
        .await
        .unwrap()
        .unwrap();
    // The old token's context no longer authenticates.
    let e = changesets::propose(
        &app,
        agent,
        Uuid::now_v7(),
        &note_doc(w7, lv, "old token"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unauthenticated);
    let o = t.approve(&keep, Uuid::now_v7()).await.unwrap();
    assert_eq!(status(&o), "succeeded", "rotation keeps proposals");

    // The client disabled: an existing proposal cannot be applied.
    let lv = lock_version(&pool, w7).await;
    let pd = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "client off"),
        ORIGIN,
    )
    .await
    .unwrap();
    agents::set_agent_client_enabled(&pool, "Claude Code", false)
        .await
        .unwrap();
    let e = changesets::propose(&app, agent2, Uuid::now_v7(), &note_doc(w7, lv, "x"), ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unauthenticated);
    let o = t.approve(&pd, Uuid::now_v7()).await.unwrap();
    assert_eq!(
        o.execution.unwrap().error.unwrap().code,
        "delegation_unavailable"
    );
    agents::set_agent_client_enabled(&pool, "Claude Code", true)
        .await
        .unwrap();

    // T-A7: an expired delegation is unavailable for approving too.
    let live: Uuid = id(
        &pool,
        &format!("SELECT id FROM delegations WHERE project_id = '{web}' AND revoked_at IS NULL"),
    )
    .await;
    let lv = lock_version(&pool, w7).await;
    let pe = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "expiring"),
        ORIGIN,
    )
    .await
    .unwrap();
    exec(&pool, &format!("UPDATE delegations SET created_at = now() - interval '2 days', expires_at = now() - interval '1 second' WHERE id = '{live}'")).await;
    let e = t.approve(&pe, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable);
    exec(&pool, &format!("UPDATE delegations SET created_at = now(), expires_at = now() + interval '20 days' WHERE id = '{live}'")).await;

    // §8.6: the owner can no longer read a target → proposal_unavailable, nothing accepted.
    let pu = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "unreadable"),
        ORIGIN,
    )
    .await
    .unwrap();
    // The issue moves to a project alice is not in (no epoch change in R:
    // membership elsewhere is untouched).
    let hidden = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('HID','Hidden') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("UPDATE issues SET project_id = '{hidden}' WHERE id = '{w7}'"),
    )
    .await;
    let decisions = count(&pool, "SELECT count(*) FROM changeset_decisions").await;
    let e = t.approve(&pu, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ProposalUnavailable, "{e:#}");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changeset_decisions").await,
        decisions
    );
    exec(
        &pool,
        &format!("UPDATE issues SET project_id = '{web}' WHERE id = '{w7}'"),
    )
    .await;

    // §8.5 / M5: an owner revision cannot move the proposal to another project.
    let other_issue = id(
        &pool,
        "SELECT id FROM issues WHERE subject = 'other project'",
    )
    .await;
    let r = call(
        &router,
        &ab,
        "POST",
        &format!("/changesets/{}/revisions", pu.changeset_id),
        Some(json!({"expected_current_version": 1, "proposal": note_doc(other_issue, 0, "elsewhere")})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM changeset_versions WHERE changeset_id = '{}'",
                pu.changeset_id
            )
        )
        .await,
        1
    );
    // An agent revision while an approved execution is pending: execution_in_progress.
    let k = Uuid::now_v7();
    faults::arm("approve.after_a", Action::Fail);
    assert!(t.approve(&pu, k).await.unwrap().pending);
    let e = changesets::revise(
        &app,
        agent2,
        Uuid::now_v7(),
        pu.changeset_id,
        1,
        &note_doc(w7, lv, "late"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ExecutionInProgress);
    // T-R12 (expiry half): expire_due ends the queued execution too.
    changesets::expire_due(&app).await.unwrap(); // the T-R13 proposal
    set_expiry(&pool, pu.changeset_id, -1.0).await;
    assert_eq!(changesets::expire_due(&app).await.unwrap(), 1);
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT state FROM changesets WHERE id = '{}'",
                pu.changeset_id
            )
        )
        .await,
        "expired"
    );
    let o = t.approve(&pu, k).await.unwrap();
    let ex = o.execution.unwrap();
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "changeset_expired")
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM changeset_executions WHERE changeset_id = '{}' AND status IN ('queued','running')", pu.changeset_id)).await,
        0
    );

    // T-R5 (session half): the approving session revoked before step 5 → zero effect.
    let ab2 = login(&router, "alice").await;
    let alice2 = auth::session_from_cookie(&pool, &policy, &ab2.session)
        .await
        .unwrap()
        .unwrap()
        .ctx;
    let before = notes(&pool, w7).await;
    let ps = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "session"),
        ORIGIN,
    )
    .await
    .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.before_step5", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), ps.proposal_hash.clone(), ps.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice2,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    let sid = alice2.session_id().unwrap();
    auth::revoke_session(&pool, &alice, sid, "test")
        .await
        .unwrap();
    g.release.notify_one();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "unauthenticated")
    );
    assert_eq!(notes(&pool, w7).await, before, "zero effect");

    // After step 5 the session is held FOR SHARE: a revocation waits for the write.
    let ab3 = login(&router, "alice").await;
    let alice3 = auth::session_from_cookie(&pool, &policy, &ab3.session)
        .await
        .unwrap()
        .unwrap()
        .ctx;
    let lv = lock_version(&pool, w7).await;
    let before = notes(&pool, w7).await;
    let ps = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "session held"),
        ORIGIN,
    )
    .await
    .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.after_step5", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), ps.proposal_hash.clone(), ps.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice3,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    let revoking = {
        let pool = pool.clone();
        let sid = alice3.session_id().unwrap();
        tokio::spawn(async move {
            auth::revoke_session(&pool, &alice, sid, "test")
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !revoking.is_finished(),
        "the revocation waits for the session lock"
    );
    g.release.notify_one();
    assert_eq!(status(&applying.await.unwrap().unwrap()), "succeeded");
    revoking.await.unwrap();
    assert_eq!(notes(&pool, w7).await, before + 1);
    let lv = lock_version(&pool, w7).await;

    // T-R5 (suspension half): the approver suspended before step 5 → unauthenticated.
    let before = notes(&pool, w7).await;
    let ps = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "suspended"),
        ORIGIN,
    )
    .await
    .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.before_step5", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), ps.proposal_hash.clone(), ps.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    exec(
        &pool,
        &format!("UPDATE principals SET status = 'locked' WHERE id = '{alice_id}'"),
    )
    .await;
    g.release.notify_one();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    exec(
        &pool,
        &format!("UPDATE principals SET status = 'active' WHERE id = '{alice_id}'"),
    )
    .await;
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "unauthenticated")
    );
    assert_eq!(notes(&pool, w7).await, before, "zero effect");

    // Locking a user bumps every project (T-X12): the context is quarantined
    // even after alice is active again. She starts a new work context.
    let e = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "old ctx"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    agents::new_context(&pool, &policy, &alice, client, true)
        .await
        .unwrap();
    let deleg = agents::create_delegation(
        &pool,
        &policy,
        &alice,
        DelegationInput {
            agent_client_id: client,
            project_id: web,
            proposal_ops: [
                "create_issue",
                "update_fields",
                "transition",
                "add_note",
                "duplicate_of",
            ]
            .map(String::from)
            .to_vec(),
            writable_fields: ["subject", "description", "assigned_to_id"]
                .map(String::from)
                .to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
        },
    )
    .await
    .unwrap();
    let cred3 = agents::issue_credential(&pool, &policy, &alice, deleg.context_id, "t3", None)
        .await
        .unwrap();
    let agent2 = auth::authenticate_bearer(&pool, &cred3.token)
        .await
        .unwrap()
        .unwrap();

    // The agent suspended before step 5 → delegation_unavailable, zero effect.
    let lv = lock_version(&pool, w7).await;
    let ps = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "agent off"),
        ORIGIN,
    )
    .await
    .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.before_step5", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), ps.proposal_hash.clone(), ps.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    exec(
        &pool,
        &format!("UPDATE principals SET status = 'locked' WHERE id = '{agent_id}'"),
    )
    .await;
    g.release.notify_one();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    exec(
        &pool,
        &format!("UPDATE principals SET status = 'active' WHERE id = '{agent_id}'"),
    )
    .await;
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "delegation_unavailable")
    );
    assert_eq!(notes(&pool, w7).await, before);

    // The delegation revoked: propose → delegation_unavailable; approve → 409 before acceptance.
    let lv = lock_version(&pool, w7).await;
    let pv = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "revoked"),
        ORIGIN,
    )
    .await
    .unwrap();
    exec(&pool, &format!("UPDATE delegations SET revoked_at = now(), revoked_reason = 'test' WHERE project_id = '{web}' AND revoked_at IS NULL")).await;
    let e = changesets::propose(&app, agent2, Uuid::now_v7(), &note_doc(w7, lv, "x"), ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable, "{e:#}");
    let decisions = count(&pool, "SELECT count(*) FROM changeset_decisions").await;
    let e = t.approve(&pv, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changeset_decisions").await,
        decisions,
        "rejected before acceptance: no rows"
    );
    // The context closed: context_unavailable.
    clone_delegation(&pool, deleg.id).await;
    let pz = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "closing"),
        ORIGIN,
    )
    .await
    .unwrap();
    let pz2 = changesets::propose(
        &app,
        agent2,
        Uuid::now_v7(),
        &note_doc(w7, lv, "closing too"),
        ORIGIN,
    )
    .await
    .unwrap();
    // T-R14: closed between A and B → B re-checks the context (step 4b), zero effect.
    let before = notes(&pool, w7).await;
    let g = Arc::new(Gate::default());
    faults::arm("approve.after_a", Action::Block(g.clone()));
    let applying = {
        let (app, h, cs) = (app.clone(), pz.proposal_hash.clone(), pz.changeset_id);
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice,
                Uuid::now_v7(),
                cs,
                &ApproveInput {
                    version: 1,
                    proposal_hash: h,
                    review_mode: None,
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    agents::close_context(&pool, &alice, deleg.context_id)
        .await
        .unwrap();
    g.release.notify_one();
    let ex = applying.await.unwrap().unwrap().execution.unwrap();
    assert_eq!(
        (ex.status.as_str(), ex.error.unwrap().code.as_str()),
        ("failed", "context_unavailable")
    );
    assert_eq!(notes(&pool, w7).await, before);
    // After closing, approving is refused before acceptance.
    let e = t.approve(&pz2, Uuid::now_v7()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable, "{e:#}");

    // ---------------------------------------------- DB guards (§8.1)
    let e = sqlx::query("UPDATE changeset_decisions SET review_mode = 'queue'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("append-only") || e.to_string().contains("append"),
        "{e}"
    );
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "UPDATE changeset_executions SET error_code = 'x' WHERE id = '{exec1}'"
    )))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("finished"), "{e}");
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "UPDATE changesets SET state = 'proposed' WHERE id = '{cs1}'"
    )))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("terminal"), "{e}");
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "UPDATE changesets SET expires_at = now() WHERE id = '{}'",
        pv.changeset_id
    )))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("immutable"), "{e}");
    let e = sqlx::query("DELETE FROM changeset_applications")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("append"), "{e}");
    // An execution can only reference an approval.
    let reject_decision: Uuid = id(
        &pool,
        "SELECT id FROM changeset_decisions WHERE decision = 'reject' LIMIT 1",
    )
    .await;
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO execution_refs (id, kind) VALUES ('{x}', 'changeset');
         INSERT INTO changeset_executions (id, changeset_id, version, proposal_hash, attempt_no, operation_request_id,
                                           decision_id, executor_id, executor_session_id, status)
         SELECT '{x}', d.changeset_id, d.version, d.proposal_hash, 9, (SELECT id FROM operation_requests LIMIT 1),
                d.id, d.decider_id, d.session_id, 'queued' FROM changeset_decisions d WHERE d.id = '{reject_decision}'",
        x = Uuid::now_v7()
    )))
    .execute(&pool)
    .await;
    assert!(e.is_err(), "a rejection is not executable");
    faults::clear();
}

/// T-S7: every migration from an empty database, the S5 FKs on issue_events included.
#[tokio::test]
async fn s5_migrations_from_empty() {
    let url = match std::env::var("OXLEDGER_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger_test".into(),
    };
    let (base, rest) = url.rsplit_once('/').unwrap();
    let name = format!("{}_s7", rest.split('?').next().unwrap());
    let admin = PgPool::connect(&format!("{base}/postgres")).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let pool = oxledger::db::connect(&format!("{base}/{name}"))
        .await
        .unwrap();
    let fks: Vec<String> = sqlx::query_scalar(
        "SELECT conname::text FROM pg_constraint WHERE conrelid = 'issue_events'::regclass AND contype = 'f'
         AND conname LIKE 'issue_events_%changeset%' OR conname = 'issue_events_execution_fk' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        fks,
        [
            "issue_events_changeset_op_fk",
            "issue_events_changeset_version_fk",
            "issue_events_execution_fk"
        ]
    );
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}

/// A new delegation like `d` (same context and project), with its policy
/// rows (§16.3 rule 1: a missing row is "off").
async fn clone_delegation(pool: &PgPool, d: Uuid) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "WITH n AS (
           INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id, security_epoch,
                                    proposal_ops, writable_fields, expires_at)
           SELECT agent_principal_id, delegator_user_id, context_id, project_id, security_epoch,
                  proposal_ops, writable_fields, now() + interval '20 days' FROM delegations WHERE id = '{d}'
           RETURNING id),
         c AS (INSERT INTO delegation_command_policies
               SELECT n.id, command_kind, mode FROM n, delegation_command_policies WHERE delegation_id = '{d}')
         INSERT INTO delegation_field_policies
         SELECT n.id, field_key, mode FROM n, delegation_field_policies WHERE delegation_id = '{d}'"
    )))
    .execute(pool)
    .await
    .unwrap();
}
