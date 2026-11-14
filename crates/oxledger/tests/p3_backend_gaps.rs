// SPDX-License-Identifier: GPL-2.0-or-later
//! P3 backend gaps from the frontend (docs/impl/p1-backend-gaps.md G38–G47).
//!
//! * G38: command effects name the issue events they recorded.
//! * G39: `TeamDetail.safety_summary` splits direct and review commands.
//! * G40 / G41: issue history attribution (actor kind, on behalf of,
//!   approver) and the agent chain flag.
//! * G42: `ActivityItem.self_triage` and `GET /ai-team/activity`.
//! * G43: form version creator / publisher names.
//! * G44: `GET /time-entries` tracker and fixed_version filters.
//! * G46: permission flags on `/projects` and view details.
//! * G47: log_time's issue key is stored as an id; older key-form bodies
//!   still show their summary row.
//!
//! G45 (AI settings audit) is in m1_10_ai.rs, next to its helpers.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents::{self, DelegationInput, PolicyInput};
use oxledger::ai_team;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands;
use oxledger::error::ErrorCode;
use oxledger::forms::{self, DraftInput, NewForm, Part};
use oxledger::policy::PolicyMode::{self, Direct, Review};
use oxledger::time_reports::{self, ListQuery as TimeQuery};
use oxledger::undo::{self, ActivityQuery, UndoInput};
use oxledger::{issue_view, review_queue, views};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

/// Every test resets the database (World::build): one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------- helpers

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn key_of(w: &World, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "p3", "ops": ops, "preconditions": pre})
}

fn pre(i: Uuid, lv: i32) -> Value {
    json!({"target": tgt(i), "lock_version": lv})
}

/// The per-agent write rates (quota.rs) are not what this file tests.
async fn clear_rates(w: &World) {
    exec(&w.pool, "DELETE FROM agent_write_log").await;
}

async fn submit(w: &World, agent: AuthContext, d: &Value) -> SubmitResult {
    clear_rates(w).await;
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN)
        .await
        .unwrap()
}

fn applied(r: &SubmitResult) -> Uuid {
    match r {
        SubmitResult::Applied { execution_id, .. } => *execution_id,
        other => panic!("expected applied, got {other:?}"),
    }
}

async fn agent_note(w: &World, agent: AuthContext, i: Uuid, body: &str) -> Uuid {
    applied(&submit(w, agent, &note_doc(i, lv(w, i).await, body)).await)
}

async fn agent_subject(w: &World, agent: AuthContext, i: Uuid, s: &str) -> Uuid {
    applied(&submit(w, agent, &subject_doc(i, lv(w, i).await, s)).await)
}

/// A delegation of `project` from `who` to the world's client.
async fn delegate_as(
    w: &World,
    who: &AuthContext,
    project: Uuid,
    ops: &[&str],
) -> agents::DelegationView {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        who,
        DelegationInput {
            agent_client_id: w.client,
            project_id: project,
            proposal_ops: ops.iter().map(|s| s.to_string()).collect(),
            writable_fields: ["subject", "description", "assigned_to_id"]
                .map(String::from)
                .to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap()
}

const OPS: &[&str] = &[
    "create_issue",
    "update_fields",
    "transition",
    "add_note",
    "duplicate_of",
];

async fn token_as(w: &World, who: &AuthContext, context: Uuid) -> AuthContext {
    let c = agents::issue_credential(&w.pool, &w.policy, who, context, "t", None)
        .await
        .unwrap();
    auth::authenticate_bearer(&w.pool, &c.token)
        .await
        .unwrap()
        .unwrap()
}

/// The readable history of `i` for `who`, newest first, as JSON.
async fn history(w: &World, who: AuthContext, i: Uuid) -> Vec<Value> {
    let v = issue_view::spa_view(
        &w.app,
        who,
        &key_of(w, i).await,
        Some(vec!["recent_events".into()]),
        None,
        Some(20),
    )
    .await
    .unwrap();
    serde_json::to_value(&v).unwrap()["recent_events"]["items"]
        .as_array()
        .unwrap()
        .clone()
}

fn ids(a: &undo::Activity) -> Vec<Uuid> {
    a.items.iter().filter_map(|i| i.execution_id).collect()
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "https://spa.test";

struct Browser {
    session: String,
    csrf: String,
}

fn cookie(resp: &axum::response::Response, name: &str) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{name}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap()
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
    let c = cookie(&lc, LOGIN_COOKIE);
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={c}"))
                .header("x-csrf-token", token)
                .header(header::ORIGIN, ORIGIN_SPA)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = cookie(&resp, SESSION_COOKIE);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

async fn get(r: &Router, b: &Browser, path: &str) -> (StatusCode, Value) {
    let req = Request::get(format!("/api/v1{path}"))
        .header(header::ORIGIN, ORIGIN_SPA)
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
        .header("x-csrf-token", &b.csrf)
        .body(Body::empty())
        .unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    )
}

// ---------------------------------------------------------------- G38

#[tokio::test]
async fn g38_effects_name_the_recorded_events() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let i = w.issue(w.web, "g38").await;
    let d = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "renamed"}}),
            json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": w.doing}),
            json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "a note"}),
        ],
        vec![pre(i, 0)],
    );
    clear_rates(&w).await;
    let op_id = Uuid::now_v7();
    let first = changesets::command_submit(&w.app, agent, op_id, &d, false, ORIGIN)
        .await
        .unwrap();
    let SubmitResult::Applied { effects, .. } = &first else {
        panic!("{first:?}")
    };
    assert_eq!(effects.len(), 1);
    let kinds: BTreeSet<&str> = effects[0].events.iter().map(String::as_str).collect();
    assert_eq!(
        kinds,
        ["fields_changed", "note_added", "status_changed"].into(),
        "{effects:?}"
    );
    // A replay comes from the stored result: the same kinds.
    let again = changesets::command_submit(&w.app, agent, op_id, &d, false, ORIGIN)
        .await
        .unwrap();
    let SubmitResult::Applied { effects: e2, .. } = &again else {
        panic!("{again:?}")
    };
    assert_eq!(e2[0].events, effects[0].events);

    // A person's create: `created`.
    let r = commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "new"}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.effects[0].events, ["created"]);

    // Hidden: an issue the agent can no longer read leaves the replay
    // without its effect (and so without its kinds); the other one stays.
    let d2 = doc(
        vec![
            json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                    "tracker_id": w.bug, "fields": {"subject": "made by the agent"}}),
            json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "again"}),
        ],
        vec![pre(i, lv(&w, i).await)],
    );
    clear_rates(&w).await;
    let op2 = Uuid::now_v7();
    let r2 = changesets::command_submit(&w.app, agent, op2, &d2, false, ORIGIN)
        .await
        .unwrap();
    let SubmitResult::Applied { effects: e2, .. } = &r2 else {
        panic!("{r2:?}")
    };
    assert_eq!(e2.len(), 2);
    let made = e2.iter().find(|e| e.created).unwrap().issue_id;
    assert_eq!(e2.iter().find(|e| e.created).unwrap().events, ["created"]);
    exec(
        &pool,
        &format!("UPDATE issues SET archived_at = now() WHERE id = '{made}'"),
    )
    .await;
    let gone = changesets::command_submit(&w.app, agent, op2, &d2, false, ORIGIN)
        .await
        .unwrap();
    let SubmitResult::Applied { effects: e3, .. } = &gone else {
        panic!("{gone:?}")
    };
    assert_eq!(e3.len(), 1, "{e3:?}");
    assert_eq!(e3[0].issue_id, i);
    assert_eq!(e3[0].events, ["note_added"]);
}

