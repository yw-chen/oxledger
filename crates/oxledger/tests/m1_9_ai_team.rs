// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.9 (§3.12, §4.4, §7.9, §7.10a): the AI team — runtimes, role
//! principals sharing one work context, pause, the agent chain limit, the
//! AI team page and the compatibility endpoints.
//! T-M1-85…99 (84 is in m1_9_upgrade.rs), T-M1-66a, 121, 123, 124, 127,
//! 128, 133. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents;
use oxledger::agents::PolicyInput;
use oxledger::ai_team::{
    self, CreateRoleInput, CreatedRole, CredentialInput, NewRuntime, PauseInput, ResumeInput,
    RoleDelegation, RuntimeChoice, RuntimeNewContextInput,
};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::SessionPolicy;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput, SubmitResult};
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode::{self, Direct};
use oxledger::undo::{self, UndoInput};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

fn reason(e: &anyhow::Error) -> String {
    details(e)["reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn agent(w: &World, token: &str) -> AuthContext {
    auth::authenticate_bearer(&w.pool, token)
        .await
        .unwrap()
        .unwrap()
}

async fn new_client(w: &World, name: &str) -> Uuid {
    let id = agents::create_agent_client(&w.pool, name, "policy-2026-01")
        .await
        .unwrap();
    agents::set_agent_client_enabled(&w.pool, name, true)
        .await
        .unwrap();
    id
}

fn new_rt(label: &str) -> RuntimeChoice {
    RuntimeChoice {
        id: None,
        new: Some(NewRuntime {
            label: label.into(),
            isolation_ack: true,
        }),
    }
}

fn in_rt(id: Uuid) -> RuntimeChoice {
    RuntimeChoice {
        id: Some(id),
        new: None,
    }
}

fn role(
    runtime: RuntimeChoice,
    client: Uuid,
    key: &str,
    template: &str,
    projects: &[Uuid],
) -> CreateRoleInput {
    CreateRoleInput {
        runtime,
        client_id: client,
        role_key: Some(key.into()),
        template: template.into(),
        duties: format!("duties of {key}"),
        delegations: projects
            .iter()
            .map(|p| RoleDelegation {
                project_id: *p,
                expires_in_days: Some(30),
                max_ops: None,
                policies: None,
            })
            .collect(),
        read_own_time: false,
        read_own_time_ack: false,
        token_label: None,
        token_expires_in_days: None,
    }
}

fn with_commands(mut i: CreateRoleInput, c: &[(&str, PolicyMode)]) -> CreateRoleInput {
    for d in &mut i.delegations {
        d.policies = Some(PolicyInput {
            commands: c
                .iter()
                .map(|(k, m)| (k.to_string(), *m))
                .collect::<BTreeMap<_, _>>(),
            fields: BTreeMap::new(),
        });
    }
    i
}

async fn create(w: &World, who: AuthContext, i: &CreateRoleInput) -> anyhow::Result<CreatedRole> {
    ai_team::create(&w.app, &w.policy, who, Uuid::now_v7(), i).await
}

async fn submit(w: &World, a: AuthContext, doc: &Value) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, a, Uuid::now_v7(), doc, false, ORIGIN).await
}

async fn lv(w: &World, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn note(w: &World, a: AuthContext, issue: Uuid, body: &str) -> anyhow::Result<SubmitResult> {
    submit(w, a, &note_doc(issue, lv(w, issue).await, body)).await
}

fn applied(r: &SubmitResult) {
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
}

fn review(r: &SubmitResult) -> (String, Option<Value>, Uuid, String) {
    match r {
        SubmitResult::ReviewRequired { reason, proposal } => (
            reason.clone(),
            proposal.details.clone(),
            proposal.changeset_id,
            proposal.proposal_hash.clone(),
        ),
        other => panic!("expected review_required, got {other:?}"),
    }
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "agent", "ops": ops, "preconditions": pre})
}

fn public_note(i: Uuid, lv: i32) -> Value {
    note_doc(i, lv, "public")
}

fn public_field(i: Uuid, lv: i32) -> Value {
    subject_doc(i, lv, "renamed")
}

fn public_create(w: &World, project: Uuid) -> Value {
    doc(
        vec![
            json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                    "tracker_id": w.bug, "fields": {"subject": "new"}}),
        ],
        vec![],
    )
}

/// The 02 §16.6 breaker (three audience blocks an hour suspend the context)
/// would turn the next refusal into `agent_suspended`; these tests look at
/// each refusal on its own.
async fn unblock(w: &World) {
    exec(
        &w.pool,
        "DELETE FROM agent_write_log WHERE kind = 'audience_blocked'",
    )
    .await;
    exec(
        &w.pool,
        "UPDATE agent_context_write_controls SET suspended_at = NULL, suspension_reason = NULL",
    )
    .await;
}

async fn chain(w: &World, issue: Uuid) -> Option<(i32, i32)> {
    sqlx::query_as("SELECT writes, depth FROM issue_agent_chains WHERE issue_id = $1")
        .bind(issue)
        .fetch_optional(&w.pool)
        .await
        .unwrap()
}

/// alice's runtime R1 (client "Claude Code"): X = the 02 personal role,
/// delegated AUTH (narrow audience); Y = role `helper`, delegated WEB (wide).
struct Shared {
    runtime: Uuid,
    x: AuthContext,
    x_token: String,
    y: AuthContext,
    y_id: Uuid,
    x_id: Uuid,
}

async fn shared(w: &World) -> Shared {
    let dx = w.delegate(w.auth_p).await.unwrap();
    let (x_token, x) = w.token(dx.context_id).await;
    let y = create(
        w,
        w.alice_ctx,
        &role(
            in_rt(dx.runtime_id),
            w.client,
            "helper",
            "personal",
            &[w.web],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        y.delegations[0].context_id, dx.context_id,
        "one shared context"
    );
    Shared {
        runtime: dx.runtime_id,
        x,
        x_token,
        y: agent(w, y.token.as_deref().unwrap()).await,
        y_id: y.principal.id,
        x_id: dx.agent_principal_id,
    }
}

// ---------------------------------------------------------------- T-M1-85, 86, 87, 88, 66a

#[tokio::test]
async fn t_m1_85_88_shared_context() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let iw = w.issue(w.web, "web").await;
    let ia = w.issue(w.auth_p, "auth").await;
    let s = shared(&w).await;

    // T-M1-85: Y never had AUTH, but R is the runtime's: every public write
    // into WEB is refused.
    for d in [
        public_note(iw, lv(&w, iw).await),
        public_field(iw, lv(&w, iw).await),
        public_create(&w, w.web),
    ] {
        let e = submit(&w, s.y, &d).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden, "{e:#}");
        unblock(&w).await;
    }

    // T-M1-66a (representative E4w subset): the same three writes by one
    // agent holding both delegations (bob's own runtime) end the same way.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let mut both = role(
        new_rt("bob-laptop"),
        w.client,
        "solo",
        "personal",
        &[w.auth_p, w.web],
    );
    both.duties.clear();
    let solo = create(&w, bob, &both).await.unwrap();
    let solo = agent(&w, solo.token.as_deref().unwrap()).await;
    for d in [
        public_note(iw, lv(&w, iw).await),
        public_field(iw, lv(&w, iw).await),
        public_create(&w, w.web),
    ] {
        let one = submit(&w, solo, &d).await.unwrap_err();
        let two = submit(&w, s.y, &d).await.unwrap_err();
        unblock(&w).await;
        assert_eq!(code(&one), code(&two));
        assert_eq!(kind(&one), kind(&two));
    }
    // Verification D10: with evidence each agent can read (a WEB issue's
    // description), step 7 passes and the audience check (step 8) answers,
    // the same for both.
    exec(
        &pool,
        &format!("UPDATE issues SET description = '登入後被導回登入頁' WHERE id = '{iw}'"),
    )
    .await;
    let key: String =
        sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
            .bind(iw)
            .fetch_one(&pool)
            .await
            .unwrap();
    for a in [solo, s.y] {
        let g = oxledger::issue_view::get(
            &w.app,
            a,
            serde_json::from_value(
                json!({"key": key, "fields": ["description"], "citations": ["description"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let cit = serde_json::to_value(g).unwrap()["citations"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        let mut d = public_note(iw, lv(&w, iw).await);
        d["evidence"] = json!([{"citation_id": cit, "quote": "登入後被導回登入頁"}]);
        let e = submit(&w, a, &d).await.unwrap_err();
        unblock(&w).await;
        assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden, "{e:#}");
    }
    // X reading AUTH and writing AUTH stays allowed (narrow ⊆ narrow).
    applied(&note(&w, s.x, ia, "x in auth").await.unwrap());

    // T-M1-86: a role in another, isolated runtime does not share R.
    let codex = new_client(&w, "Codex").await;
    let other = create(
        &w,
        w.alice_ctx,
        &role(new_rt("second box"), codex, "w", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let other = agent(&w, other.token.as_deref().unwrap()).await;
    applied(&note(&w, other, iw, "isolated").await.unwrap());

    // T-M1-88: the token decides who acts; X reads only its own delegations.
    assert_eq!(s.y.principal_id(), Some(s.y_id));
    assert_eq!(s.x.principal_id(), Some(s.x_id));
    let mut conn = pool.acquire().await.unwrap();
    let xs: Vec<Uuid> = oxledger::db::load_scope(&mut conn, s.x)
        .await
        .unwrap()
        .project_ids()
        .collect();
    assert_eq!(xs, vec![w.auth_p], "X does not read Y's WEB");
    drop(conn);
    let d = agents::describe_agent_context(&w.app, s.x).await.unwrap();
    // Verification D7: WEB is in R through Y, counted but never named.
    assert_eq!(
        d.context
            .projects
            .iter()
            .map(|p| p.project_id)
            .collect::<Vec<_>>(),
        vec![w.auth_p]
    );
    assert_eq!(d.context.other_role_projects, 1);
    let text = serde_json::to_string(&d).unwrap();
    assert!(
        !text.contains("WEB") && !text.contains(&w.web.to_string()),
        "{text}"
    );
    assert_eq!(d.role.key, "personal");
    assert_eq!(d.runtime.shared_with, vec!["helper@alice".to_string()]);
    assert_eq!(d.limits.agent_chain.max_depth, 2);
    assert_eq!(d.limits.agent_chain.max_writes, 20);
    let e = submit(&w, s.x, &public_note(iw, lv(&w, iw).await))
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::NotFound,
        "X has no WEB delegation: {e:#}"
    );

    // T-M1-87: AUTH's epoch moves → X and Y stop together; the other runtime goes on.
    exec(
        &pool,
        &format!(
            "UPDATE projects SET security_epoch = security_epoch + 1 WHERE id = '{}'",
            w.auth_p
        ),
    )
    .await;
    for (a, other_handle) in [(s.x, "helper@alice"), (s.y, "claude-code@alice")] {
        let e = note(&w, a, iw, "after").await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::ContextUnavailable, "{e:#}");
        assert_eq!(kind(&e), "quarantined");
        assert_eq!(details(&e)["shared_with"], json!([other_handle]));
    }
    applied(&note(&w, other, iw, "still fine").await.unwrap());
    let _ = s.x_token;
    let _ = s.runtime;
}

// ---------------------------------------------------------------- T-M1-89

#[tokio::test]
async fn t_m1_89_runtime_new_context() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let ia = w.issue(w.auth_p, "auth").await;
    let s = shared(&w).await;
    // A pending proposal of X.
    let p = changesets::command_submit(
        &w.app,
        s.x,
        Uuid::now_v7(),
        &note_doc(ia, lv(&w, ia).await, "later"),
        true,
        ORIGIN,
    )
    .await
    .unwrap();
    let (_, _, cs, hash) = review(&p);

    // External runtimes need the declaration.
    let e = ai_team::runtime_new_context(
        &w.app,
        &w.policy,
        w.alice_ctx,
        s.runtime,
        &RuntimeNewContextInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "ack_required");
    let c = ai_team::runtime_new_context(
        &w.app,
        &w.policy,
        w.alice_ctx,
        s.runtime,
        &RuntimeNewContextInput {
            fresh_context_ack: true,
            carry_over_delegations: false,
        },
    )
    .await
    .unwrap();
    assert!(c.source_projects.is_empty(), "the new R is empty");
    assert_eq!(c.runtime_id, s.runtime);
    // Every role's delegations and tokens went with the old context.
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM delegations WHERE runtime_id = '{}' AND revoked_at IS NULL",
                s.runtime
            )
        )
        .await,
        0
    );
    for t in [&s.x_token] {
        let e = auth::authenticate_bearer(&pool, t).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    }
    let e = note(&w, s.y, ia, "y").await.unwrap_err();
    assert!(
        matches!(
            code(&e),
            ErrorCode::ContextUnavailable | ErrorCode::Unauthenticated
        ),
        "{e:#}"
    );
    // The pending proposal cannot run.
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: hash,
            review_mode: None,
        },
    )
    .await;
    let failed = match o {
        Err(_) => true,
        Ok(o) => o.execution.is_some_and(|e| e.status == "failed"),
    };
    assert!(failed, "a proposal of the closed context does not apply");
}

