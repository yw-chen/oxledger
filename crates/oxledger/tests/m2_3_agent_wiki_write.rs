// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.3b, the writing half: agents write the wiki through
//! `command_submit` (§3.11, §3.12 R-AG3, R-AG5, R-AG6, R-AG11; §4.3, §4.4).
//! T-M2-40 and T-M2-41 (writes), 42c (write), 43, 44 (direct, review and
//! off, the pauses, a closed context), 45, 46, 47a, 48; the provenance of
//! an `agent_direct` and an approved (`external_agent`) wiki write, a
//! resend, a revert's history rule, the delegation defaults, the 「文件維護」
//! template, and wiki proposals: their check, diff, conflicts, readers and
//! revisions. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput, PolicyInput};
use oxledger::ai_team;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::{self, ApproveInput, SubmitResult, ValidateInput};
use oxledger::commands;
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::quota::limits;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

const ORIGIN: &str = "https://spa.test";

const DEV_WIKI: [&str; 6] = [
    "view_wiki_pages",
    "view_wiki_edits",
    "edit_wiki_pages",
    "rename_wiki_pages",
    "delete_wiki_pages",
    "protect_wiki_pages",
];

async fn role(pool: &PgPool, name: &str, visibility: &str, perms: &[&str]) -> Uuid {
    let r = id(
        pool,
        &format!(
            "INSERT INTO roles (name, issues_visibility) VALUES ('{name}','{visibility}') RETURNING id"
        ),
    )
    .await;
    grant(pool, r, perms).await;
    r
}

async fn user(pool: &PgPool, login: &str) -> Uuid {
    auth::create_user(pool, login, login, PW, false)
        .await
        .unwrap()
}

async fn setup(pool: &PgPool) -> World {
    let w = World::build(pool).await;
    exec(
        pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(pool, w.dev, &DEV_WIKI).await;
    w
}

/// alice's own wiki command (POST /commands).
async fn person(w: &World, op: Value, pre: Option<Value>) -> commands::CommandResult {
    let mut d = json!({"command_schema_version": 1, "ops": [op]});
    if let Some(p) = pre {
        d["preconditions"] = json!([p]);
    }
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
        .await
        .unwrap_or_else(|e| panic!("{d}: {e:#}"))
}

async fn page(w: &World, project: &str, title: &str, body: &str) -> Uuid {
    person(
        w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": project, "page": title}, "body": body}),
        None,
    )
    .await
    .wiki[0]
        .page_id
}

async fn lock_version(w: &World, page: Uuid) -> i64 {
    count(
        &w.pool,
        &format!("SELECT lock_version FROM wiki_pages WHERE id = '{page}'"),
    )
    .await
}

/// alice delegates `projects` with the issue ops of [`World::delegate`]
/// and the wiki ops (`modes` override the defaults); the agent's context.
async fn agent_with(
    w: &World,
    projects: &[Uuid],
    modes: &[(&str, PolicyMode)],
    read_own_time: bool,
) -> AuthContext {
    let mut ctx = None;
    for p in projects {
        let policies = (!modes.is_empty()).then(|| PolicyInput {
            commands: modes
                .iter()
                .map(|(k, m)| (k.to_string(), *m))
                .collect::<BTreeMap<_, _>>(),
            ..Default::default()
        });
        let v = agents::create_delegation(
            &w.pool,
            &w.policy,
            &w.alice_ctx,
            DelegationInput {
                agent_client_id: w.client,
                project_id: *p,
                proposal_ops: ["add_note", "update_fields", "wiki_put", "wiki_move"]
                    .map(String::from)
                    .to_vec(),
                writable_fields: ["subject"].map(String::from).to_vec(),
                max_ops: None,
                expires_in_days: Some(30),
                policies,
                read_own_time,
                read_own_time_ack: read_own_time,
            },
        )
        .await
        .unwrap();
        ctx = Some(v.context_id);
    }
    w.token(ctx.expect("a project")).await.1
}

async fn agent(w: &World, projects: &[Uuid]) -> AuthContext {
    agent_with(w, projects, &[], false).await
}

fn put(project: &str, page: &str, body: &str) -> Value {
    json!({"command_schema_version": 1, "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_put",
           "target": {"project": project, "page": page}, "body": body}]})
}