// ---------------------------------------------------------------- G39

#[tokio::test]
async fn g39_safety_summary_splits_direct_and_review() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    let d = w.delegate(w.web).await.unwrap();
    w.delegate(w.auth_p).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let agent_id = agent.principal_id().unwrap();
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        PolicyInput {
            commands: BTreeMap::from([("transition".to_string(), Review)]),
            fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    // The sponsor's own policy rows are the reference.
    let mine = ai_team::detail(&w.app, w.alice_ctx, agent_id)
        .await
        .unwrap();
    let web_policy = mine
        .policies
        .iter()
        .find(|p| p.project_id == w.web)
        .unwrap();
    let with = |m: PolicyMode| -> Vec<String> {
        web_policy
            .commands
            .iter()
            .filter(|(_, x)| **x == m)
            .map(|(c, _)| c.clone())
            .collect()
    };
    assert!(with(Review).contains(&"transition".to_string()));
    assert!(!with(Direct).is_empty());

    // carol (a member of WEB only, not a manager) sees no policy rows but
    // both columns of WEB's summary.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let seen = ai_team::detail(&w.app, carol, agent_id).await.unwrap();
    assert!(seen.policies.is_empty());
    assert_eq!(seen.safety_summary.len(), 1, "{:?}", seen.safety_summary);
    let s = &seen.safety_summary[0];
    assert_eq!(s.project_id, w.web);
    assert_eq!(s.direct, with(Direct));
    assert_eq!(s.review, with(Review));
    assert!(!s.direct.contains(&"transition".to_string()));
    let mut both: Vec<String> = s.direct.iter().chain(&s.review).cloned().collect();
    both.sort();
    assert_eq!(both, s.allowed_commands);
    // Hidden: AUTH (carol is not a member) appears nowhere.
    let text = serde_json::to_string(&seen).unwrap();
    assert!(!text.contains("\"AUTH\""), "{text}");
    assert!(!text.contains(&w.auth_p.to_string()), "{text}");
    // bob is in both projects: two rows.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let b = ai_team::detail(&w.app, bob, agent_id).await.unwrap();
    assert_eq!(b.safety_summary.len(), 2);
    // Someone in neither project does not see the role at all.
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let dave = sign_in(&pool, &w.policy, "dave").await;
    let e = ai_team::detail(&w.app, dave, agent_id).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- G40 / G41

#[tokio::test]
async fn g40_g41_history_attribution_and_chain() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let i = w.issue(w.web, "g40").await;

    agent_note(&w, agent, i, "first").await;
    agent_note(&w, agent, i, "second").await;
    let h = history(&w, bob, i).await;
    // Newest first: the second write extended the chain to 2.
    assert_eq!(h[0]["actor_kind"], "agent");
    assert_eq!(h[0]["on_behalf_of"]["name"], "alice");
    assert_eq!(h[0]["on_behalf_of"]["id"], json!(w.alice));
    assert_eq!(h[0]["agent_chain"], true, "{:?}", h[0]);
    assert_eq!(h[1]["actor_kind"], "agent");
    assert!(h[1].get("agent_chain").is_none(), "one write is no chain");
    assert!(h[0].get("approved_by").is_none());
    // Only a flag: no count anywhere.
    assert!(!h[0].to_string().contains("chain_writes"));

    // A person in between ends the chain; the next agent write starts anew.
    let r = commands::submit(
        &w.app,
        bob,
        Uuid::now_v7(),
        &note_doc(i, lv(&w, i).await, "a person"),
    )
    .await
    .unwrap();
    assert_eq!(r.effects[0].events, ["note_added"]);
    agent_note(&w, agent, i, "third").await;
    let h = history(&w, bob, i).await;
    assert_eq!(h[1]["actor_kind"], "user");
    assert!(h[1].get("on_behalf_of").is_none());
    assert!(h[0].get("agent_chain").is_none(), "{:?}", h[0]);

    // An approved proposal: 「Alice 核准」.
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i, lv(&w, i).await, "proposed"),
        ORIGIN,
    )
    .await
    .unwrap();
    let out = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p.changeset_id,
        &ApproveInput {
            version: p.version,
            proposal_hash: p.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        out.execution.as_ref().map(|e| e.status.as_str()),
        Some("succeeded")
    );
    let h = history(&w, bob, i).await;
    assert_eq!(h[0]["actor_kind"], "agent");
    assert_eq!(h[0]["on_behalf_of"]["name"], "alice");
    assert_eq!(h[0]["approved_by"]["name"], "alice", "{:?}", h[0]);
    let approved_event: Uuid = serde_json::from_value(h[0]["event_id"].clone()).unwrap();

    // Hidden: an approver other than the person the agent acted for is
    // shown only to that approver and to site administrators.
    exec(
        &pool,
        &format!(
            "UPDATE issue_events SET approver_id = '{}' WHERE id = '{approved_event}'",
            w.carol
        ),
    )
    .await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let root_id = auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    for (who, shown) in [
        (bob, false),
        (w.alice_ctx, false),
        (carol, true),
        (root, true),
    ] {
        let h = history(&w, who, i).await;
        let e = h
            .iter()
            .find(|e| e["event_id"] == json!(approved_event))
            .unwrap();
        assert_eq!(e.get("approved_by").is_some(), shown, "{e:?}");
        if shown {
            assert_eq!(e["approved_by"]["name"], "carol");
        }
        // The rest of the attribution is the same for everyone.
        assert_eq!(e["on_behalf_of"]["name"], "alice");
    }
    let _ = root_id;
    // Someone who cannot see the issue gets nothing.
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let dave = sign_in(&pool, &w.policy, "dave").await;
    let e = issue_view::spa_view(
        &w.app,
        dave,
        &key_of(&w, i).await,
        Some(vec!["recent_events".into()]),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- G42

#[tokio::test]
async fn g42_self_triage_and_activity_across_roles() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["triage_issues"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}'
             WHERE project_id = '{}'",
            w.done, w.web
        ),
    )
    .await;
    let a = w.alice_ctx;
    // alice's agent X: WEB, with triage_decide.
    let ctx_x = w.new_context().await;
    let mut ops: Vec<&str> = OPS.to_vec();
    ops.push("triage_decide");
    delegate_as(&w, &a, w.web, &ops).await;
    let x = token_as(&w, &a, ctx_x).await;
    let x_id = x.principal_id().unwrap();
    // bob's agent Y: WEB and AUTH.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let ctx_y = agents::new_context(&pool, &w.policy, &bob, w.client, true)
        .await
        .unwrap();
    delegate_as(&w, &bob, w.web, OPS).await;
    delegate_as(&w, &bob, w.auth_p, OPS).await;
    let y = token_as(&w, &bob, ctx_y).await;
    let y_id = y.principal_id().unwrap();

    // X files an issue and accepts it itself (A5 self_triage).
    let r = submit(
        &w,
        x,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "x filed"}}),
            ],
            vec![],
        ),
    )
    .await;
    let SubmitResult::Applied { effects, .. } = &r else {
        panic!("{r:?}")
    };
    let filed = effects[0].issue_id;
    let triaged = applied(
        &submit(
            &w,
            x,
            &doc(
                vec![
                    json!({"op_id": op(), "kind": "triage_decide", "target": tgt(filed),
                            "decision": "accept"}),
                ],
                vec![pre(filed, lv(&w, filed).await)],
            ),
        )
        .await,
    );
    let i_web = w.issue(w.web, "web").await;
    let i_auth = w.issue(w.auth_p, "auth").await;
    let x_plain = agent_subject(&w, x, i_web, "x wrote").await;
    // Y may have read AUTH and WEB: it writes where every reader can read
    // both (AUTH); a write to WEB would be an audience expansion (carol).
    let y_first = agent_subject(&w, y, i_auth, "y wrote").await;
    let y_auth = agent_subject(&w, y, i_auth, "y wrote again").await;

    // The single-role list carries the flag.
    let own = undo::activity_page(&w.app, a, x_id, &ActivityQuery::default())
        .await
        .unwrap();
    let flag = |act: &undo::Activity, id: Uuid| {
        act.items
            .iter()
            .find(|i| i.execution_id == Some(id))
            .unwrap()
            .self_triage
    };
    assert!(flag(&own, triaged));
    assert!(!flag(&own, x_plain));
    assert!(own.items.iter().all(|i| i.agent.is_none()), "untagged");

    // The merged list (alice: X as sponsor, Y as a member of WEB).
    let all = ai_team::activity_all(&w.app, a, None, None, &ActivityQuery::default())
        .await
        .unwrap();
    let got: BTreeSet<Uuid> = ids(&all).into_iter().collect();
    for e in [triaged, x_plain, y_first, y_auth] {
        assert!(got.contains(&e), "{e} missing from {all:?}");
    }
    assert!(flag(&all, triaged));
    for it in &all.items {
        let ag = it.agent.as_ref().expect("tagged");
        let want = if [y_first, y_auth].contains(&it.execution_id.unwrap()) {
            y_id
        } else {
            x_id
        };
        assert_eq!(ag.agent_principal_id, want);
    }
    // Newest first by (at, id).
    let order: Vec<_> = all.items.iter().map(|i| i.at).collect();
    let mut sorted = order.clone();
    sorted.sort_by(|p, q| q.cmp(p));
    assert_eq!(order, sorted);

    // role: one role; a role the caller does not see is an empty list.
    let only_y = ai_team::activity_all(&w.app, a, Some(y_id), None, &ActivityQuery::default())
        .await
        .unwrap();
    assert_eq!(
        ids(&only_y).into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([y_first, y_auth])
    );
    let none = ai_team::activity_all(
        &w.app,
        a,
        Some(Uuid::now_v7()),
        None,
        &ActivityQuery::default(),
    )
    .await
    .unwrap();
    assert!(none.items.is_empty() && none.next_cursor.is_none());

    // runtime: only roles whose runtime the caller may see.
    let rt = |agent: Uuid| {
        let pool = pool.clone();
        async move {
            id(
                &pool,
                &format!(
                    "SELECT runtime_id FROM agent_principals WHERE agent_principal_id = '{agent}'"
                ),
            )
            .await
        }
    };
    let (rt_x, rt_y) = (rt(x_id).await, rt(y_id).await);
    let on_x = ai_team::activity_all(&w.app, a, None, Some(rt_x), &ActivityQuery::default())
        .await
        .unwrap();
    assert!(!on_x.items.is_empty());
    assert!(
        on_x.items
            .iter()
            .all(|i| i.agent.as_ref().unwrap().agent_principal_id == x_id)
    );
    let on_y = ai_team::activity_all(&w.app, a, None, Some(rt_y), &ActivityQuery::default())
        .await
        .unwrap();
    assert!(
        on_y.items.is_empty(),
        "alice sees Y only as a member: its runtime is not hers to filter by"
    );
    let bob_on_y = ai_team::activity_all(&w.app, bob, None, Some(rt_y), &ActivityQuery::default())
        .await
        .unwrap();
    assert!(!bob_on_y.items.is_empty());

    // carol (WEB only) sees role Y (it is delegated WEB) but not its AUTH
    // writes, and paging one by one neither shortens a page nor makes an
    // extra one.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let c_all = ai_team::activity_all(&w.app, carol, None, None, &ActivityQuery::default())
        .await
        .unwrap();
    let c_ids: BTreeSet<Uuid> = ids(&c_all).into_iter().collect();
    assert!(!c_ids.contains(&y_auth) && !c_ids.contains(&y_first));
    assert!(c_ids.contains(&x_plain) && c_ids.contains(&triaged));
    assert!(ai_team::detail(&w.app, carol, y_id).await.is_ok());
    let c_y = ai_team::activity_all(&w.app, carol, Some(y_id), None, &ActivityQuery::default())
        .await
        .unwrap();
    assert!(c_y.items.is_empty() && c_y.next_cursor.is_none());
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let p = ai_team::activity_all(
            &w.app,
            carol,
            None,
            None,
            &ActivityQuery {
                limit: Some(1),
                cursor: cursor.take(),
                ..ActivityQuery::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(p.items.len(), 1, "every page is full");
        paged.extend(ids(&p));
        match p.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, ids(&c_all));
    // The cursor is bound to the filters.
    let p1 = ai_team::activity_all(
        &w.app,
        carol,
        None,
        None,
        &ActivityQuery {
            limit: Some(1),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    let e = ai_team::activity_all(
        &w.app,
        carol,
        Some(y_id),
        None,
        &ActivityQuery {
            limit: Some(1),
            cursor: p1.next_cursor.clone(),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    // ...and to the runtime (alice: X's runtime, then none).
    let p1 = ai_team::activity_all(
        &w.app,
        a,
        None,
        Some(rt_x),
        &ActivityQuery {
            limit: Some(1),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert!(p1.next_cursor.is_some());
    let e = ai_team::activity_all(
        &w.app,
        a,
        None,
        None,
        &ActivityQuery {
            limit: Some(1),
            cursor: p1.next_cursor.clone(),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);

    // alice's undo of X's write shows under X, not under Y.
    undo::apply(&w.app, a, Uuid::now_v7(), x_plain, &UndoInput::default())
        .await
        .unwrap();
    let all = ai_team::activity_all(&w.app, a, None, None, &ActivityQuery::default())
        .await
        .unwrap();
    let u = all
        .items
        .iter()
        .find(|i| i.kind == "undo")
        .expect("the undo");
    assert_eq!(u.agent.as_ref().unwrap().agent_principal_id, x_id);
    let only_y = ai_team::activity_all(&w.app, a, Some(y_id), None, &ActivityQuery::default())
        .await
        .unwrap();
    assert!(only_y.items.iter().all(|i| i.kind != "undo"));

    // HTTP: the route and its parameters.
    let r = router(&w);
    let br = login(&r, "carol").await;
    let (s, body) = get(&r, &br, &format!("/ai-team/activity?role={x_id}")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i["agent"]["handle"].is_string()));
    let (s, _) = get(&r, &br, "/ai-team/activity?runtime=nope").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------- G43

#[tokio::test]
async fn g43_form_version_names() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(&pool, mgr, &["view_issues", "manage_project"]).await;
    for u in [w.bob, w.carol] {
        exec(
            &pool,
            &format!(
                "INSERT INTO member_roles SELECT id, '{mgr}' FROM members
                 WHERE project_id = '{}' AND principal_id = '{u}'",
                w.web
            ),
        )
        .await;
    }
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let form = forms::ensure(
        &w.app,
        root,
        &NewForm {
            tracker_id: w.bug,
            project: Some("WEB".into()),
        },
    )
    .await
    .unwrap()
    .summary
    .id;
    let body = json!({"rules": [{"id": "due_when_assigned", "field": "due_date", "kind": "required_if",
                                 "when": {"all": [{"field": "assignee", "op": "is_set"}]}}]});
    // v1 by root (a site admin, not a member of WEB), v2 by bob.
    let v1 = forms::put_draft(
        &w.app,
        root,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body: body.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(v1.created_by_name.as_deref(), Some("root"), "your own name");
    let v2 = forms::put_draft(
        &w.app,
        bob,
        form,
        Part::Rules,
        &DraftInput {
            base_version: v1.version,
            body,
        },
    )
    .await
    .unwrap();
    assert_eq!(v2.created_by_name.as_deref(), Some("bob"));
    // v1 was published by bob (the publish flow itself is m1_5's).
    exec(
        &pool,
        &format!(
            "UPDATE form_rule_versions SET state = 'superseded', published_by = '{}', published_at = now()
             WHERE form_id = '{form}' AND version = {}",
            w.bob, v1.version
        ),
    )
    .await;
    let names = |vs: &[forms::FormVersionView], v: i32| {
        let x = vs.iter().find(|x| x.version == v).unwrap();
        (x.created_by_name.clone(), x.published_by_name.clone())
    };
    // carol: bob is a member of WEB; root is not.
    let c = forms::versions(&w.app, carol, form, Part::Rules)
        .await
        .unwrap();
    assert_eq!(names(&c, v2.version), (Some("bob".into()), None));
    assert_eq!(names(&c, v1.version), (None, Some("bob".into())));
    assert_eq!(
        c.iter()
            .find(|x| x.version == v1.version)
            .unwrap()
            .created_by,
        v1.created_by
    );
    // The detail follows the same rule.
    let d = forms::get(&w.app, carol, form).await.unwrap();
    assert_eq!(
        d.rules.draft.as_ref().unwrap().created_by_name.as_deref(),
        Some("bob")
    );
    // root (site admin) sees every name.
    let r = forms::versions(&w.app, root, form, Part::Rules)
        .await
        .unwrap();
    assert_eq!(
        names(&r, v1.version),
        (Some("root".into()), Some("bob".into()))
    );
    // Not even the name: nowhere in carol's answer.
    let text = serde_json::to_string(&c).unwrap();
    assert!(!text.contains("\"root\""), "{text}");
    // A non-manager cannot read the versions at all.
    let alice = w.alice_ctx;
    let e = forms::versions(&w.app, alice, form, Part::Rules)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
}

// ---------------------------------------------------------------- G44

#[tokio::test]
async fn g44_time_entries_tracker_and_version_filters() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility, time_entries_visibility) VALUES ('Lead','all','all') RETURNING id",
    )
    .await;
    grant(&pool, lead, &["view_issues", "view_time_entries"]).await;
    for (p, u) in [(w.web, w.alice), (w.auth_p, w.alice), (w.web, w.carol)] {
        exec(
            &pool,
            &format!(
                "INSERT INTO member_roles SELECT id, '{lead}' FROM members
                 WHERE project_id = '{p}' AND principal_id = '{u}'"
            ),
        )
        .await;
    }
    let feature = id(
        &pool,
        &format!(
            "INSERT INTO trackers (name, position, default_status_id) VALUES ('Feature', 2, '{}') RETURNING id",
            w.new
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO project_trackers VALUES ('{}', '{feature}')",
            w.web
        ),
    )
    .await;
    let v1 = id(
        &pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, created_by)
             VALUES ('{}', 'v1', 'v1', '{}') RETURNING id",
            w.web, w.alice
        ),
    )
    .await;
    let bug_v1 = w.issue(w.web, "bug in v1").await;
    let bug = w.issue(w.web, "bug").await;
    let feat = w.issue(w.web, "feature").await;
    let auth_bug = w.issue(w.auth_p, "auth bug").await;
    exec(
        &pool,
        &format!(
            "UPDATE issues SET fixed_version_id = '{v1}' WHERE id = '{bug_v1}';
             UPDATE issues SET tracker_id = '{feature}' WHERE id = '{feat}';"
        ),
    )
    .await;
    let act = id(
        &pool,
        "SELECT id FROM time_entry_activities WHERE is_default",
    )
    .await;
    let entry = |project: Uuid, issue: Option<Uuid>, user: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO time_entries (project_id, issue_id, user_id, author_id, activity_id, spent_on, hours)
                 VALUES ($1, $2, $3, $3, $4, current_date, 1) RETURNING id",
            )
            .bind(project)
            .bind(issue)
            .bind(user)
            .bind(act)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let e_bug_v1 = entry(w.web, Some(bug_v1), w.bob).await;
    let e_bug = entry(w.web, Some(bug), w.bob).await;
    let e_feat = entry(w.web, Some(feat), w.carol).await;
    let e_none = entry(w.web, None, w.bob).await;
    let e_auth = entry(w.auth_p, Some(auth_bug), w.bob).await;

    let list = |who: AuthContext, tracker: Option<String>, fixed_version: Option<String>| {
        let app = w.app.clone();
        async move {
            time_reports::list(
                &app,
                who,
                &TimeQuery {
                    tracker,
                    fixed_version,
                    ..TimeQuery::default()
                },
            )
            .await
        }
    };
    let set = |p: &time_reports::TimeEntryPage| -> BTreeSet<Uuid> {
        p.items.iter().map(|i| i.id).collect()
    };
    let a = w.alice_ctx;
    let s = |v: &[Uuid]| v.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(
        set(&list(a, Some(w.bug.to_string()), None).await.unwrap()),
        s(&[e_bug_v1, e_bug, e_auth])
    );
    assert_eq!(
        set(&list(a, Some(feature.to_string()), None).await.unwrap()),
        s(&[e_feat])
    );
    assert_eq!(
        set(&list(a, Some("none".into()), None).await.unwrap()),
        s(&[e_none])
    );
    assert_eq!(
        set(&list(a, None, Some(v1.to_string())).await.unwrap()),
        s(&[e_bug_v1])
    );
    assert_eq!(
        set(&list(a, None, Some("none".into())).await.unwrap()),
        s(&[e_bug, e_feat, e_none, e_auth])
    );
    assert_eq!(
        set(&list(a, Some(w.bug.to_string()), Some("none".into()))
            .await
            .unwrap()),
        s(&[e_bug, e_auth])
    );
    // The drill-down matches the summary's group.
    let sum = time_reports::summary(
        &w.app,
        a,
        &time_reports::SummaryQuery {
            from: Some("2000-01-01".into()),
            to: Some("2000-12-31".into()),
            group_by: Some("tracker".into()),
            ..Default::default()
        },
    )
    .await;
    assert!(sum.is_ok());
    // Hidden: carol (WEB only) never gets AUTH's entry through a filter.
    assert_eq!(
        set(&list(carol_ctx(&w).await, Some(w.bug.to_string()), None)
            .await
            .unwrap()),
        s(&[e_bug_v1, e_bug])
    );
    // Bad values.
    for (t, v) in [
        (Some("bug".to_string()), None),
        (None, Some("v1".to_string())),
    ] {
        let e = list(a, t, v).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidArgument, "{e:#}");
    }
    // The cursor is bound to the filters.
    let p1 = time_reports::list(
        &w.app,
        a,
        &TimeQuery {
            tracker: Some(w.bug.to_string()),
            limit: Some(1),
            ..TimeQuery::default()
        },
    )
    .await
    .unwrap();
    let e = time_reports::list(
        &w.app,
        a,
        &TimeQuery {
            limit: Some(1),
            cursor: p1.next_cursor.clone(),
            ..TimeQuery::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    // fixed_version too (with the same tracker, only it differs).
    let p1 = time_reports::list(
        &w.app,
        a,
        &TimeQuery {
            fixed_version: Some("none".into()),
            limit: Some(1),
            ..TimeQuery::default()
        },
    )
    .await
    .unwrap();
    assert!(p1.next_cursor.is_some());
    let e = time_reports::list(
        &w.app,
        a,
        &TimeQuery {
            limit: Some(1),
            cursor: p1.next_cursor.clone(),
            ..TimeQuery::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    let e = time_reports::list(
        &w.app,
        a,
        &TimeQuery {
            fixed_version: Some(v1.to_string()),
            limit: Some(1),
            cursor: p1.next_cursor.clone(),
            ..TimeQuery::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    // HTTP: the query parameters reach the list.
    let r = router(&w);
    let b = login(&r, "alice").await;
    let (st, body) = get(&r, &b, &format!("/time-entries?fixed_version={v1}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let got: Vec<&str> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, vec![e_bug_v1.to_string().as_str()]);
    let (st, _) = get(&r, &b, "/time-entries?tracker=x").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

async fn carol_ctx(w: &World) -> AuthContext {
    sign_in(&w.pool, &w.policy, "carol").await
}

// ---------------------------------------------------------------- G46

#[tokio::test]
async fn g46_permission_flags() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let views_role = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('ViewMgr','all') RETURNING id",
    )
    .await;
    grant(&pool, views_role, &["view_issues", "manage_public_views"]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{views_role}' FROM members
             WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.bob
        ),
    )
    .await;
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','all') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    let dave = auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, dave, viewer).await;

    let r = router(&w);
    let flags = |body: &Value, key: &str| -> Option<Value> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == key)
            .map(|p| p["permissions"].clone())
    };
    let alice = login(&r, "alice").await;
    let (s, body) = get(&r, &alice, "/projects").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        flags(&body, "WEB"),
        Some(json!({"add_issues": true, "manage_public_views": false}))
    );
    let bob = login(&r, "bob").await;
    let (_, body) = get(&r, &bob, "/projects").await;
    assert_eq!(
        flags(&body, "WEB"),
        Some(json!({"add_issues": true, "manage_public_views": true}))
    );
    assert_eq!(
        flags(&body, "AUTH"),
        Some(json!({"add_issues": true, "manage_public_views": false}))
    );
    let d = login(&r, "dave").await;
    let (_, body) = get(&r, &d, "/projects").await;
    assert_eq!(
        flags(&body, "WEB"),
        Some(json!({"add_issues": false, "manage_public_views": false}))
    );
    // Hidden: a project dave cannot see is not listed.
    assert_eq!(flags(&body, "AUTH"), None);
    // A closed project takes no new issues (and no project views).
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            w.auth_p
        ),
    )
    .await;
    let (_, body) = get(&r, &bob, "/projects").await;
    assert_eq!(
        flags(&body, "AUTH"),
        Some(json!({"add_issues": false, "manage_public_views": false}))
    );

    // ViewDetail: the built-in board of WEB.
    let board = format!("builtin:project_board:{}", w.web);
    let bob_ctx = sign_in(&pool, &w.policy, "bob").await;
    let v = views::get(&w.app, bob_ctx, &board).await.unwrap();
    let p = v.permissions.unwrap();
    assert!(p.add_issues && p.manage_public_views);
    let v = views::get(&w.app, w.alice_ctx, &board).await.unwrap();
    let p = v.permissions.unwrap();
    assert!(p.add_issues && !p.manage_public_views);
    let dave_ctx = sign_in(&pool, &w.policy, "dave").await;
    let v = views::get(&w.app, dave_ctx, &board).await.unwrap();
    let p = v.permissions.unwrap();
    assert!(!p.add_issues && !p.manage_public_views);
    // A personal view without a project has no flags.
    let (s, body) = get(&r, &alice, "/views?scope=personal").await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------- G47

#[tokio::test]
async fn g47_log_time_issue_key_becomes_an_id() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING;
         UPDATE org_settings SET timelog_max_hours_per_day = 0, timelog_accept_0_hours = true,
                timelog_accept_future_dates = true, timelog_accept_closed_issues = true,
                timelog_require_issue = false, timelog_require_comments = false;
         UPDATE time_entry_activities SET active = true",
    )
    .await;
    grant(&pool, w.dev, &["log_time", "view_time_entries"]).await;
    let ctx = w.new_context().await;
    let mut ops: Vec<&str> = OPS.to_vec();
    ops.push("log_time");
    delegate_as(&w, &w.alice_ctx, w.web, &ops).await;
    let agent = token_as(&w, &w.alice_ctx, ctx).await;
    let i = w.issue(w.web, "time me").await;
    let key = key_of(&w, i).await;
    let act = id(
        &pool,
        "SELECT id FROM time_entry_activities WHERE is_default",
    )
    .await;
    let fields =
        json!({"hours": "1.5", "spent_on": "2026-11-01", "activity_id": act, "comments": "x"});
    let d = doc(
        vec![
            json!({"op_id": op(), "kind": "log_time", "issue": {"type": "issue", "key": key.to_lowercase()},
                    "fields": fields}),
        ],
        vec![],
    );
    let p = changesets::propose(&w.app, agent, Uuid::now_v7(), &d, ORIGIN)
        .await
        .unwrap();
    // The stored body names the issue by id.
    let body: Value = sqlx::query_scalar(
        "SELECT c.body FROM changeset_versions v JOIN proposal_contents c ON c.id = v.content_id
         WHERE v.changeset_id = $1",
    )
    .bind(p.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        body["ops"][0]["issue"],
        json!({"type": "issue", "id": i}),
        "{body}"
    );
    // The proposal page and the queue show the 「記工時」 row.
    let v = changesets::get(&w.app, w.alice_ctx, p.changeset_id, None)
        .await
        .unwrap();
    let row = v
        .diff
        .iter()
        .find(|r| r.field == "time")
        .expect("a time row");
    assert_eq!(row.target.key, key);
    assert_eq!(row.after["hours"], "1.5");

    // Bodies stored before the fix (key form) still show their row.
    let legacy = doc(
        vec![
            json!({"op_id": op(), "kind": "log_time", "issue": {"type": "issue", "key": key},
                    "fields": fields}),
        ],
        vec![],
    );
    let mut c = pool.acquire().await.unwrap();
    let rows = review_queue::diff(&mut c, w.alice_ctx, &legacy, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].target.id, i);
    // Hidden: a key-form body on an issue the viewer cannot see has no row.
    let a_issue = w.issue(w.auth_p, "auth time").await;
    let hidden = doc(
        vec![json!({"op_id": op(), "kind": "log_time",
                    "issue": {"type": "issue", "key": key_of(&w, a_issue).await},
                    "fields": fields})],
        vec![],
    );
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let rows = review_queue::diff(&mut c, carol, &hidden, &BTreeMap::new())
        .await
        .unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    let rows = review_queue::diff(&mut c, w.alice_ctx, &hidden, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// ---------------------------------------------------------------- second round

/// G38: each event kind once per issue (two notes are one `note_added`).
#[tokio::test]
async fn g38_each_event_kind_once() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "twice").await;
    let r = commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "one"}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "two"}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "s"}}),
            ],
            vec![pre(i, lv(&w, i).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.effects.len(), 1);
    let ev = &r.effects[0].events;
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_eq!(
        ev.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        ["fields_changed", "note_added"].into()
    );
}