// ---------------------------------------------------------------- T-M1-90

#[tokio::test]
async fn t_m1_90_pause() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let iw = w.issue(w.web, "web").await;
    let ia = w.issue(w.auth_p, "auth").await;
    // bob manages AUTH (not WEB); root is a site admin.
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(&pool, mgr, &["view_issues", "manage_project"]).await;
    exec(&pool, &format!(
        "INSERT INTO member_roles SELECT m.id, '{mgr}' FROM members m WHERE m.project_id = '{}' AND m.principal_id = '{}'",
        w.auth_p, w.bob
    )).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let root = sign_in(&pool, &w.policy, "root").await;

    // Z: one role delegated AUTH and WEB in its own runtime (its R is both).
    let z = create(
        &w,
        w.alice_ctx,
        &role(new_rt("z box"), w.client, "z", "personal", &[w.auth_p]),
    )
    .await
    .unwrap();
    let zid = z.principal.id;
    let za = agent(&w, z.token.as_deref().unwrap()).await;
    // A proposal made before any pause.
    let p = changesets::command_submit(
        &w.app,
        za,
        Uuid::now_v7(),
        &note_doc(ia, lv(&w, ia).await, "queued"),
        true,
        ORIGIN,
    )
    .await
    .unwrap();
    let (_, _, cs, hash) = review(&p);

    // The sponsor pauses the whole role: writes stop at once.
    ai_team::pause(
        &w.app,
        w.alice_ctx,
        zid,
        &PauseInput {
            scope: "principal".into(),
            project_id: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    let e = note(&w, za, ia, "paused").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "agent_paused");
    assert_eq!(details(&e)["scope"], "principal");
    // V25: approving the earlier proposal now does nothing.
    let notes_before = count(
        &pool,
        &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{ia}'"),
    )
    .await;
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: hash.clone(),
            review_mode: None,
        },
    )
    .await;
    match o {
        Err(e) => assert_eq!(reason(&e), "agent_paused", "{e:#}"),
        Ok(o) => {
            let ex = o.execution.unwrap();
            assert_eq!(ex.status, "failed");
            assert_eq!(ex.error.unwrap().code, "forbidden_op");
        }
    }
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{ia}'")
        )
        .await,
        notes_before
    );
    // A project manager cannot pause the whole role, nor resume the sponsor's pause.
    let e = ai_team::pause(
        &w.app,
        bob,
        zid,
        &PauseInput {
            scope: "principal".into(),
            project_id: None,
            reason: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    ai_team::resume(
        &w.app,
        &w.policy,
        w.alice_ctx,
        zid,
        &ResumeInput {
            scope: "principal".into(),
            project_id: None,
        },
    )
    .await
    .unwrap();
    applied(&note(&w, za, ia, "resumed").await.unwrap());

    // The AUTH manager pauses Z in AUTH: a reason is required.
    let e = ai_team::pause(
        &w.app,
        bob,
        zid,
        &PauseInput {
            scope: "project".into(),
            project_id: Some(w.auth_p),
            reason: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    ai_team::pause(
        &w.app,
        bob,
        zid,
        &PauseInput {
            scope: "project".into(),
            project_id: Some(w.auth_p),
            reason: Some("too noisy".into()),
        },
    )
    .await
    .unwrap();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT paused_reason FROM delegations WHERE agent_principal_id = $1 AND project_id = $2",
    )
    .bind(zid)
    .bind(w.auth_p)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some("too noisy"));
    let e = note(&w, za, ia, "paused here").await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused");
    assert_eq!(details(&e)["scope"], "project");
    // bob does not manage WEB: no pause there (Z has a WEB delegation too).
    agents::create_delegation_for(
        &pool,
        &w.policy,
        &w.alice_ctx,
        zid,
        agents::DelegationInput {
            agent_client_id: Uuid::nil(),
            project_id: w.web,
            proposal_ops: vec!["add_note".into()],
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    let e = ai_team::pause(
        &w.app,
        bob,
        zid,
        &PauseInput {
            scope: "project".into(),
            project_id: Some(w.web),
            reason: Some("x".into()),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    // The manager resumes his own layer.
    ai_team::resume(
        &w.app,
        &w.policy,
        bob,
        zid,
        &ResumeInput {
            scope: "project".into(),
            project_id: Some(w.auth_p),
        },
    )
    .await
    .unwrap();
    applied(&note(&w, za, ia, "back").await.unwrap());

    // A site admin's pause is lifted only by a site admin.
    ai_team::pause(
        &w.app,
        root,
        zid,
        &PauseInput {
            scope: "principal".into(),
            project_id: None,
            reason: Some("incident".into()),
        },
    )
    .await
    .unwrap();
    let e = ai_team::resume(
        &w.app,
        &w.policy,
        w.alice_ctx,
        zid,
        &ResumeInput {
            scope: "principal".into(),
            project_id: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = ai_team::resume(
        &w.app,
        &w.policy,
        bob,
        zid,
        &ResumeInput {
            scope: "principal".into(),
            project_id: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    ai_team::resume(
        &w.app,
        &w.policy,
        root,
        zid,
        &ResumeInput {
            scope: "principal".into(),
            project_id: None,
        },
    )
    .await
    .unwrap();
    applied(&note(&w, za, ia, "admin resumed").await.unwrap());
    let _ = iw;
}

// ---------------------------------------------------------------- T-M1-91, 92

#[tokio::test]
async fn t_m1_91_92_team_page() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let ia = w.issue(w.auth_p, "auth").await;
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(&pool, mgr, &["view_issues", "manage_project"]).await;
    exec(&pool, &format!(
        "INSERT INTO member_roles SELECT m.id, '{mgr}' FROM members m WHERE m.project_id = '{}' AND m.principal_id = '{}'",
        w.auth_p, w.bob
    )).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let root = sign_in(&pool, &w.policy, "root").await;
    let dave = sign_in(&pool, &w.policy, "dave").await;

    // X: AUTH only. Z: AUTH and WEB (the research 04 case: one bot, two projects).
    let x = w.delegate(w.auth_p).await.unwrap();
    let z = create(
        &w,
        w.alice_ctx,
        &role(
            in_rt(x.runtime_id),
            w.client,
            "z",
            "triage",
            &[w.auth_p, w.web],
        ),
    )
    .await
    .unwrap();
    let zid = z.principal.id;
    let za = agent(&w, z.token.as_deref().unwrap()).await;

    // T-M1-92: five writes on AUTH, which carol cannot read.
    for k in 0..5 {
        applied(&note(&w, za, ia, &format!("n{k}")).await.unwrap());
    }

    let handles =
        |l: &ai_team::TeamList| l.items.iter().map(|m| m.handle.clone()).collect::<Vec<_>>();
    // The sponsor sees both, with duties and every write.
    let l = ai_team::list(&w.app, w.alice_ctx, None, None)
        .await
        .unwrap();
    assert_eq!(handles(&l), vec!["claude-code@alice", "z@alice"]);
    let zm = l.items.iter().find(|m| m.id == zid).unwrap();
    assert_eq!(zm.duties.as_deref(), Some("duties of z"));
    assert_eq!(zm.stats.writes_today, 5);
    assert_eq!(zm.stats.undo_rate_7d.total, 5);
    assert!(zm.stats.blocked_7d.is_some());
    // carol (WEB only) sees Z, not X; no duties; none of the AUTH writes.
    let l = ai_team::list(&w.app, carol, None, None).await.unwrap();
    assert_eq!(handles(&l), vec!["z@alice"]);
    let zm = &l.items[0];
    assert!(zm.duties.is_none());
    assert_eq!(zm.stats.writes_today, 0);
    assert_eq!(zm.stats.undo_rate_7d.total, 0);
    assert!(zm.stats.blocked_7d.is_none());
    let text = serde_json::to_string(&l).unwrap();
    assert!(!text.contains("duties"), "{text}");
    // An invisible role answers like a missing one.
    let e1 = ai_team::detail(&w.app, carol, x.agent_principal_id)
        .await
        .unwrap_err();
    let e2 = ai_team::detail(&w.app, carol, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&e1), ErrorCode::NotFound);
    assert_eq!(format!("{e1}"), format!("{e2}"));
    assert_eq!(details(&e1), details(&e2));
    // dave sees nothing at all.
    assert!(
        ai_team::list(&w.app, dave, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // ?project= lists only roles delegated there (a hidden project lists nothing).
    let l = ai_team::list(&w.app, carol, Some(w.auth_p), None)
        .await
        .unwrap();
    assert!(l.items.is_empty());
    let l = ai_team::list(&w.app, w.alice_ctx, None, Some("z@alice"))
        .await
        .unwrap();
    assert_eq!(handles(&l), vec!["z@alice"]);

    // Details: carol gets the safety summary for WEB only, no policies, no runtime.
    let d = ai_team::detail(&w.app, carol, zid).await.unwrap();
    assert_eq!(
        d.safety_summary
            .iter()
            .map(|s| s.project_id)
            .collect::<Vec<_>>(),
        vec![w.web]
    );
    assert!(d.policies.is_empty());
    assert!(d.runtime.is_none());
    assert!(!serde_json::to_string(&d).unwrap().contains("duties"));
    // bob manages AUTH only: the AUTH policy, both summaries, no duties.
    let d = ai_team::detail(&w.app, bob, zid).await.unwrap();
    assert_eq!(
        d.policies.iter().map(|p| p.project_id).collect::<Vec<_>>(),
        vec![w.auth_p]
    );
    assert_eq!(d.safety_summary.len(), 2);
    assert!(d.member.duties.is_none());
    assert!(d.runtime.is_none());
    // The site admin and the sponsor get everything.
    for who in [root, w.alice_ctx] {
        let d = ai_team::detail(&w.app, who, zid).await.unwrap();
        assert_eq!(d.policies.len(), 2);
        assert_eq!(d.member.duties.as_deref(), Some("duties of z"));
        let rt = d.runtime.unwrap();
        assert_eq!(rt.shared_with, vec!["claude-code@alice".to_string()]);
        assert_eq!(rt.sources, vec!["AUTH".to_string(), "WEB".to_string()]);
    }
    // Activity: carol sees none of the AUTH writes; alice sees them.
    let a = ai_team::activity(&w.app, carol, zid, None, None)
        .await
        .unwrap();
    assert!(a.items.is_empty());
    let a = ai_team::activity(&w.app, w.alice_ctx, zid, None, None)
        .await
        .unwrap();
    assert_eq!(a.items.len(), 5);
}

// ---------------------------------------------------------------- T-M1-93, 95, 98, 123

/// bob's runtime with role `a` (personal: all direct) and role `b` (custom:
/// only add_note direct), both delegated WEB.
async fn pair(w: &World) -> (AuthContext, AuthContext, AuthContext) {
    let bob = sign_in(&w.pool, &w.policy, "bob").await;
    let a = create(
        w,
        bob,
        &role(new_rt("pair"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let b = create(
        w,
        bob,
        &with_commands(
            role(in_rt(a.runtime.id), w.client, "b", "custom", &[w.web]),
            &[("add_note", Direct)],
        ),
    )
    .await
    .unwrap();
    (
        bob,
        agent(w, a.token.as_deref().unwrap()).await,
        agent(w, b.token.as_deref().unwrap()).await,
    )
}

#[tokio::test]
async fn t_m1_93_95_98_123_chain() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "chain").await;
    let (bob, a, b) = pair(&w).await;

    // A → B → A: depth 2, allowed.
    applied(&note(&w, a, i, "a1").await.unwrap());
    assert_eq!(chain(&w, i).await, Some((1, 0)));
    applied(&note(&w, b, i, "b1").await.unwrap());
    applied(&note(&w, a, i, "a2").await.unwrap());
    assert_eq!(chain(&w, i).await, Some((3, 2)));

    // T-M1-98: validate says it would go to review.
    let v = changesets::validate_command(
        &w.app,
        b,
        &oxledger::forms::ValidateCommandInput {
            command: note_doc(i, lv(&w, i).await, "b2?"),
            expected_rules_version: None,
        },
    )
    .await
    .unwrap();
    assert!(v.would_require_review);

    // T-M1-123: an `off` op is still refused, not sent for review.
    let e = submit(&w, b, &subject_doc(i, lv(&w, i).await, "b renames"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");

    // T-M1-93: the fourth write (depth 3) goes to review, with no counts or names.
    let notes = count(
        &pool,
        &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i}'"),
    )
    .await;
    let lock = lv(&w, i).await;
    let r = note(&w, b, i, "b2").await.unwrap();
    let (why, det, cs, hash) = review(&r);
    assert_eq!(why, "agent_chain");
    assert_eq!(
        det.unwrap(),
        json!({"exceeded": "depth", "max_depth": 2, "max_writes": 20})
    );
    let text = serde_json::to_string(&r).unwrap();
    assert!(
        !text.contains("a@bob") && !text.contains("\"writes\":"),
        "{text}"
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i}'")
        )
        .await,
        notes
    );
    assert_eq!(lv(&w, i).await, lock);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM agent_write_log WHERE kind = 'chain_review'"
        )
        .await,
        1
    );
    assert_eq!(chain(&w, i).await, Some((3, 2)), "a review is not a write");
    // The queue shows why, and it cannot be ticked.
    let l = changesets::list(&w.app, bob, None, None, None, None)
        .await
        .unwrap();
    let row = l.items.iter().find(|c| c.changeset_id == cs).unwrap();
    assert_eq!(row.agent_chain.as_deref(), Some("depth"));
    assert_eq!(row.review_modes, vec!["detail".to_string()]);

    // T-M1-95: approving it is a human intervention: the chain is gone and
    // the apply itself does not count; its events stay the agent's.
    let o = changesets::approve_and_apply(
        &w.app,
        bob,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: hash,
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(o.execution.unwrap().status, "succeeded");
    assert_eq!(chain(&w, i).await, None);
    let origin: String = sqlx::query_scalar(
        "SELECT origin FROM issue_events WHERE issue_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(origin, "external_agent");
    applied(&note(&w, b, i, "fresh").await.unwrap());
    assert_eq!(chain(&w, i).await, Some((1, 0)), "a new chain");

    // A person's note resets it.
    let human = oxledger::commands::submit(
        &w.app,
        bob,
        Uuid::now_v7(),
        &note_doc(i, lv(&w, i).await, "human"),
    )
    .await
    .unwrap();
    let _ = human;
    assert_eq!(chain(&w, i).await, None);
    // A person's undo resets it too.
    let r = note(&w, a, i, "to undo").await.unwrap();
    let SubmitResult::Applied { execution_id, .. } = r else {
        panic!()
    };
    assert_eq!(chain(&w, i).await, Some((1, 0)));
    undo::apply(
        &w.app,
        bob,
        Uuid::now_v7(),
        execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(chain(&w, i).await, None);
}

// ---------------------------------------------------------------- T-M1-94

#[tokio::test]
async fn t_m1_94_chain_writes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "many").await;
    let (_, a, _) = pair(&w).await;
    for k in 1..=20 {
        applied(&note(&w, a, i, &format!("w{k}")).await.unwrap());
        // Keep the 02 §16.6 per-issue and per-minute breakers out of the way
        // (the same as raising their thresholds for this test).
        exec(&pool, "DELETE FROM agent_write_log").await;
    }
    assert_eq!(chain(&w, i).await, Some((20, 0)));
    let (why, det, _, _) = review(&note(&w, a, i, "w21").await.unwrap());
    assert_eq!(why, "agent_chain");
    assert_eq!(det.unwrap()["exceeded"], "writes");
}

// ---------------------------------------------------------------- T-M1-96, 97

#[tokio::test]
async fn t_m1_96_97_chain_reads_and_concurrency() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "race").await;
    let (_, a, b) = pair(&w).await;
    // T-M1-96: reads and describe do not touch the chain.
    agents::describe_agent_context(&w.app, a).await.unwrap();
    changesets::validate_command(
        &w.app,
        a,
        &oxledger::forms::ValidateCommandInput {
            command: note_doc(i, lv(&w, i).await, "v"),
            expected_rules_version: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(chain(&w, i).await, None);

    // T-M1-97: two agents at once on one issue: serialized, counted right.
    let l = lv(&w, i).await;
    let (da, db) = (note_doc(i, l, "a"), note_doc(i, l, "b"));
    let (ra, rb) = common::wait_for("two racing submits", common::WAIT_TIMEOUT, async {
        tokio::join!(submit(&w, a, &da), submit(&w, b, &db))
    })
    .await;
    let ok = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Ok(SubmitResult::Applied { .. })))
        .count() as i32;
    assert!(ok >= 1, "{ra:?} {rb:?}");
    for r in [&ra, &rb] {
        if let Err(e) = r {
            assert_ne!(
                code(e),
                ErrorCode::TemporarilyUnavailable,
                "no lock timeout: {e:#}"
            );
        }
    }
    let (writes, _) = chain(&w, i).await.unwrap();
    assert_eq!(writes, ok);
}

// ---------------------------------------------------------------- T-M1-99

#[tokio::test]
async fn t_m1_99_templates() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "triage me").await;
    let t = create(
        &w,
        w.alice_ctx,
        &role(new_rt("triage box"), w.client, "triage", "triage", &[w.web]),
    )
    .await
    .unwrap();
    let ta = agent(&w, t.token.as_deref().unwrap()).await;
    let e = submit(&w, ta, &public_create(&w, w.web)).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    let d = json!({"command_schema_version": 1, "title": "t",
                   "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"description": "x"}}],
                   "preconditions": [{"target": tgt(i), "lock_version": lv(&w, i).await}]});
    let e = submit(&w, ta, &d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    applied(&note(&w, ta, i, "triage note").await.unwrap());

    // The stored policy is the template's full allow-list, copied: a later
    // template version would not change it (only `template_version` says so).
    let spec = ai_team::template_spec(&mut pool.acquire().await.unwrap(), "triage")
        .await
        .unwrap();
    let p = agents::delegation_policies(&pool, &w.alice_ctx, t.delegations[0].id)
        .await
        .unwrap();
    for (op, mode) in &p.commands {
        assert_eq!(spec.policies.commands.get(op), Some(mode), "{op}");
    }
    assert_eq!(p.commands["create_issue"], PolicyMode::Off);
    assert_eq!(p.fields["description"], PolicyMode::Off);
    assert_eq!(p.fields["priority_id"], Direct);
    assert_eq!(p.fields["assigned_to_id"], PolicyMode::Review);
    let d = ai_team::detail(&w.app, w.alice_ctx, t.principal.id)
        .await
        .unwrap();
    let u = d.template_update.unwrap();
    assert_eq!(
        (u.current_version, u.outdated),
        (ai_team::TEMPLATE_VERSION, false)
    );
    assert_eq!(d.member.role.template, "triage");
    // An unknown template and the built-in AI's own are refused.
    for bad in ["builtin_chat", "nope"] {
        let e = create(
            &w,
            w.alice_ctx,
            &role(in_rt(t.runtime.id), w.client, "x", bad, &[w.web]),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue);
    }
}

// ---------------------------------------------------------------- T-M1-121

/// T1b regression (T-M1-121 under contention): two creates of the same new
/// client's role in two new runtimes, held at the start of their
/// transactions and released together, 20 times. One wins and the other
/// gets `runtime_client_taken`, never a deadlock (the sponsor lock used to
/// come after the operation request's FOR KEY SHARE on the sponsor row).
#[tokio::test]
async fn t1b_concurrent_creates_never_deadlock() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    faults::clear();
    for n in 0..20 {
        let client = new_client(&w, &format!("Race {n}")).await;
        let a = role(new_rt(&format!("RA{n}")), client, "c", "personal", &[w.web]);
        let b = role(new_rt(&format!("RB{n}")), client, "c", "personal", &[w.web]);
        let (ga, gb) = (Arc::new(Gate::default()), Arc::new(Gate::default()));
        faults::arm("ai_team.create.begun", Action::Block(ga.clone()));
        faults::arm("ai_team.create.begun", Action::Block(gb.clone()));
        // The barrier: both requests are inside their transactions before
        // either goes on.
        let barrier = async {
            ga.reached.notified().await;
            gb.reached.notified().await;
            ga.release.notify_one();
            gb.release.notify_one();
        };
        let (x, y, ()) = common::wait_for("two racing creates", common::WAIT_TIMEOUT, async {
            tokio::join!(
                create(&w, w.alice_ctx, &a),
                create(&w, w.alice_ctx, &b),
                barrier
            )
        })
        .await;
        let (won, lost) = match (x, y) {
            (Ok(r), Err(e)) | (Err(e), Ok(r)) => (r, e),
            (x, y) => panic!(
                "round {n}: exactly one create wins: {:?} / {:?}",
                x.map(|r| r.runtime.id),
                y.map(|r| r.runtime.id)
            ),
        };
        assert_eq!(
            classify(&lost).map(|a| a.code),
            Some(ErrorCode::RuntimeClientTaken),
            "round {n}: {lost:#}"
        );
        // Free the role slot for the next round.
        ai_team::retire_runtime(&w.app, &w.policy, w.alice_ctx, won.runtime.id)
            .await
            .unwrap();
    }
    faults::clear();
}

#[tokio::test]
async fn t_m1_121_one_runtime_per_client() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r1 = create(
        &w,
        w.alice_ctx,
        &role(new_rt("R1"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a_token = r1.token.clone().unwrap();
    let e = create(
        &w,
        w.alice_ctx,
        &role(new_rt("R2"), w.client, "b", "personal", &[w.web]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RuntimeClientTaken);
    assert_eq!(code(&e).http_status(), 409);
    assert_eq!(details(&e)["runtime_id"], json!(r1.runtime.id));
    assert_eq!(details(&e)["label"], "R1");

    // Two new runtimes for one new client at the same time: one wins.
    let cursor = new_client(&w, "Cursor").await;
    let i1 = role(new_rt("C1"), cursor, "c", "personal", &[w.web]);
    let i2 = role(new_rt("C2"), cursor, "c", "personal", &[w.web]);
    let (x, y) = common::wait_for("two creates for one client", common::WAIT_TIMEOUT, async {
        tokio::join!(create(&w, w.alice_ctx, &i1), create(&w, w.alice_ctx, &i2))
    })
    .await;
    assert_eq!([x.is_ok(), y.is_ok()].iter().filter(|o| **o).count(), 1);
    let loser = x.err().or(y.err()).unwrap();
    assert_eq!(code(&loser), ErrorCode::RuntimeClientTaken, "{loser:#}");

    // Retire R1: its delegations and tokens go, and the client is free again.
    ai_team::retire_runtime(&w.app, &w.policy, w.alice_ctx, r1.runtime.id)
        .await
        .unwrap();
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM delegations WHERE runtime_id = '{}' AND revoked_at IS NULL",
                r1.runtime.id
            )
        )
        .await,
        0
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM agent_credentials WHERE runtime_id = '{}' AND revoked_at IS NULL", r1.runtime.id)).await,
        0
    );
    assert!(
        auth::authenticate_bearer(&pool, &a_token)
            .await
            .map(|o| o.is_none())
            .unwrap_or(true)
    );
    create(
        &w,
        w.alice_ctx,
        &role(new_rt("R3"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------- T-M1-124

fn quoted(def: &str) -> std::collections::BTreeSet<String> {
    def.split('\'')
        .enumerate()
        .filter(|(n, _)| n % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect()
}

#[tokio::test]
async fn t_m1_124_check_lists_match_code() {
    let pool = test_pool().await;
    let def = |name: &'static str| {
        let pool = pool.clone();
        async move {
            let d: String = sqlx::query_scalar(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap();
            quoted(&d)
        }
    };
    let set = |v: &[&str]| {
        v.iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        def("agent_write_log_kind_check").await,
        set(&oxledger::quota::WRITE_LOG_KINDS)
    );
    assert_eq!(
        def("delegation_command_policies_command_kind_check").await,
        set(&agents::PROPOSAL_OPS)
    );
    assert_eq!(
        def("delegations_proposal_ops_check").await,
        set(&agents::PROPOSAL_OPS)
    );
}

// ---------------------------------------------------------------- T-M1-127

#[tokio::test]
async fn t_m1_127_builtin() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    // A built-in client, runtime and principal as M1.10 will create them.
    let bc = id(&pool, "INSERT INTO agent_clients (name, egress_policy_ref, enabled, builtin) VALUES ('oxledger-ai', 'builtin', true, true) RETURNING id").await;
    let rt = id(&pool, &format!("INSERT INTO agent_runtimes (sponsor_user_id, kind, label, isolation_ack) VALUES ('{}', 'builtin', 'oxledger-ai', true) RETURNING id", w.alice)).await;
    let bp = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('agent', 'alice 的 oxledger-ai') RETURNING id",
    )
    .await;
    exec(&pool, &format!(
        "INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id, handle, runtime_id, role_key, role_template, template_version)
         VALUES ('{bp}', '{}', '{bc}', 'oxledger-ai@alice', '{rt}', 'oxledger_ai', 'builtin_chat', 1)",
        w.alice
    )).await;
    let session = id(
        &pool,
        &format!(
            "SELECT id FROM browser_sessions WHERE user_id = '{}' ORDER BY created_at DESC LIMIT 1",
            w.alice
        ),
    )
    .await;
    let ctx = id(&pool, &format!(
        "INSERT INTO agent_work_contexts (created_for_principal_id, runtime_id, fresh_context_ack, created_by_session) VALUES ('{bp}', '{rt}', true, '{session}') RETURNING id"
    )).await;
    exec(&pool, &format!(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id, security_epoch, proposal_ops, writable_fields, max_ops, expires_at)
         SELECT '{bp}', '{}', '{ctx}', id, security_epoch, ARRAY['add_note'], ARRAY[]::text[], 10, now() + interval '1 day' FROM projects WHERE id = '{}'",
        w.alice, w.web
    )).await;
    // No token for the built-in AI, through the API…
    let e = ai_team::issue_credential(
        &w.app,
        &w.policy,
        w.alice_ctx,
        bp,
        &CredentialInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "builtin_no_token");
    let e = agents::issue_credential_for(&pool, &w.policy, &w.alice_ctx, ctx, None, "t", None)
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "builtin_no_token");
    // M1.10 (design 05 §3.12.5): the built-in AI is on the AI team page
    // under the same visibility rules, with pause and resume only (M1.9's
    // verification D8 kept it off until M1.10).
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    for who in [w.alice_ctx, root] {
        let l = ai_team::list(&w.app, who, None, None).await.unwrap();
        assert!(l.items.iter().any(|m| m.id == bp));
        let d = ai_team::detail(&w.app, who, bp).await.unwrap();
        assert!(
            !d.actions
                .iter()
                .any(|a| a == "retire" || a == "credentials")
        );
    }
    let e = ai_team::retire(&w.app, &w.policy, w.alice_ctx, bp)
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "builtin_runtime");
    // …or behind its back (the composite FK on the runtime kind).
    let r = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO agent_credentials (context_id, token_sha256, label, expires_at, agent_principal_id, runtime_id)
         VALUES ('{ctx}', decode(repeat('ab', 32), 'hex'), 'x', now() + interval '1 day', '{bp}', '{rt}')"
    )))
    .execute(&pool)
    .await;
    assert!(
        r.is_err(),
        "a token row for the built-in runtime is refused"
    );
    // The built-in client cannot be delegated from outside.
    let e = create(
        &w,
        w.alice_ctx,
        &role(new_rt("fake"), bc, "x", "personal", &[w.web]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "builtin_client");
    let e = agents::create_delegation(
        &pool,
        &w.policy,
        &w.alice_ctx,
        agents::DelegationInput {
            agent_client_id: bc,
            project_id: w.web,
            proposal_ops: vec!["add_note".into()],
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "builtin_client");
    // An external client whose name slugs to oxledger-ai gets another handle,
    // and the database refuses the reserved one outright.
    let look = new_client(&w, "Oxledger AI").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("look-alike"), look, "personal", "personal", &[w.web]),
    )
    .await
    .unwrap();
    assert_eq!(r.principal.handle, "ext-oxledger-ai@alice");
    let r2 = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "UPDATE agent_principals SET handle = 'oxledger-ai@alice-2' WHERE agent_principal_id = '{}'",
        r.principal.id
    )))
    .execute(&pool)
    .await;
    assert!(r2.is_err());
}