fn update(project: &str, page: Uuid, lv: i64, body: &str) -> Value {
    json!({"command_schema_version": 1,
           "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                    "target": {"project": project, "page_id": page}, "body": body}],
           "preconditions": [{"wiki_page": page, "lock_version": lv}]})
}

async fn submit(w: &World, a: AuthContext, doc: &Value) -> anyhow::Result<Value> {
    submit_as(w, a, Uuid::now_v7(), doc, false).await
}

async fn submit_as(
    w: &World,
    a: AuthContext,
    op: Uuid,
    doc: &Value,
    review: bool,
) -> anyhow::Result<Value> {
    changesets::command_submit(&w.app, a, op, doc, review, ORIGIN)
        .await
        .map(|r: SubmitResult| serde_json::to_value(r).unwrap())
}

fn reason(e: &anyhow::Error) -> String {
    classify(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .details
        .and_then(|d| d.get("reason").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

async fn body_of(w: &World, page: Uuid) -> String {
    sqlx::query_scalar(
        "SELECT v.body FROM wiki_page_versions v JOIN wiki_pages p ON p.id = v.page_id
         WHERE p.id = $1 AND v.version = p.current_version",
    )
    .bind(page)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------- T-M2-40

/// origin, actor_id, actor_kind, on_behalf_of_id, delegation_id,
/// agent_client_id, command_execution_id.
type ProvRow = (
    String,
    Uuid,
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
);

/// T-M2-40, writing: W(P) = A(P). A new page, an update with its
/// lock_version, a move; each an `agent_direct` execution for alice, counted
/// per page; a resend replays; a stale lock_version is `conflict`.
#[tokio::test]
async fn t40_agent_writes_a_delegated_wiki() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let a = agent(&w, &[w.web]).await;
    let agent_id = a.principal_id().unwrap();

    let op = Uuid::now_v7();
    let doc = put("WEB", "Runbook", "step 1");
    let r = submit_as(&w, a, op, &doc, false).await.unwrap();
    assert_eq!(r["outcome"], "applied", "{r}");
    let created = &r["wiki"][0];
    assert_eq!(created["created"], true);
    assert_eq!(created["title"], "Runbook");
    let page: Uuid = created["page_id"].as_str().unwrap().parse().unwrap();
    assert!(r["search"]["consistent_after"].as_str().is_some());

    // The same request again: the stored result, nothing written twice.
    let again = submit_as(&w, a, op, &doc, false).await.unwrap();
    assert_eq!(again["execution_id"], r["execution_id"]);
    assert_eq!(again["wiki"][0]["page_id"], created["page_id"]);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM wiki_page_versions WHERE page_id = '{page}'")
        )
        .await,
        1
    );

    // Provenance (0034's CHECK for agent_direct).
    let (origin, actor, kind, behalf, deleg, client, exec): ProvRow = sqlx::query_as(
        "SELECT origin, actor_id, actor_kind, on_behalf_of_id, delegation_id, agent_client_id,
                command_execution_id
         FROM wiki_page_versions WHERE page_id = $1 AND version = 1",
    )
    .bind(page)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (origin.as_str(), actor, kind.as_str(), behalf),
        ("agent_direct", agent_id, "agent", Some(w.alice))
    );
    assert!(deleg.is_some() && client == Some(w.client));
    let exec = exec.unwrap();
    assert_eq!(exec.to_string(), r["execution_id"].as_str().unwrap());
    let (eorigin, ebehalf): (String, Option<Uuid>) =
        sqlx::query_as("SELECT origin, on_behalf_of_id FROM command_executions WHERE id = $1")
            .bind(exec)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((eorigin.as_str(), ebehalf), ("agent_direct", Some(w.alice)));
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM command_execution_source_projects WHERE execution_id = '{exec}'")
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM wiki_events WHERE page_id = '{page}' AND origin = 'agent_direct'")
        )
        .await,
        1,
        "the created event"
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM agent_write_log WHERE kind = 'wiki_write' AND wiki_page_id = '{page}'")
        )
        .await,
        1
    );

    // An update needs the lock_version; a stale one is a conflict.
    let e = submit(
        &w,
        a,
        &json!({"command_schema_version": 1, "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page_id": page}, "body": "x"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let lv = lock_version(&w, page).await;
    let r = submit(&w, a, &update("WEB", page, lv, "step 1\nstep 2"))
        .await
        .unwrap();
    assert_eq!(r["wiki"][0]["version"], 2, "{r}");
    assert_eq!(body_of(&w, page).await, "step 1\nstep 2");
    let e = submit(&w, a, &update("WEB", page, lv, "stale"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);

    // wiki_move: a new title, keeping a redirect.
    let r = submit(
        &w,
        a,
        &json!({"command_schema_version": 1, "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_move",
                "target": {"project": "WEB", "page_id": page}, "title": "Runbook v2"}]}),
    )
    .await
    .unwrap();
    assert_eq!(r["wiki"][0]["title"], "Runbook v2", "{r}");

    // A page title that is taken is wiki_page_exists, as for people.
    let e = submit(&w, a, &put("WEB", "Runbook v2", "dup"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::WikiPageExists);
}

// ---------------------------------------------------------------- T-M2-41, 42c, 43

/// T-M2-41: a role reading only the wiki (W ⊋ A): readable, not writable.
#[tokio::test]
async fn t41_a_wider_wiki_is_not_writable() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let wiki_only = role(&pool, "WikiOnly", "all", &["view_wiki_pages"]).await;
    let dora = user(&pool, "dora").await;
    member(&pool, w.web, dora, wiki_only).await;
    let p = page(&w, "WEB", "Home", "hello").await;
    let a = agent(&w, &[w.web]).await;
    let e = submit(
        &w,
        a,
        &update("WEB", p, lock_version(&w, p).await, "changed"),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden);
    assert_eq!(body_of(&w, p).await, "hello");
}