/// G40: the flag is the chain of the event's own issue, never the longest
/// chain the same write extended (verification probe v_g40); an issue
/// created by the write starts its own chain (no flag).
#[tokio::test]
async fn g40_chain_flag_is_per_issue() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let a = w.issue(w.web, "A").await;
    let b = w.issue(w.web, "B").await;
    // B: two agent writes (chain 2).
    agent_note(&w, agent, b, "b1").await;
    agent_note(&w, agent, b, "b2").await;
    // One document: the first agent write on A, the third on B.
    let d = doc(
        vec![
            json!({"op_id": op(), "kind": "add_note", "target": tgt(a), "body": "a1"}),
            json!({"op_id": op(), "kind": "add_note", "target": tgt(b), "body": "b3"}),
        ],
        vec![pre(a, lv(&w, a).await), pre(b, lv(&w, b).await)],
    );
    applied(&submit(&w, agent, &d).await);
    let chain_a: i32 =
        sqlx::query_scalar("SELECT writes FROM issue_agent_chains WHERE issue_id = $1")
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(chain_a, 1, "A's own chain is one write");
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let h = history(&w, bob, a).await;
    assert_eq!(h[0]["actor_kind"], "agent");
    assert!(
        h[0].get("agent_chain").is_none(),
        "A's first agent write is no chain, but got {:?}",
        h[0]
    );
    let h = history(&w, bob, b).await;
    assert_eq!(h[0]["agent_chain"], true, "B's third write: {:?}", h[0]);

    // A new issue written together with B (chain 4) starts at one.
    let r = submit(
        &w,
        agent,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "new"}}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(b), "body": "b4"}),
            ],
            vec![pre(b, lv(&w, b).await)],
        ),
    )
    .await;
    let SubmitResult::Applied { effects, .. } = &r else {
        panic!("{r:?}")
    };
    let made = effects.iter().find(|e| e.created).unwrap().issue_id;
    let h = history(&w, bob, made).await;
    assert_eq!(h[0]["actor_kind"], "agent");
    assert!(h.iter().all(|e| e.get("agent_chain").is_none()), "{h:?}");
    let h = history(&w, bob, b).await;
    assert_eq!(h[0]["agent_chain"], true);
}