// ---------------------------------------------------------------- T-M1-128

#[tokio::test]
async fn t_m1_128_delegations_by_principal_and_alias() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    // The 02 path: a single-role runtime; the alias issues tokens as before.
    let d = w.delegate(w.web).await.unwrap();
    let c = agents::issue_credential(&pool, &w.policy, &w.alice_ctx, d.context_id, "t", None)
        .await
        .unwrap();
    assert_eq!(c.agent_principal_id, d.agent_principal_id);
    // A second role of the same client in the same runtime.
    let r = create(
        &w,
        w.alice_ctx,
        &role(in_rt(d.runtime_id), w.client, "second", "custom", &[w.web]),
    )
    .await
    .unwrap();
    // POST /delegations with agent_principal_id lands on that role.
    let v = agents::create_delegation_for(
        &pool,
        &w.policy,
        &w.alice_ctx,
        r.principal.id,
        agents::DelegationInput {
            agent_client_id: Uuid::nil(),
            project_id: w.auth_p,
            proposal_ops: vec!["add_note".into()],
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(v.agent_principal_id, r.principal.id);
    assert_eq!(v.role_key, "second");
    assert_eq!(v.context_id, d.context_id);
    // The alias now needs agent_principal_id.
    let e = agents::issue_credential(&pool, &w.policy, &w.alice_ctx, d.context_id, "t", None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    assert_eq!(reason(&e), "principal_required");
    let c = agents::issue_credential_for(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.context_id,
        Some(r.principal.id),
        "t",
        None,
    )
    .await
    .unwrap();
    let a = agent(&w, &c.token).await;
    assert_eq!(a.principal_id(), Some(r.principal.id));
    // Someone else's role is not found.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = agents::create_delegation_for(
        &pool,
        &w.policy,
        &bob,
        r.principal.id,
        agents::DelegationInput {
            agent_client_id: Uuid::nil(),
            project_id: w.web,
            proposal_ops: vec!["add_note".into()],
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // A replay of POST /ai-team gives the same ids and no token.
    let key = Uuid::now_v7();
    let i = role(in_rt(d.runtime_id), w.client, "third", "custom", &[w.web]);
    let one = ai_team::create(&w.app, &w.policy, w.alice_ctx, key, &i)
        .await
        .unwrap();
    let two = ai_team::create(&w.app, &w.policy, w.alice_ctx, key, &i)
        .await
        .unwrap();
    assert!(one.token.is_some());
    assert!(two.token.is_none());
    assert_eq!(one.principal.id, two.principal.id);
    assert_eq!(one.credential_id, two.credential_id);
    assert_eq!(
        one.delegations.iter().map(|d| d.id).collect::<Vec<_>>(),
        two.delegations.iter().map(|d| d.id).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------- T-M1-133

#[tokio::test]
async fn t_m1_133_time_readers() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let mut x = role(new_rt("mine"), w.client, "x", "personal", &[w.web]);
    x.read_own_time = true;
    x.read_own_time_ack = true;
    let x = create(&w, w.alice_ctx, &x).await.unwrap();
    assert!(x.warnings.is_empty(), "a new runtime shares nothing");
    let y = create(
        &w,
        w.alice_ctx,
        &role(
            in_rt(x.runtime.id),
            w.client,
            "y",
            "design_review",
            &[w.web],
        ),
    )
    .await
    .unwrap();
    assert_eq!(y.warnings.len(), 1);
    let warn = &y.warnings[0];
    assert_eq!(warn.kind, "shared_runtime");
    assert_eq!(warn.roles, vec!["x@alice".to_string()]);
    assert_eq!(warn.sources, vec!["WEB".to_string()]);
    assert_eq!(warn.time_readers, vec!["x@alice".to_string()]);
    let d = ai_team::detail(&w.app, w.alice_ctx, y.principal.id)
        .await
        .unwrap();
    assert_eq!(d.runtime.unwrap().time_readers, vec!["x@alice".to_string()]);
    let rts = ai_team::runtimes(&w.app, w.alice_ctx).await.unwrap();
    assert_eq!(rts.len(), 1);
    assert_eq!(rts[0].time_readers, vec!["x@alice".to_string()]);
    assert_eq!(rts[0].roles.len(), 2);
    // Rename with lock_version.
    let r = ai_team::rename_runtime(
        &w.app,
        w.alice_ctx,
        x.runtime.id,
        &ai_team::RenameRuntimeInput {
            label: "laptop".into(),
            lock_version: rts[0].lock_version,
        },
    )
    .await
    .unwrap();
    assert_eq!(r.label, "laptop");
    let e = ai_team::rename_runtime(
        &w.app,
        w.alice_ctx,
        x.runtime.id,
        &ai_team::RenameRuntimeInput {
            label: "again".into(),
            lock_version: rts[0].lock_version,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // Someone else's runtime does not exist for bob.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = ai_team::rename_runtime(
        &w.app,
        bob,
        x.runtime.id,
        &ai_team::RenameRuntimeInput {
            label: "mine".into(),
            lock_version: r.lock_version,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // Retiring a role revokes its tokens; the other role goes on.
    ai_team::retire(&w.app, &w.policy, w.alice_ctx, x.principal.id)
        .await
        .unwrap();
    assert!(
        auth::authenticate_bearer(&pool, x.token.as_deref().unwrap())
            .await
            .map(|o| o.is_none())
            .unwrap_or(true)
    );
    let ya = agent(&w, y.token.as_deref().unwrap()).await;
    agents::describe_agent_context(&w.app, ya).await.unwrap();
}

// ================================================================ independent verification (M1.9)
// The tests below come from the independent verification (v_*) or were
// added for its findings (D1–D10, M14).

/// dave: a WEB member who reads issues but not private notes.
async fn dave_viewer(w: &World) -> AuthContext {
    let viewer = id(
        &w.pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','all') RETURNING id",
    )
    .await;
    grant(&w.pool, viewer, &["view_issues"]).await;
    let dave = auth::create_user(&w.pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(&w.pool, w.web, dave, viewer).await;
    sign_in(&w.pool, &w.policy, "dave").await
}

/// D1 / U35: a write that only added a private note neither shows nor
/// counts for a member who cannot read private notes.
#[tokio::test]
async fn v_activity_hides_private_note_writes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dave = dave_viewer(&w).await;
    let i = w.issue(w.web, "public issue").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "noter", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a = agent(&w, r.token.as_deref().unwrap()).await;
    let d = json!({"command_schema_version": 1, "title": "private",
                   "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i),
                            "body": "secret", "is_private": true}],
                   "preconditions": [{"target": tgt(i), "lock_version": lv(&w, i).await}]});
    applied(&submit(&w, a, &d).await.unwrap());
    let l = ai_team::list(&w.app, dave, None, None).await.unwrap();
    assert_eq!(l.items.len(), 1);
    let act = ai_team::activity(&w.app, dave, r.principal.id, None, None)
        .await
        .unwrap();
    assert!(
        act.items.is_empty(),
        "private note write leaks to dave: {:?}",
        serde_json::to_value(&act).unwrap()
    );
    assert_eq!(l.items[0].stats.writes_today, 0, "counted for dave");
    assert_eq!(l.items[0].stats.undo_rate_7d.total, 0);
    // The sponsor (who reads private notes in WEB) still sees and counts it.
    let act = ai_team::activity(&w.app, w.alice_ctx, r.principal.id, None, None)
        .await
        .unwrap();
    assert_eq!(act.items.len(), 1);
    let l = ai_team::list(&w.app, w.alice_ctx, None, None)
        .await
        .unwrap();
    assert_eq!(l.items[0].stats.writes_today, 1);
}

/// D1, the M1.5 counterpart: a write that only changed a custom field that
/// later became restricted (`visible = false`, no field role for the
/// viewer) neither shows nor counts; a site admin still sees its diff.
#[tokio::test]
async fn v_activity_hides_restricted_field_writes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dave = dave_viewer(&w).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let f: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ('Budget', 'int', true, true) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "INSERT INTO field_trackers (field_id, tracker_id) VALUES ({f}, '{}')",
            w.bug
        ),
    )
    .await;
    let cf = format!("cf:{f}");
    let i = w.issue(w.web, "budgeted").await;
    let d = agents::create_delegation(
        &pool,
        &w.policy,
        &w.alice_ctx,
        agents::DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: vec!["update_fields".into()],
            writable_fields: vec![cf.clone()],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    let (_, a) = w.token(d.context_id).await;
    let doc = json!({"command_schema_version": 1, "title": "budget",
                     "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                              "set": {cf.clone(): {"int": "7"}}}],
                     "preconditions": [{"target": tgt(i), "lock_version": lv(&w, i).await}]});
    applied(&submit(&w, a, &doc).await.unwrap());
    let before = ai_team::activity(&w.app, dave, d.agent_principal_id, None, None)
        .await
        .unwrap();
    assert_eq!(before.items.len(), 1, "visible while the field is visible");
    assert_eq!(before.items[0].diff[0].field, cf);
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = false WHERE id = {f}"),
    )
    .await;
    for who in [dave, w.alice_ctx] {
        let act = ai_team::activity(&w.app, who, d.agent_principal_id, None, None)
            .await
            .unwrap();
        assert!(
            act.items.is_empty(),
            "{:?}",
            serde_json::to_value(&act).unwrap()
        );
        let l = ai_team::list(&w.app, who, None, None).await.unwrap();
        assert_eq!(l.items[0].stats.writes_today, 0);
    }
    let act = ai_team::activity(&w.app, root, d.agent_principal_id, None, None)
        .await
        .unwrap();
    assert_eq!(act.items.len(), 1);
    assert_eq!(act.items[0].diff[0].field, cf);
}

/// T-M1-96: `log_time` on an issue is not an agent write of the chain
/// (neither a write nor a hand-off).
#[tokio::test]
async fn v_chain_ignores_log_time() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    exec(
        &pool,
        "UPDATE org_settings SET timelog_max_hours_per_day = 0, timelog_accept_0_hours = true,
                timelog_accept_future_dates = true, timelog_accept_closed_issues = true,
                timelog_require_issue = false, timelog_require_comments = false",
    )
    .await;
    exec(&pool, "UPDATE time_entry_activities SET active = true").await;
    grant(&pool, w.dev, &["log_time", "view_time_entries"]).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let i = w.issue(w.web, "timed").await;
    let a = create(
        &w,
        bob,
        &role(new_rt("pair"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let b = create(
        &w,
        bob,
        &with_commands(
            role(in_rt(a.runtime.id), w.client, "b", "custom", &[w.web]),
            &[("add_note", Direct), ("log_time", Direct)],
        ),
    )
    .await
    .unwrap();
    let a = agent(&w, a.token.as_deref().unwrap()).await;
    let b = agent(&w, b.token.as_deref().unwrap()).await;
    applied(&note(&w, a, i, "a1").await.unwrap());
    assert_eq!(chain(&w, i).await, Some((1, 0)));
    let log = json!({"command_schema_version": 1, "title": "time",
                     "ops": [{"op_id": op(), "kind": "log_time", "issue": tgt(i),
                              "fields": {"hours": "1", "spent_on": "2026-11-02"}}],
                     "preconditions": []});
    applied(&submit(&w, b, &log).await.unwrap());
    assert_eq!(chain(&w, i).await, Some((1, 0)), "log_time counted");
    applied(&note(&w, a, i, "a2").await.unwrap());
    assert_eq!(
        chain(&w, i).await,
        Some((2, 0)),
        "log_time counted as a hand-off"
    );
}

/// T-M1-97, sharper: at writes = 19 two concurrent direct writes on one
/// issue; the chain ends at 20 and the other one is sent for review.
#[tokio::test]
async fn v_chain_concurrent_at_limit() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let i = w.issue(w.web, "race").await;
    let a = create(
        &w,
        bob,
        &role(new_rt("pair"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a = agent(&w, a.token.as_deref().unwrap()).await;
    for k in 1..=19 {
        applied(&note(&w, a, i, &format!("w{k}")).await.unwrap());
        exec(&pool, "DELETE FROM agent_write_log").await;
    }
    assert_eq!(chain(&w, i).await, Some((19, 0)));
    let l = lv(&w, i).await;
    let (d1, d2) = (note_doc(i, l, "x"), note_doc(i, l, "y"));
    let (r1, r2) = common::wait_for("two submits by one agent", common::WAIT_TIMEOUT, async {
        tokio::join!(submit(&w, a, &d1), submit(&w, a, &d2))
    })
    .await;
    let outcomes = [&r1, &r2]
        .iter()
        .map(|r| match r {
            Ok(SubmitResult::Applied { .. }) => "applied".to_string(),
            Ok(SubmitResult::ReviewRequired { reason, .. }) => format!("review:{reason}"),
            Err(e) => format!("err:{e:#}"),
        })
        .collect::<Vec<_>>();
    let (writes, _) = chain(&w, i).await.unwrap();
    assert!(writes <= 20, "chain went past 20: {writes} {outcomes:?}");
    assert!(
        outcomes.iter().any(|o| o == "review:agent_chain"),
        "{outcomes:?}"
    );
}

/// V3: the 0029 trigger alone (no application lock) keeps one runtime per
/// sponsor and client under concurrency.
#[tokio::test]
async fn v_q2_trigger_concurrent_sql() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let c = new_client(&w, "Racer").await;
    let mut rts = Vec::new();
    for l in ["T1", "T2"] {
        rts.push(
            id(
                &pool,
                &format!(
                    "INSERT INTO agent_runtimes (sponsor_user_id, kind, label, isolation_ack, backfilled)
                     VALUES ('{}', 'external', '{l}', true, true) RETURNING id",
                    w.alice
                ),
            )
            .await,
        );
    }
    let url = common::test_db().url();
    let insert = |n: usize, rt: Uuid| {
        format!(
            "WITH p AS (INSERT INTO principals (kind, name) VALUES ('agent', 'r{n}') RETURNING id)
             INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id, handle,
                                           runtime_id, role_key, role_template, template_version)
             SELECT p.id, '{}', '{c}', 'race{n}@alice', '{rt}', 'personal', 'personal', 1 FROM p",
            w.alice
        )
    };
    let mut c1 = PgConnection::connect(&url).await.unwrap();
    let mut c2 = PgConnection::connect(&url).await.unwrap();
    sqlx::raw_sql("BEGIN").execute(&mut c1).await.unwrap();
    sqlx::raw_sql("BEGIN").execute(&mut c2).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(insert(1, rts[0])))
        .execute(&mut c1)
        .await
        .unwrap();
    let s2 = insert(2, rts[1]);
    let h = tokio::spawn(async move {
        let r = sqlx::raw_sql(sqlx::AssertSqlSafe(s2))
            .execute(&mut c2)
            .await;
        let ok = r.is_ok();
        let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
        let _ = sqlx::raw_sql(if ok { "COMMIT" } else { "ROLLBACK" })
            .execute(&mut c2)
            .await;
        (ok, msg)
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !h.is_finished(),
        "the second insert must wait for the first"
    );
    sqlx::raw_sql("COMMIT").execute(&mut c1).await.unwrap();
    let (ok, msg) = common::join("the concurrent statement", h).await;
    assert!(!ok, "both runtimes got the client");
    assert!(msg.contains("runtime_client_taken"), "{msg}");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM agent_principals WHERE agent_client_id = '{c}'")
        )
        .await,
        1
    );
}

/// §3.12.4: a chain review is not eligible for queue approval.
#[tokio::test]
async fn v_chain_review_not_queue_approvable() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "q").await;
    let (bob, a, b) = pair(&w).await;
    applied(&note(&w, a, i, "1").await.unwrap());
    applied(&note(&w, b, i, "2").await.unwrap());
    applied(&note(&w, a, i, "3").await.unwrap());
    let r = note(&w, b, i, "4").await.unwrap();
    let SubmitResult::ReviewRequired { reason, proposal } = r else {
        panic!("{r:?}")
    };
    assert_eq!(reason, "agent_chain");
    let e = changesets::approve_and_apply(
        &w.app,
        bob,
        Uuid::now_v7(),
        proposal.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: proposal.proposal_hash.clone(),
            review_mode: Some("queue".into()),
        },
    )
    .await;
    let failed = match &e {
        Err(_) => true,
        Ok(o) => o.execution.as_ref().is_none_or(|x| x.status != "succeeded"),
    };
    assert!(failed, "queue approval of a chain review went through");
    assert_eq!(chain(&w, i).await, Some((3, 2)));
}