/// T-M2-42c (regression): an `own` role holding view_wiki_pages makes W ⊋ A
/// (`holders()` would miss it and let the write through). The role has no
/// view_issues: one that reads issues with `own` makes the project not
/// delegable at all.
#[tokio::test]
async fn t42c_own_role_blocks_the_write() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let own = role(&pool, "OwnWiki", "own", &["view_wiki_pages"]).await;
    let frank = user(&pool, "frank").await;
    member(&pool, w.web, frank, own).await;
    let a = agent(&w, &[w.web]).await;
    let e = submit(&w, a, &put("WEB", "New", "x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden);
}

/// T-M2-43: with AUTH (fewer readers) among the sources, neither a WEB note
/// nor a WEB wiki page is allowed; AUTH's own wiki is.
#[tokio::test]
async fn t43_sources_narrow_the_destinations() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let i = w.issue(w.web, "web").await;
    let a = agent(&w, &[w.web, w.auth_p]).await;
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let e = submit(&w, a, &note_doc(i, lv, "from AUTH"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden);
    let e = submit(&w, a, &put("WEB", "Notes", "from AUTH"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden);
    let r = submit(&w, a, &put("AUTH", "Notes", "inside AUTH"))
        .await
        .unwrap();
    assert_eq!(r["outcome"], "applied");
}

// ---------------------------------------------------------------- T-M2-44

/// T-M2-44 (review below): `off`, a paused principal, a paused delegation,
/// a closed context.
#[tokio::test]
async fn t44_policy_pauses_and_the_context() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let off = agent_with(
        &w,
        &[w.web],
        &[("wiki_put", Off), ("wiki_move", Direct)],
        false,
    )
    .await;
    let e = submit(&w, off, &put("WEB", "Off", "x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "policy");

    w.new_context().await;
    let w2 = &w;
    let a = agent(w2, &[w2.web]).await;
    let agent_id = a.principal_id().unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE agent_principals SET paused_at = now(), paused_by = '{}', pause_scope = 'sponsor'
             WHERE agent_principal_id = '{agent_id}'",
            w2.alice
        ),
    )
    .await;
    let e = submit(w2, a, &put("WEB", "Paused", "x")).await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused");
    exec(
        &pool,
        &format!(
            "UPDATE agent_principals SET paused_at = NULL, paused_by = NULL, pause_scope = NULL
             WHERE agent_principal_id = '{agent_id}'"
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET paused_at = now(), paused_by = '{}', paused_reason = 'r'
             WHERE agent_principal_id = '{agent_id}'",
            w2.alice
        ),
    )
    .await;
    let e = submit(w2, a, &put("WEB", "Paused", "x")).await.unwrap_err();
    assert_eq!(reason(&e), "agent_paused");
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET paused_at = NULL, paused_by = NULL, paused_reason = NULL
             WHERE agent_principal_id = '{agent_id}'"
        ),
    )
    .await;
    submit(w2, a, &put("WEB", "Fine", "x")).await.unwrap();

    // A permission change moves WEB's epoch: the context is closed to writes.
    let later = role(&pool, "Later", "all", &[]).await;
    let gina = user(&pool, "gina").await;
    member(&pool, w2.web, gina, later).await;
    grant(&pool, later, &["view_wiki_pages"]).await;
    let e = submit(w2, a, &put("WEB", "Late", "x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
}

// ---------------------------------------------------------------- T-M2-45

/// T-M2-45: a protected page (even though alice may protect pages),
/// attachments, and the wiki ops that are for people only.
#[tokio::test]
async fn t45_protected_attachments_and_people_only_ops() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Policy", "rules").await;
    person(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_protect",
               "target": {"project": "WEB", "page_id": p}, "protected": true}),
        None,
    )
    .await;
    let a = agent(&w, &[w.web]).await;
    let e = submit(&w, a, &update("WEB", p, lock_version(&w, p).await, "mine"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "wiki_protected");
    let e = submit(
        &w,
        a,
        &json!({"command_schema_version": 1, "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_move",
                "target": {"project": "WEB", "page_id": p}, "title": "Moved"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "wiki_protected");

    let mut doc = put("WEB", "Pic", "x");
    doc["ops"][0]["attachments"] = json!([{"upload_id": Uuid::now_v7()}]);
    let e = submit(&w, a, &doc).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "attachments_people_only");

    for op in [
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_delete", "target": {"project": "WEB", "page_id": p}}),
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_protect", "target": {"project": "WEB", "page_id": p}, "protected": false}),
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_restore", "target": {"project": "WEB", "page_id": p}}),
    ] {
        let e = submit(&w, a, &json!({"command_schema_version": 1, "ops": [op]}))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "{op}");
    }
}