/// G42: a Triage write whose decision event the viewer cannot read
/// (redacted) but whose note is readable is listed without the self_triage
/// flag; before the redaction the same item had it. (A triage document
/// writes only the triaged issue, so "the decision is unreadable, the rest
/// readable" happens within one issue.)
#[tokio::test]
async fn g42_self_triage_needs_a_readable_decision() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["triage_issues"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}'
             WHERE project_id = '{}'",
            w.done, w.web
        ),
    )
    .await;
    let a = w.alice_ctx;
    let ctx_x = w.new_context().await;
    let mut ops: Vec<&str> = OPS.to_vec();
    ops.push("triage_decide");
    delegate_as(&w, &a, w.web, &ops).await;
    let x = token_as(&w, &a, ctx_x).await;
    let x_id = x.principal_id().unwrap();
    let r = submit(
        &w,
        x,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "x filed"}}),
            ],
            vec![],
        ),
    )
    .await;
    let SubmitResult::Applied { effects, .. } = &r else {
        panic!("{r:?}")
    };
    let filed = effects[0].issue_id;
    let both = applied(
        &submit(
            &w,
            x,
            &doc(
                vec![
                    json!({"op_id": op(), "kind": "triage_decide", "target": tgt(filed),
                            "decision": "accept"}),
                    json!({"op_id": op(), "kind": "add_note", "target": tgt(filed),
                            "body": "accepted it"}),
                ],
                vec![pre(filed, lv(&w, filed).await)],
            ),
        )
        .await,
    );
    let item = |act: &undo::Activity| {
        act.items
            .iter()
            .find(|i| i.execution_id == Some(both))
            .cloned()
    };
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let before = ai_team::activity_all(&w.app, bob, Some(x_id), None, &ActivityQuery::default())
        .await
        .unwrap();
    assert!(item(&before).unwrap().self_triage);
    // The decision event is redacted (as redaction::redact does).
    let ev: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE command_execution_id = $1 AND kind = 'triage_changed'",
    )
    .bind(both)
    .fetch_one(&pool)
    .await
    .unwrap();
    let red: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_event_redactions (event_id, redacted_by, reason_code, reason)
         VALUES ($1, $2, 'test', 'test') RETURNING id",
    )
    .bind(ev)
    .bind(w.alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE issue_events SET payload = '\"[已遮蔽]\"', redaction_id = $2 WHERE id = $1",
    )
    .bind(ev)
    .bind(red)
    .execute(&pool)
    .await
    .unwrap();
    for who in [bob, a] {
        let after = ai_team::activity_all(&w.app, who, Some(x_id), None, &ActivityQuery::default())
            .await
            .unwrap();
        let it = item(&after).expect("listed: the note is readable");
        assert_eq!(it.keys, vec![key_of(&w, filed).await]);
        assert!(!it.self_triage, "the decision is not readable");
    }
    let own = undo::activity_page(&w.app, a, x_id, &ActivityQuery::default())
        .await
        .unwrap();
    assert!(!item(&own).unwrap().self_triage);
}