/// The over-limit write is a queue-tickable `update_fields` (priority):
/// only the chain reason keeps it off the queue.
#[tokio::test]
async fn v_chain_review_tickable_shape_not_queue_approvable() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let i = w.issue(w.web, "q2").await;
    let a = create(
        &w,
        bob,
        &role(new_rt("pair2"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let mut bi = with_commands(
        role(in_rt(a.runtime.id), w.client, "b", "custom", &[w.web]),
        &[("add_note", Direct), ("update_fields", Direct)],
    );
    for d in &mut bi.delegations {
        d.policies
            .as_mut()
            .unwrap()
            .fields
            .insert("priority_id".into(), Direct);
    }
    let b = create(&w, bob, &bi).await.unwrap();
    let a = agent(&w, a.token.as_deref().unwrap()).await;
    let b = agent(&w, b.token.as_deref().unwrap()).await;
    applied(&note(&w, a, i, "1").await.unwrap());
    applied(&note(&w, b, i, "2").await.unwrap());
    applied(&note(&w, a, i, "3").await.unwrap());
    let high: Uuid = sqlx::query_scalar("SELECT id FROM issue_priorities WHERE name = 'High'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let d = json!({"command_schema_version": 1, "title": "prio",
        "ops": [{"op_id": Uuid::now_v7().to_string(), "kind": "update_fields",
                 "target": {"type": "issue", "id": i}, "set": {"priority_id": high}}],
        "preconditions": [{"target": {"type": "issue", "id": i}, "lock_version": lv(&w, i).await}]});
    let r = submit(&w, b, &d).await.unwrap();
    let SubmitResult::ReviewRequired { reason, proposal } = r else {
        panic!("{r:?}")
    };
    assert_eq!(reason, "agent_chain");
    let e = changesets::approve_and_apply(
        &w.app,
        bob,
        Uuid::now_v7(),
        proposal.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: proposal.proposal_hash.clone(),
            review_mode: Some("queue".into()),
        },
    )
    .await;
    let msg = format!("{:?}", e.as_ref().map(|_| ()).map_err(|x| format!("{x:#}")));
    assert!(
        msg.contains("review_outdated"),
        "queue approval of a chain review: {msg}"
    );
}