// ---------------------------------------------------------------- T-M2-46

/// T-M2-46 / R-AG6: the 11th write to one page in 10 minutes suspends the
/// context (`wiki_write_burst`) and leaves the page as it was.
#[tokio::test]
async fn t46_one_page_written_too_often() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Hot", "v1").await;
    let a = agent(&w, &[w.web]).await;
    let (agent_id, ctx) = (a.principal_id().unwrap(), a.context_id().unwrap());
    // Ten earlier writes (spread out so the per-minute quota is not what
    // stops the next one).
    for i in 0..limits::WIKI_PAGE_WRITES_PER_10_MIN {
        exec(
            &pool,
            &format!(
                "INSERT INTO agent_write_log (agent_principal_id, context_id, kind, wiki_page_id, at)
                 VALUES ('{agent_id}', '{ctx}', 'wiki_write', '{p}', clock_timestamp() - interval '{} seconds')",
                90 + i * 30
            ),
        )
        .await;
    }
    let e = submit(&w, a, &update("WEB", p, lock_version(&w, p).await, "v2"))
        .await
        .unwrap_err();
    let ae = classify(&e).unwrap();
    assert_eq!(ae.code, ErrorCode::AgentSuspended, "{e:#}");
    assert_eq!(ae.details.unwrap()["reason"], "wiki_write_burst");
    assert_eq!(body_of(&w, p).await, "v1");
    // The suspension committed: another page is refused too.
    let e = submit(&w, a, &put("WEB", "Other", "x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AgentSuspended);
}

// ---------------------------------------------------------------- T-M2-48

/// T-M2-48: a delegation made before M2.3b holds no wiki op; a wiki write
/// is `forbidden_op` (policy) and the policy page offers the wiki ops as
/// new ones. A new delegation gets them `direct` (A5).
#[tokio::test]
async fn t48_older_delegations_and_the_defaults() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let old = w.delegate(w.web).await.unwrap();
    let (_, a) = w.token(old.context_id).await;
    let e = submit(&w, a, &put("WEB", "X", "x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "policy");
    let v = agents::delegation_policies(&w.pool, &w.alice_ctx, old.id)
        .await
        .unwrap();
    for op in ["wiki_put", "wiki_move"] {
        assert!(v.available_commands.iter().any(|c| c == op), "{op}");
    }

    let n = agent(&w, &[w.auth_p]).await;
    let d: Uuid =
        sqlx::query_scalar("SELECT id FROM delegations WHERE context_id = $1 AND project_id = $2")
            .bind(n.context_id().unwrap())
            .bind(w.auth_p)
            .fetch_one(&pool)
            .await
            .unwrap();
    let v = agents::delegation_policies(&w.pool, &w.alice_ctx, d)
        .await
        .unwrap();
    assert_eq!(v.commands.get("wiki_put"), Some(&Direct));
    assert_eq!(v.commands.get("wiki_move"), Some(&Direct));
    assert!(!v.available_commands.iter().any(|c| c.starts_with("wiki_")));
}

/// Design 06 §4.4: 「文件維護」 holds the wiki ops only, reviewed; no other
/// template holds them.
#[tokio::test]
async fn templates_and_the_wiki_ops() {
    let pool = test_pool().await;
    let _w = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let docs = ai_team::template_spec(&mut conn, "docs").await.unwrap();
    assert_eq!(docs.proposal_ops, ["wiki_put", "wiki_move"]);
    assert_eq!(docs.policies.commands.get("wiki_put"), Some(&Review));
    assert_eq!(docs.policies.commands.get("wiki_move"), Some(&Review));
    assert_eq!(ai_team::default_role_key("docs"), Some("docs"));
    for t in ai_team::TEMPLATES.iter().filter(|t| **t != "docs") {
        let s = ai_team::template_spec(&mut conn, t).await.unwrap();
        assert!(
            !s.proposal_ops.iter().any(|o| o.starts_with("wiki_")),
            "{t}"
        );
    }
}

// ---------------------------------------------------------------- reverts

/// R-AG1 for a revert: it reads an older version, so H(P) ⊇ A(P) is needed
/// (`wiki_history_not_delegable`); with it, the revert writes version 1's
/// body again.
#[tokio::test]
async fn a_revert_needs_the_history_rule() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Doc", "first").await;
    let lv = lock_version(&w, p).await;
    person(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": "WEB", "page_id": p}, "body": "second"}),
        Some(json!({"wiki_page": p, "lock_version": lv})),
    )
    .await;
    let revert = |lv: i64| {
        json!({"command_schema_version": 1,
               "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                        "target": {"project": "WEB", "page_id": p}, "revert_from": 1}],
               "preconditions": [{"wiki_page": p, "lock_version": lv}]})
    };
    let a = agent(&w, &[w.web]).await;
    let r = submit(&w, a, &revert(lock_version(&w, p).await))
        .await
        .unwrap();
    assert_eq!(r["outcome"], "applied");
    assert_eq!(body_of(&w, p).await, "first");

    // Someone who reads the issues and the wiki but not its history.
    let no_history = role(
        &pool,
        "NoHistory",
        "all",
        &["view_issues", "view_wiki_pages"],
    )
    .await;
    let erin = user(&pool, "erin").await;
    member(&pool, w.auth_p, erin, no_history).await;
    let q = page(&w, "AUTH", "Doc", "first").await;
    let lv = lock_version(&w, q).await;
    person(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": "AUTH", "page_id": q}, "body": "second"}),
        Some(json!({"wiki_page": q, "lock_version": lv})),
    )
    .await;
    // erin also reads AUTH's issues: A(AUTH) grows with her, W(AUTH) too,
    // H(AUTH) does not.
    w.new_context().await;
    let b = agent(&w, &[w.auth_p]).await;
    let mut doc = revert(lock_version(&w, q).await);
    doc["ops"][0]["target"]["project"] = json!("AUTH");
    doc["ops"][0]["target"]["page_id"] = json!(q);
    doc["preconditions"][0]["wiki_page"] = json!(q);
    let e = submit(&w, b, &doc).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "wiki_history_not_delegable");
    assert_eq!(body_of(&w, q).await, "second");
}