/// G43: names of group members, members of another project only, removed
/// members, and what a site administrator sees (verification probe v_g43).
#[tokio::test]
async fn g43_group_and_other_project_members() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(&pool, mgr, &["view_issues", "manage_project"]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{mgr}' FROM members
             WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.bob
        ),
    )
    .await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let dave = auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let erin = auth::create_user(&pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    // dave is in WEB only through a group; erin is a member of AUTH only.
    let g = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'Team') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO group_members VALUES ('{g}', '{dave}')"),
    )
    .await;
    member(&pool, w.web, g, w.dev).await;
    member(&pool, w.auth_p, erin, w.dev).await;
    let form = forms::ensure(
        &w.app,
        root,
        &NewForm {
            tracker_id: w.bug,
            project: Some("WEB".into()),
        },
    )
    .await
    .unwrap()
    .summary
    .id;
    let body = json!({"rules": [{"id": "r", "field": "due_date", "kind": "required_if",
                                 "when": {"all": [{"field": "assignee", "op": "is_set"}]}}]});
    let v1 = forms::put_draft(
        &w.app,
        bob,
        form,
        Part::Rules,
        &DraftInput {
            base_version: 0,
            body,
        },
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE form_rule_versions SET created_by = '{dave}', published_by = '{erin}', published_at = now()
             WHERE form_id = '{form}' AND version = {}",
            v1.version
        ),
    )
    .await;
    let vs = forms::versions(&w.app, bob, form, Part::Rules)
        .await
        .unwrap();
    let x = vs.iter().find(|x| x.version == v1.version).unwrap();
    assert_eq!(x.created_by_name.as_deref(), Some("dave"), "group member");
    assert_eq!(x.published_by_name, None, "a member of another project");
    assert!(!serde_json::to_string(&vs).unwrap().contains("erin"));
    // carol, a direct member, is removed from WEB and becomes the creator.
    exec(
        &pool,
        &format!(
            "UPDATE form_rule_versions SET created_by = '{}' WHERE form_id = '{form}' AND version = {};
             DELETE FROM member_roles WHERE member_id IN
                 (SELECT id FROM members WHERE project_id = '{}' AND principal_id = '{}');
             DELETE FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            w.carol, v1.version, w.web, w.carol, w.web, w.carol
        ),
    )
    .await;
    let vs = forms::versions(&w.app, bob, form, Part::Rules)
        .await
        .unwrap();
    let x = vs.iter().find(|x| x.version == v1.version).unwrap();
    assert_eq!(x.created_by_name, None, "removed from the project");
    let vs = forms::versions(&w.app, root, form, Part::Rules)
        .await
        .unwrap();
    let x = vs.iter().find(|x| x.version == v1.version).unwrap();
    assert_eq!(
        x.created_by_name.as_deref(),
        Some("carol"),
        "admin sees all"
    );
    assert_eq!(x.published_by_name.as_deref(), Some("erin"));
}