/// An agent token cannot use the person's command path (which records
/// `human_direct` and would clear the chain), nor undo.
#[tokio::test]
async fn v_agent_cannot_write_as_human() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "h").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "a", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a = agent(&w, r.token.as_deref().unwrap()).await;
    applied(&note(&w, a, i, "1").await.unwrap());
    let e = oxledger::commands::submit(
        &w.app,
        a,
        Uuid::now_v7(),
        &note_doc(i, lv(&w, i).await, "x"),
    )
    .await;
    assert!(e.is_err());
    assert_eq!(chain(&w, i).await, Some((1, 0)));
    let e = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        Uuid::now_v7(),
        &UndoInput::default(),
    )
    .await;
    assert!(e.is_err());
}

/// A role's rights stay within the sponsor's: a `personal` role of a
/// sponsor who may only add notes cannot edit fields or create issues.
#[tokio::test]
async fn v_template_cannot_exceed_sponsor() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let noter = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Noter','all') RETURNING id",
    )
    .await;
    grant(&pool, noter, &["view_issues", "add_issue_notes"]).await;
    let erin_id = auth::create_user(&pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, erin_id, noter).await;
    let erin = sign_in(&pool, &w.policy, "erin").await;
    let i = w.issue(w.web, "e").await;
    let r = create(
        &w,
        erin,
        &role(new_rt("erin box"), w.client, "all", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a = agent(&w, r.token.as_deref().unwrap()).await;
    applied(&note(&w, a, i, "fine").await.unwrap());
    let e = submit(&w, a, &subject_doc(i, lv(&w, i).await, "renamed"))
        .await
        .unwrap_err();
    assert!(
        matches!(code(&e), ErrorCode::ForbiddenOp | ErrorCode::NotFound),
        "{e:#}"
    );
    let e = submit(&w, a, &public_create(&w, w.web)).await.unwrap_err();
    assert!(
        matches!(code(&e), ErrorCode::ForbiddenOp | ErrorCode::NotFound),
        "{e:#}"
    );
    // Someone else's runtime cannot be chosen.
    let e = create(
        &w,
        w.alice_ctx,
        &role(in_rt(r.runtime.id), w.client, "steal", "personal", &[w.web]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // Nor someone else's context through the 02 alias.
    let e = agents::issue_credential_for(
        &pool,
        &w.policy,
        &w.alice_ctx,
        r.delegations[0].context_id,
        Some(r.principal.id),
        "t",
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

/// `POST /delegations` with `agent_client_id` lands on the client's
/// `personal` role even when another role of the same client exists.
#[tokio::test]
async fn v_alias_lands_on_personal() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "aaa", "custom", &[w.web]),
    )
    .await
    .unwrap();
    let d = w.delegate(w.auth_p).await.unwrap();
    assert_eq!(d.runtime_id, r.runtime.id, "Q2: same runtime");
    assert_eq!(d.role_key, "personal");
    assert_ne!(d.agent_principal_id, r.principal.id);
    let d2 = w.delegate(w.web).await.unwrap();
    assert_eq!(d2.agent_principal_id, d.agent_principal_id);
}

/// D4: the 02 alias creating a role is held to the same B-31 limit.
#[tokio::test]
async fn v_alias_respects_role_limit() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let first = create(
        &w,
        w.alice_ctx,
        &role(new_rt("full"), w.client, "r0", "custom", &[w.web]),
    )
    .await
    .unwrap();
    for k in 1..ai_team::MAX_ACTIVE_ROLES {
        create(
            &w,
            w.alice_ctx,
            &role(
                in_rt(first.runtime.id),
                w.client,
                &format!("r{k}"),
                "custom",
                &[w.web],
            ),
        )
        .await
        .unwrap();
    }
    let e = w.delegate(w.web).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
    assert_eq!(kind(&e), "role_limit");
    assert_eq!(details(&e)["path"], "agent_client_id");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_principals WHERE delegator_user_id = '{}'",
                w.alice
            )
        )
        .await,
        ai_team::MAX_ACTIVE_ROLES
    );
    // Retiring one frees a place for the alias.
    ai_team::retire(&w.app, &w.policy, w.alice_ctx, first.principal.id)
        .await
        .unwrap();
    let d = w.delegate(w.web).await.unwrap();
    assert_eq!(d.role_key, "personal");
}