// ---------------------------------------------------------------- review (T-M2-44, 47a)

async fn approve(
    w: &World,
    cs: Uuid,
    r: &Value,
    mode: Option<&str>,
) -> anyhow::Result<changesets::ApplyOutcome> {
    changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: r["proposal_hash"].as_str().unwrap().to_string(),
            review_mode: mode.map(String::from),
        },
    )
    .await
}

fn cs_of(r: &Value) -> Uuid {
    r["changeset_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no proposal: {r}"))
        .parse()
        .unwrap()
}

/// origin, actor_id, proposer_id, approver_id, on_behalf_of_id,
/// changeset_id, execution_id, command_execution_id.
type ReviewProv = (
    String,
    Uuid,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
);

/// T-M2-44 (review): the policy sends the write for review. The proposal is
/// checked by a trial write that leaves nothing; the owner sees it against
/// the page as it is now (R-W7) and applies it; the version is the agent's
/// for alice, approved by her (`external_agent`). A queue tick is refused
/// (wiki ops are read in full).
#[tokio::test]
async fn t44_review_proposes_and_the_owner_applies() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Guide", "line one\nline two\n").await;
    let a = agent_with(
        &w,
        &[w.web],
        &[("wiki_put", Review), ("wiki_move", Review)],
        false,
    )
    .await;
    let op = Uuid::now_v7();
    let doc = update("WEB", p, lock_version(&w, p).await, "line one\nline 2\n");
    let r = submit_as(&w, a, op, &doc, false).await.unwrap();
    assert_eq!(r["outcome"], "review_required", "{r}");
    assert_eq!(r["reason"], "policy");
    assert_eq!(r["validation"]["status"], "passed", "{r}");
    let cs = cs_of(&r);
    let versions = format!("SELECT count(*) FROM wiki_page_versions WHERE page_id = '{p}'");
    assert_eq!(count(&pool, &versions).await, 1);
    assert_eq!(body_of(&w, p).await, "line one\nline two\n");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM command_executions WHERE origin = 'agent_direct'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM changeset_version_ops WHERE changeset_id = '{cs}' AND kind = 'wiki_put'"
            )
        )
        .await,
        1
    );
    // A resend answers with the same proposal.
    let again = submit_as(&w, a, op, &doc, false).await.unwrap();
    assert_eq!(again["changeset_id"], r["changeset_id"]);
    assert_eq!(again["reason"], "policy");
    // The agent reads its own proposal.
    let g = changesets::agent_get(&w.app, a, cs, None, ORIGIN)
        .await
        .unwrap();
    assert!(g.proposal.is_some());
    assert_eq!(g.content_unavailable, None);

    // The owner: the body (its project by id) and the diff with the page now.
    let v = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    assert_eq!(
        v.proposal.as_ref().unwrap()["ops"][0]["target"]["project"],
        json!(w.web.to_string())
    );
    let d = v.wiki.clone().expect("a wiki diff");
    assert_eq!(d.page_id, Some(p));
    assert_eq!(d.kind, "wiki_put");
    assert!(!d.stale);
    assert!(
        d.hunks.iter().any(|h| h
            .lines
            .iter()
            .any(|l| l.op == "insert" && l.text == "line 2")),
        "{d:?}"
    );
    let val = changesets::validate(&w.app, w.alice_ctx, cs, &ValidateInput { version: 1 })
        .await
        .unwrap();
    assert_eq!(val.status, "passed", "{val:?}");
    assert_eq!(count(&pool, &versions).await, 1);
    let e = approve(&w, cs, &r, Some("queue")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReviewOutdated);

    let out = approve(&w, cs, &r, None).await.unwrap();
    let ex = out.execution.expect("finished");
    assert_eq!(ex.status, "succeeded", "{ex:?}");
    assert_eq!(body_of(&w, p).await, "line one\nline 2\n");
    let agent_id = a.principal_id().unwrap();
    let row: ReviewProv = sqlx::query_as(
        "SELECT origin, actor_id, proposer_id, approver_id, on_behalf_of_id, changeset_id,
                execution_id, command_execution_id
         FROM wiki_page_versions WHERE page_id = $1 AND version = 2",
    )
    .bind(p)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "external_agent".to_string(),
            agent_id,
            Some(agent_id),
            Some(w.alice),
            Some(w.alice),
            Some(cs),
            Some(ex.execution_id),
            None
        )
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM changesets WHERE id = '{cs}' AND state = 'succeeded'")
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM execution_wiki_changes WHERE execution_id = '{}' AND page_id = '{p}'",
                ex.execution_id
            )
        )
        .await,
        1
    );
}