/// G46: the flags follow what a create would do (a tracker without a
/// default status; verification probe v_g46), the project status, group
/// membership, and a view whose project the viewer no longer sees.
#[tokio::test]
async fn g46_flags_follow_create_status_groups_and_visibility() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let r = router(&w);
    let flags = |body: &Value, key: &str| -> Option<Value> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == key)
            .map(|p| p["permissions"].clone())
    };
    // A project whose only tracker has no default status: create fails, so
    // the flag is false.
    let p = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OPS','Ops') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO enabled_modules VALUES ('{p}', 'issue_tracking')"),
    )
    .await;
    let t = id(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('NoDefault', 5) RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO project_trackers VALUES ('{p}', '{t}')"),
    )
    .await;
    member(&pool, p, w.alice, w.dev).await;
    let alice = login(&r, "alice").await;
    let (_, body) = get(&r, &alice, "/projects").await;
    let flag = flags(&body, "OPS").unwrap()["add_issues"].clone();
    let res = commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": p, "tracker_id": t,
                        "fields": {"subject": "x"}}),
            ],
            vec![],
        ),
    )
    .await;
    assert_eq!(flag, json!(false));
    assert!(res.is_err());
    // With a creatable tracker as well, it is true again.
    exec(
        &pool,
        &format!("INSERT INTO project_trackers VALUES ('{p}', '{}')", w.bug),
    )
    .await;
    let (_, body) = get(&r, &alice, "/projects").await;
    assert_eq!(flags(&body, "OPS").unwrap()["add_issues"], true);

    // Through a group: dave is in WEB only as a member of Team.
    let views_role = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('ViewMgr','all') RETURNING id",
    )
    .await;
    grant(&pool, views_role, &["view_issues", "manage_public_views"]).await;
    let dave = auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let g = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'Team') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO group_members VALUES ('{g}', '{dave}')"),
    )
    .await;
    let gm = member(&pool, w.web, g, w.dev).await;
    exec(
        &pool,
        &format!("INSERT INTO member_roles VALUES ('{gm}', '{views_role}')"),
    )
    .await;
    let d = login(&r, "dave").await;
    let (_, body) = get(&r, &d, "/projects").await;
    assert_eq!(
        flags(&body, "WEB"),
        Some(json!({"add_issues": true, "manage_public_views": true}))
    );
    assert_eq!(flags(&body, "AUTH"), None);
    let dave_ctx = sign_in(&pool, &w.policy, "dave").await;
    let board = format!("builtin:project_board:{}", w.web);
    let v = views::get(&w.app, dave_ctx, &board).await.unwrap();
    assert_eq!(
        v.permissions,
        Some(oxledger::perm::ProjectFlags {
            add_issues: true,
            manage_public_views: true
        })
    );

    // A view bound to a project its owner no longer sees: no flags.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let view = views::create(
        &w.app,
        bob,
        Uuid::now_v7(),
        &views::NewView {
            scope: "personal".into(),
            project_id: Some("AUTH".into()),
            kind: "list".into(),
            name: "mine".into(),
            def: json!({"kind": "list"}),
        },
    )
    .await
    .unwrap();
    assert!(view.permissions.is_some());
    exec(
        &pool,
        &format!(
            "DELETE FROM member_roles WHERE member_id IN
                 (SELECT id FROM members WHERE project_id = '{0}' AND principal_id = '{1}');
             DELETE FROM members WHERE project_id = '{0}' AND principal_id = '{1}'",
            w.auth_p, w.bob
        ),
    )
    .await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let v = views::get(&w.app, bob, &view.id).await.unwrap();
    assert!(v.project.is_none());
    assert_eq!(v.permissions, None, "{v:?}");

    // manage_public_views in a closed project: false (and no new issues).
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    let (_, body) = get(&r, &d, "/projects").await;
    assert_eq!(
        flags(&body, "WEB"),
        Some(json!({"add_issues": false, "manage_public_views": false}))
    );
    let v = views::get(&w.app, dave_ctx, &board).await.unwrap();
    assert_eq!(v.permissions, Some(oxledger::perm::ProjectFlags::default()));
}