/// Resume needs a step-up; pause does not.
#[tokio::test]
async fn v_resume_needs_step_up() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let p = |reason: Option<&str>| PauseInput {
        scope: "principal".into(),
        project_id: None,
        reason: reason.map(String::from),
    };
    ai_team::pause(&w.app, w.alice_ctx, r.principal.id, &p(Some("why")))
        .await
        .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE browser_sessions SET reauthenticated_at = now() - interval '1 day' WHERE user_id = '{}'",
            w.alice
        ),
    )
    .await;
    let e = ai_team::resume(
        &w.app,
        &w.policy,
        w.alice_ctx,
        r.principal.id,
        &ResumeInput {
            scope: "principal".into(),
            project_id: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReauthenticationRequired);
    ai_team::pause(&w.app, w.alice_ctx, r.principal.id, &p(None))
        .await
        .unwrap();
}

/// D9: a resume re-checks the breaker; a suspended shared context stays
/// suspended (and the role paused) until the context itself is resumed.
#[tokio::test]
async fn v_resume_rechecks_breaker() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    ai_team::pause(
        &w.app,
        w.alice_ctx,
        r.principal.id,
        &PauseInput {
            scope: "principal".into(),
            project_id: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE agent_context_write_controls SET suspended_at = now(), suspension_reason = 'audience_blocked'
             WHERE context_id = '{}'",
            r.delegations[0].context_id
        ),
    )
    .await;
    let resume = ResumeInput {
        scope: "principal".into(),
        project_id: None,
    };
    let e = ai_team::resume(&w.app, &w.policy, w.alice_ctx, r.principal.id, &resume)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AgentSuspended, "{e:#}");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_principals WHERE agent_principal_id = '{}' AND paused_at IS NOT NULL",
                r.principal.id
            )
        )
        .await,
        1,
        "still paused"
    );
    exec(
        &pool,
        "UPDATE agent_context_write_controls SET suspended_at = NULL, suspension_reason = NULL",
    )
    .await;
    ai_team::resume(&w.app, &w.policy, w.alice_ctx, r.principal.id, &resume)
        .await
        .unwrap();
}