/// 06 §3.12: the page changed after the proposal: the review shows it as
/// stale, and applying it fails with `conflict` (the page keeps the
/// person's text).
#[tokio::test]
async fn a_changed_page_is_a_conflict() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Guide", "one").await;
    let a = agent_with(&w, &[w.web], &[("wiki_put", Review)], false).await;
    let r = submit(&w, a, &update("WEB", p, lock_version(&w, p).await, "agent"))
        .await
        .unwrap();
    let cs = cs_of(&r);
    person(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": "WEB", "page_id": p}, "body": "person"}),
        Some(json!({"wiki_page": p, "lock_version": lock_version(&w, p).await})),
    )
    .await;
    let v = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    assert!(v.wiki.unwrap().stale);
    let val = changesets::validate(&w.app, w.alice_ctx, cs, &ValidateInput { version: 1 })
        .await
        .unwrap();
    assert_eq!(val.status, "conflict", "{val:?}");
    let ex = approve(&w, cs, &r, None).await.unwrap().execution.unwrap();
    assert_eq!(ex.status, "failed");
    assert_eq!(ex.error.unwrap().code, "conflict");
    assert_eq!(body_of(&w, p).await, "person");
    // A new page whose title was taken since: it fails too.
    let r = submit(&w, a, &put("WEB", "Fresh", "agent")).await.unwrap();
    page(&w, "WEB", "Fresh", "person").await;
    let ex = approve(&w, cs_of(&r), &r, None)
        .await
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(ex.status, "failed", "{ex:?}");
}

/// A requested review of a move: applied as `external_agent` events. A
/// wiki proposal is read only by someone who reads that wiki (it has no
/// issue targets to decide it); it is not revised, by the owner or the
/// agent.
#[tokio::test]
async fn moves_reading_and_no_revisions() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Old", "x").await;
    let a = agent(&w, &[w.web]).await;
    let mv = json!({"command_schema_version": 1,
        "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_move",
                 "target": {"project": "WEB", "page_id": p}, "title": "New"}],
        "preconditions": [{"wiki_page": p, "lock_version": lock_version(&w, p).await}]});
    let r = submit_as(&w, a, Uuid::now_v7(), &mv, true).await.unwrap();
    assert_eq!(r["reason"], "requested", "{r}");
    let v = changesets::get(&w.app, w.alice_ctx, cs_of(&r), None)
        .await
        .unwrap();
    let d = v.wiki.unwrap();
    assert_eq!(
        (d.kind.as_str(), d.new_title.as_deref()),
        ("wiki_move", Some("New"))
    );
    let ex = approve(&w, cs_of(&r), &r, None)
        .await
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(ex.status, "succeeded", "{ex:?}");
    let title: String = sqlx::query_scalar("SELECT title FROM wiki_pages WHERE id = $1")
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(title, "New");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM wiki_events WHERE page_id = '{p}' AND kind = 'renamed'
                   AND origin = 'external_agent' AND approver_id = '{}'",
                w.alice
            )
        )
        .await,
        1
    );

    // Not revised: the owner's revision and the agent's.
    let r = submit_as(&w, a, Uuid::now_v7(), &put("WEB", "Draft", "d"), true)
        .await
        .unwrap();
    let cs = cs_of(&r);
    let i = w.issue(w.web, "an issue").await;
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let e = changesets::owner_revise(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &json!({"expected_current_version": 1, "proposal": subject_doc(i, lv, "s")}),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "wiki_proposal", "{e:#}");
    let e = changesets::revise(
        &w.app,
        a,
        Uuid::now_v7(),
        cs,
        1,
        &subject_doc(i, lv, "s"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "wiki_proposal", "{e:#}");

    // alice no longer reads WEB's wiki: the proposal is not hers to read.
    exec(
        &pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'wiki'",
            w.web
        ),
    )
    .await;
    let v = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    assert_eq!(v.proposal, None);
    assert_eq!(v.content_unavailable.as_deref(), Some("not_readable"));
    assert!(v.wiki.is_none());
    let e = changesets::validate(&w.app, w.alice_ctx, cs, &ValidateInput { version: 1 })
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ProposalUnavailable);
    assert!(approve(&w, cs, &r, None).await.is_err());
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE title = 'Draft'"
        )
        .await,
        0
    );
}