/// D2: the pause check holds the role's row FOR SHARE until the write
/// commits, so a pause waits for a running write, and every write that
/// starts after the pause's commit is refused.
#[tokio::test]
async fn v_pause_waits_for_running_write() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let i = w.issue(w.web, "racing").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let a = agent(&w, r.token.as_deref().unwrap()).await;
    let gate = Arc::new(Gate::default());
    faults::arm("ai_team.after_pause_check", Action::Block(gate.clone()));
    let pause_in = PauseInput {
        scope: "principal".into(),
        project_id: None,
        reason: None,
    };
    let write = note(&w, a, i, "in flight");
    let pausing = async {
        gate.reached.notified().await;
        let p = ai_team::pause(&w.app, w.alice_ctx, r.principal.id, &pause_in);
        tokio::pin!(p);
        let early = tokio::time::timeout(std::time::Duration::from_millis(400), &mut p).await;
        assert!(
            early.is_err(),
            "the pause did not wait for the running write"
        );
        gate.release.notify_one();
        common::wait_for("the pause after the release", common::WAIT_TIMEOUT, p)
            .await
            .unwrap();
    };
    let (written, ()) = common::wait_for(
        "the running write and the pause",
        common::WAIT_TIMEOUT,
        async { tokio::join!(write, pausing) },
    )
    .await;
    applied(&written.unwrap());
    let e = note(&w, a, i, "after").await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused");
    faults::clear();
}

/// D3: a site admin's pause survives the sponsor retiring the role (or the
/// whole runtime) and starting over; only a site admin lifts it. Who
/// paused shows only to the viewers who may see it.
#[tokio::test]
async fn v_admin_pause_not_bypassed_by_retire() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let i = w.issue(w.web, "p").await;
    let x = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "x", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let principal = |reason: Option<&str>| PauseInput {
        scope: "principal".into(),
        project_id: None,
        reason: reason.map(String::from),
    };
    ai_team::pause(
        &w.app,
        root,
        x.principal.id,
        &principal(Some("incident 42")),
    )
    .await
    .unwrap();
    // Who paused: the sponsor sees it, a member does not.
    let d = ai_team::detail(&w.app, w.alice_ctx, x.principal.id)
        .await
        .unwrap();
    let p = d.pause.unwrap();
    assert_eq!(p.paused_by.as_deref(), Some("site_admin"));
    assert_eq!(p.reason.as_deref(), Some("incident 42"));
    let d = ai_team::detail(&w.app, carol, x.principal.id)
        .await
        .unwrap();
    let p = d.pause.unwrap();
    assert!(p.paused_by.is_none() && p.reason.is_none());

    // Retire the paused role and create another one in the same runtime.
    ai_team::retire(&w.app, &w.policy, w.alice_ctx, x.principal.id)
        .await
        .unwrap();
    let y = create(
        &w,
        w.alice_ctx,
        &role(in_rt(x.runtime.id), w.client, "y", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let ya = agent(&w, y.token.as_deref().unwrap()).await;
    let e = note(&w, ya, i, "y").await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused", "{e:#}");
    assert_eq!(details(&e)["scope"], "principal");
    // Retire the whole runtime and start a new one with the same client.
    ai_team::retire_runtime(&w.app, &w.policy, w.alice_ctx, x.runtime.id)
        .await
        .unwrap();
    let z = create(
        &w,
        w.alice_ctx,
        &role(new_rt("fresh"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let za = agent(&w, z.token.as_deref().unwrap()).await;
    let e = note(&w, za, i, "z").await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused", "{e:#}");
    // The sponsor cannot lift it; a site admin can.
    let resume = ResumeInput {
        scope: "principal".into(),
        project_id: None,
    };
    let e = ai_team::resume(&w.app, &w.policy, w.alice_ctx, z.principal.id, &resume)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    ai_team::resume(&w.app, &w.policy, root, z.principal.id, &resume)
        .await
        .unwrap();
    applied(&note(&w, za, i, "z back").await.unwrap());
    // Lifted for good: a further role of the client starts unpaused.
    let v = create(
        &w,
        w.alice_ctx,
        &role(in_rt(z.runtime.id), w.client, "v", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let va = agent(&w, v.token.as_deref().unwrap()).await;
    applied(&note(&w, va, i, "v").await.unwrap());

    // A project-level pause by a project manager: `project` on the policy
    // row, for the sponsor; a plain member gets no policy rows at all.
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(&pool, mgr, &["view_issues", "manage_project"]).await;
    exec(&pool, &format!(
        "INSERT INTO member_roles SELECT m.id, '{mgr}' FROM members m WHERE m.project_id = '{}' AND m.principal_id = '{}'",
        w.web, w.bob
    ))
    .await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    ai_team::pause(
        &w.app,
        bob,
        v.principal.id,
        &PauseInput {
            scope: "project".into(),
            project_id: Some(w.web),
            reason: Some("noisy".into()),
        },
    )
    .await
    .unwrap();
    let d = ai_team::detail(&w.app, w.alice_ctx, v.principal.id)
        .await
        .unwrap();
    assert_eq!(d.policies[0].paused_by.as_deref(), Some("project"));
    let d = ai_team::detail(&w.app, carol, v.principal.id)
        .await
        .unwrap();
    assert!(d.policies.is_empty());
}

/// The principal-level reason is for the sponsor and site admins; the
/// project-level reason for that project's managers (and the sponsor).
/// Members see neither, nor does the agent.
#[tokio::test]
async fn v_pause_reason_not_leaked() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    for (scope, project) in [("principal", None), ("project", Some(w.web))] {
        ai_team::pause(
            &w.app,
            w.alice_ctx,
            r.principal.id,
            &PauseInput {
                scope: scope.into(),
                project_id: project,
                reason: Some(format!("SECRET-{scope}")),
            },
        )
        .await
        .unwrap();
    }
    let d = ai_team::detail(&w.app, carol, r.principal.id)
        .await
        .unwrap();
    let text = serde_json::to_string(&d).unwrap();
    assert!(!text.contains("SECRET"), "{text}");
    let l = ai_team::list(&w.app, carol, None, None).await.unwrap();
    let text = serde_json::to_string(&l).unwrap();
    assert!(!text.contains("SECRET"), "{text}");
    let a = agent(&w, r.token.as_deref().unwrap()).await;
    let i = w.issue(w.web, "p").await;
    let e = note(&w, a, i, "x").await.unwrap_err();
    assert!(!format!("{e:#} {}", details(&e)).contains("SECRET"));
}

/// D8: a site admin sees every external role, delegated or not (§3.12.1
/// incident handling); a reader of a public project who is not a member
/// does not see roles delegated there.
#[tokio::test]
async fn v_team_visibility_admin_and_public_non_member() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let r = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "z", "personal", &[w.web]),
    )
    .await
    .unwrap();
    // No delegation left at all.
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET revoked_at = now(), revoked_reason = 'test' WHERE agent_principal_id = '{}'",
            r.principal.id
        ),
    )
    .await;
    let l = ai_team::list(&w.app, root, None, None).await.unwrap();
    assert!(l.items.iter().any(|m| m.id == r.principal.id));
    let d = ai_team::detail(&w.app, root, r.principal.id).await.unwrap();
    assert!(d.member.duties.is_some());
    assert!(d.actions.contains(&"pause".to_string()));
    let carol = sign_in(&pool, &w.policy, "carol").await;
    assert!(
        ai_team::detail(&w.app, carol, r.principal.id)
            .await
            .is_err()
    );

    // WEB public, read by dave through the non-member role.
    let r2 = create(
        &w,
        w.alice_ctx,
        &role(in_rt(r.runtime.id), w.client, "y", "personal", &[w.web]),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE projects SET is_public = true WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    let nm = id(
        &pool,
        "WITH ins AS (INSERT INTO roles (name, issues_visibility, builtin)
                      SELECT 'Non member', 'all', 'non_member'
                      WHERE NOT EXISTS (SELECT 1 FROM roles WHERE builtin = 'non_member') RETURNING id)
         SELECT id FROM ins UNION ALL SELECT id FROM roles WHERE builtin = 'non_member'",
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permissions (role_id, permission) VALUES ('{nm}', 'view_issues') ON CONFLICT DO NOTHING"
        ),
    )
    .await;
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let dave = sign_in(&pool, &w.policy, "dave").await;
    let mut conn = pool.acquire().await.unwrap();
    let reads: Vec<Uuid> = oxledger::db::load_scope(&mut conn, dave)
        .await
        .unwrap()
        .project_ids()
        .collect();
    drop(conn);
    assert!(reads.contains(&w.web), "setup: dave reads public WEB");
    assert!(
        ai_team::list(&w.app, dave, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let e1 = ai_team::detail(&w.app, dave, r2.principal.id)
        .await
        .unwrap_err();
    let e2 = ai_team::detail(&w.app, dave, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(format!("{e1}"), format!("{e2}"));
}

// ---------------------------------------------------------------- HTTP: invisible == missing == retired

struct Resp {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
}

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
}

async fn call(r: &Router, method: &str, path: &str, body: Option<Value>, who: &Browser) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", who.session))
        .header("x-csrf-token", &who.csrf);
    if method != "GET" {
        b = b
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", Uuid::now_v7().to_string());
    }
    let body = body.map(|v| Body::from(v.to_string())).unwrap_or_default();
    let resp = r.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let mut headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "date" | "x-request-id"))
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string()))
        .collect();
    headers.sort();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn cookie(resp: &axum::response::Response, name: &str) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{name}="))
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
    let pre = cookie(&lc, LOGIN_COOKIE);
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={pre}"))
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
    let session = cookie(&resp, SESSION_COOKIE);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