/// T-M2-47a (A6 (b), R-AG11): an external agent whose delegation reads the
/// delegator's time sends every wiki write for review, whatever the policy.
#[tokio::test]
async fn t47a_read_own_time_sends_wiki_writes_for_review() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Doc", "x").await;
    let a = agent_with(&w, &[w.web], &[], true).await;
    let r = submit(&w, a, &put("WEB", "New", "x")).await.unwrap();
    assert_eq!(r["outcome"], "review_required", "{r}");
    assert_eq!(r["reason"], "time_read_delegated");
    let mv = json!({"command_schema_version": 1,
        "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_move",
                 "target": {"project": "WEB", "page_id": p}, "title": "Moved"}]});
    let r = submit(&w, a, &mv).await.unwrap();
    assert_eq!(r["reason"], "time_read_delegated", "{r}");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE title IN ('New', 'Moved')"
        )
        .await,
        0
    );
}

/// 02 §6.7 at the approval of a wiki proposal (independent verification
/// U1): an op the delegator turned off after the proposal is not applied;
/// the agent's own trial (changeset_validate) passes and leaves nothing;
/// the execution names the page it wrote.
#[tokio::test]
async fn approval_checks_the_policy_again() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let a = agent_with(&w, &[w.web], &[("wiki_put", Review)], false).await;
    let r = submit(&w, a, &put("WEB", "Later", "l")).await.unwrap();
    let cs = cs_of(&r);
    let v = changesets::agent_validate(&w.app, a, cs, 1).await.unwrap();
    assert_eq!(v.validation.unwrap().status, "passed");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE title = 'Later'"
        )
        .await,
        0
    );
    exec(
        &pool,
        &format!(
            "UPDATE delegation_command_policies SET mode = 'off'
             WHERE command_kind = 'wiki_put' AND delegation_id =
                   (SELECT delegation_id FROM changesets WHERE id = '{cs}')"
        ),
    )
    .await;
    let ex = approve(&w, cs, &r, None).await.unwrap().execution.unwrap();
    assert_eq!(ex.status, "failed", "{ex:?}");
    assert_eq!(ex.error.unwrap().code, "forbidden_op");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE title = 'Later'"
        )
        .await,
        0
    );

    exec(
        &pool,
        "UPDATE delegation_command_policies SET mode = 'review' WHERE command_kind = 'wiki_put'",
    )
    .await;
    let r = submit(&w, a, &put("WEB", "Now", "n")).await.unwrap();
    let ex = approve(&w, cs_of(&r), &r, None)
        .await
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(ex.status, "succeeded", "{ex:?}");
    let page: Uuid = sqlx::query_scalar("SELECT id FROM wiki_pages WHERE title = 'Now'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ex.items[0].wiki_page_id, Some(page), "{ex:?}");
}

/// A revert proposal's diff shows the older version only to an owner who
/// reads the history now (independent verification: R-W8 / need_history).
#[tokio::test]
async fn a_revert_diff_needs_the_history() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let p = page(&w, "WEB", "Doc", "secret first").await;
    person(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": "WEB", "page_id": p}, "body": "second"}),
        Some(json!({"wiki_page": p, "lock_version": lock_version(&w, p).await})),
    )
    .await;
    let a = agent(&w, &[w.web]).await;
    let doc = json!({"command_schema_version": 1,
        "ops": [{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                 "target": {"project": "WEB", "page_id": p}, "revert_from": 1}],
        "preconditions": [{"wiki_page": p, "lock_version": lock_version(&w, p).await}]});
    let r = submit_as(&w, a, Uuid::now_v7(), &doc, true).await.unwrap();
    let cs = cs_of(&r);
    let d = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap()
        .wiki
        .unwrap();
    assert!(!d.history_hidden);
    assert!(format!("{:?}", d.hunks).contains("secret first"), "{d:?}");
    exec(
        &pool,
        &format!(
            "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'view_wiki_edits'",
            w.dev
        ),
    )
    .await;
    let d = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap()
        .wiki
        .unwrap();
    assert!(d.history_hidden);
    assert!(d.hunks.is_empty(), "{d:?}");
}