fn same(label: &str, a: &Resp, b: &Resp) {
    assert_eq!(a.status, b.status, "{label}: status");
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(e) = v.get_mut("error").and_then(Value::as_object_mut) {
            e.remove("request_id");
        }
        v
    };
    assert_eq!(strip(&a.body), strip(&b.body), "{label}: body");
    assert_eq!(a.headers, b.headers, "{label}: headers");
}

/// 看不到＝不存在 over HTTP: an invisible role, a retired one and a missing
/// id answer the same, for every `/ai-team/{id}*` and `/me/agent-runtimes/{id}*`.
#[tokio::test]
async fn v_http_invisible_equals_missing() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let carol = login(&r, "carol").await;
    // hidden: AUTH only (carol is not in AUTH); gone: was in WEB.
    let hidden = create(
        &w,
        w.alice_ctx,
        &role(new_rt("box"), w.client, "hidden", "personal", &[w.auth_p]),
    )
    .await
    .unwrap();
    let gone = create(
        &w,
        w.alice_ctx,
        &role(
            in_rt(hidden.runtime.id),
            w.client,
            "gone",
            "personal",
            &[w.web],
        ),
    )
    .await
    .unwrap();
    let seen = call(
        &r,
        "GET",
        &format!("/ai-team/{}", gone.principal.id),
        None,
        &carol,
    )
    .await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);
    ai_team::retire(&w.app, &w.policy, w.alice_ctx, gone.principal.id)
        .await
        .unwrap();
    let missing = Uuid::now_v7();
    for suffix in ["", "/activity"] {
        let a = call(
            &r,
            "GET",
            &format!("/ai-team/{}{suffix}", hidden.principal.id),
            None,
            &carol,
        )
        .await;
        let b = call(
            &r,
            "GET",
            &format!("/ai-team/{missing}{suffix}"),
            None,
            &carol,
        )
        .await;
        let c = call(
            &r,
            "GET",
            &format!("/ai-team/{}{suffix}", gone.principal.id),
            None,
            &carol,
        )
        .await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
        same(&format!("hidden vs missing {suffix}"), &a, &b);
        same(&format!("retired vs missing {suffix}"), &c, &b);
    }
    for (path, body) in [
        ("pause", json!({"scope": "principal"})),
        ("resume", json!({"scope": "principal"})),
        ("retire", json!({})),
        ("credentials", json!({})),
    ] {
        let a = call(
            &r,
            "POST",
            &format!("/ai-team/{}/{path}", hidden.principal.id),
            Some(body.clone()),
            &carol,
        )
        .await;
        let b = call(
            &r,
            "POST",
            &format!("/ai-team/{missing}/{path}"),
            Some(body.clone()),
            &carol,
        )
        .await;
        same(&format!("POST {path}"), &a, &b);
    }
    // alice's runtime, as bob sees it, is like a missing one.
    let bob = login(&r, "bob").await;
    for (m, path, body) in [
        (
            "PATCH",
            String::new(),
            Some(json!({"label": "x", "lock_version": 1})),
        ),
        (
            "POST",
            "/new-context".to_string(),
            Some(json!({"fresh_context_ack": true})),
        ),
        ("POST", "/retire".to_string(), Some(json!({}))),
    ] {
        let a = call(
            &r,
            m,
            &format!("/me/agent-runtimes/{}{path}", hidden.runtime.id),
            body.clone(),
            &bob,
        )
        .await;
        let b = call(
            &r,
            m,
            &format!("/me/agent-runtimes/{missing}{path}"),
            body.clone(),
            &bob,
        )
        .await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{} {}", m, a.body);
        same(&format!("runtime {m}{path}"), &a, &b);
    }
}

/// The `shared_runtime` warning names only the sponsor's own roles.
#[tokio::test]
async fn v_shared_warning_is_own() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    create(
        &w,
        bob,
        &role(
            new_rt("bob box"),
            w.client,
            "bobrole",
            "personal",
            &[w.auth_p],
        ),
    )
    .await
    .unwrap();
    let x = create(
        &w,
        w.alice_ctx,
        &role(new_rt("alice box"), w.client, "x", "personal", &[w.web]),
    )
    .await
    .unwrap();
    let y = create(
        &w,
        w.alice_ctx,
        &role(in_rt(x.runtime.id), w.client, "y", "custom", &[w.web]),
    )
    .await
    .unwrap();
    let text = serde_json::to_string(&y.warnings).unwrap();
    assert!(!text.contains("bob"), "{text}");
    assert!(!text.contains("AUTH"), "{text}");
}

/// P2 G14 / G24 over HTTP: the token lists (sponsor and site admin; a
/// member is told not_sponsor; an unrelated user sees the same 404 as for a
/// missing id; the context alias is the sponsor's only) and the activity
/// lists' cursor and limit query.
#[tokio::test]
async fn p2_http_credentials_and_activity_query() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (token, agent) = w.token(ctx).await;
    let aid = agent.principal_id().unwrap();
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let alice = login(&r, "alice").await;
    let bob = login(&r, "bob").await;
    let root = login(&r, "root").await;
    let dave = login(&r, "dave").await;
    let hash: Vec<u8> = sqlx::query_scalar("SELECT token_sha256 FROM agent_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    let hexh: String = hash.iter().map(|b| format!("{b:02x}")).collect();

    let p = format!("/ai-team/{aid}/credentials");
    for who in [&alice, &root] {
        let x = call(&r, "GET", &p, None, who).await;
        assert_eq!(x.status, StatusCode::OK, "{}", x.body);
        assert_eq!(x.body["data"]["items"].as_array().unwrap().len(), 1);
        let text = x.body.to_string();
        assert!(!text.contains(&token) && !text.to_lowercase().contains(&hexh));
    }
    let b = call(&r, "GET", &p, None, &bob).await;
    assert_eq!(b.status, StatusCode::FORBIDDEN, "{}", b.body);
    assert_eq!(b.body["error"]["code"], "forbidden_op");
    assert_eq!(b.body["error"]["details"]["reason"], "not_sponsor");
    let d = call(&r, "GET", &p, None, &dave).await;
    let m = call(
        &r,
        "GET",
        &format!("/ai-team/{}/credentials", Uuid::now_v7()),
        None,
        &dave,
    )
    .await;
    assert_eq!(d.status, StatusCode::NOT_FOUND);
    same("unrelated vs missing role", &d, &m);

    // M2.0 (design 06 §3.1): the 02 alias /agent-contexts/{id}/credentials
    // is gone; the context's owner gets the router's 404 like anyone else.
    let alias = format!("/agent-contexts/{ctx}/credentials");
    let gone = call(&r, "GET", &alias, None, &alice).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
    for who in [&root, &bob, &dave] {
        let x = call(&r, "GET", &alias, None, who).await;
        same("removed alias, owner vs others", &x, &gone);
    }

    for base in [
        format!("/agents/{aid}/activity"),
        format!("/ai-team/{aid}/activity"),
    ] {
        let x = call(&r, "GET", &format!("{base}?limit=1"), None, &alice).await;
        assert_eq!(x.status, StatusCode::OK, "{}", x.body);
        assert!(x.body["data"]["items"].as_array().unwrap().len() <= 1);
        assert!(x.body["data"].get("next_cursor").is_some());
        let x = call(&r, "GET", &format!("{base}?limit=abc"), None, &alice).await;
        assert_eq!(x.status, StatusCode::BAD_REQUEST, "{base}: {}", x.body);
        assert_eq!(x.body["error"]["code"], "invalid_argument");
        let x = call(&r, "GET", &format!("{base}?cursor=garbage"), None, &alice).await;
        assert_eq!(
            x.body["error"]["code"], "invalid_cursor",
            "{base}: {}",
            x.body
        );
        let x = call(&r, "GET", &format!("{base}?from=bad"), None, &alice).await;
        assert_eq!(x.status, StatusCode::BAD_REQUEST, "{base}: {}", x.body);
    }
}
