// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6 (D20): delegation policies and `command_submit` (§16.3–§16.5).
//! T-D1, T-D2, T-D3, T-D4, T-D10, T-D11, attribution, I-2.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput, PolicyInput};
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::error::ErrorCode;
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::service;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

async fn lv(w: &World, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn submit(
    w: &World,
    agent: AuthContext,
    op: Uuid,
    doc: &Value,
) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, agent, op, doc, false, ORIGIN).await
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "agent", "ops": ops, "preconditions": pre})
}

fn create(w: &World, project: Uuid, fields: Value) -> Value {
    let mut f = fields;
    f["subject"] = f.get("subject").cloned().unwrap_or(json!("new"));
    doc(
        vec![
            json!({"op_id": op(), "kind": "create_issue", "project_id": project, "tracker_id": w.bug, "fields": f}),
        ],
        vec![],
    )
}

fn applied(r: &SubmitResult) -> Uuid {
    match r {
        SubmitResult::Applied { execution_id, .. } => *execution_id,
        other => panic!("expected applied, got {other:?}"),
    }
}

fn review(r: &SubmitResult) -> (Uuid, String) {
    match r {
        SubmitResult::ReviewRequired { reason, proposal } => {
            (proposal.changeset_id, reason.clone())
        }
        other => panic!("expected review_required, got {other:?}"),
    }
}

fn modes(c: &[(&str, PolicyMode)], f: &[(&str, PolicyMode)]) -> PolicyInput {
    PolicyInput {
        commands: c
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
        fields: f
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[tokio::test]
async fn s6_direct() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let web = w.web;
    let i1 = w.issue(web, "one").await;
    let i2 = w.issue(web, "two").await;
    let i3 = w.issue(web, "three").await;

    // ------------------------------------------------ policy rows (§16.3 rule 1)
    let d = w.delegate(web).await.unwrap();
    let p = agents::delegation_policies(&pool, &w.alice_ctx, d.id)
        .await
        .unwrap();
    assert_eq!(p.policy_version, 1);
    assert_eq!(
        p.commands.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "add_note",
            "create_issue",
            "duplicate_of",
            "transition",
            "update_fields"
        ]
    );
    assert!(p.commands.values().all(|m| *m == Direct));
    assert_eq!(
        p.fields.keys().map(String::as_str).collect::<Vec<_>>(),
        ["assigned_to_id", "description", "subject"]
    );
    assert_eq!(
        p.available_commands,
        [
            "create_version",
            "update_version",
            "triage_decide",
            "log_time",
            "update_time_entry"
        ]
    );
    // status_id is not a field (a status change is the transition command).
    let e = agents::create_delegation(
        &pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.auth_p,
            proposal_ops: vec!["update_fields".into()],
            writable_fields: vec!["status_id".into()],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    // Policies outside the ceiling are refused.
    let e = agents::create_delegation(
        &pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.auth_p,
            proposal_ops: vec!["update_fields".into()],
            writable_fields: vec!["subject".into()],
            max_ops: None,
            expires_in_days: None,
            policies: Some(modes(&[("add_note", Review)], &[])),
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);

    let (_, agent) = w.token(d.context_id).await;

    // ------------------------------------------------ T-D1: direct, attribution
    let r = submit(
        &w,
        agent,
        Uuid::now_v7(),
        &subject_doc(i1, lv(&w, i1).await, "direct"),
    )
    .await
    .unwrap();
    let x = applied(&r);
    let SubmitResult::Applied {
        effects,
        search,
        next_actions,
        ..
    } = &r
    else {
        unreachable!()
    };
    assert_eq!(effects.len(), 1);
    assert!(effects[0].changed && effects[0].lock_version == 1);
    assert!(search.consistent_after.starts_with("c1."));
    assert!(next_actions.is_empty());
    let (subject, ver): (String, i32) =
        sqlx::query_as("SELECT subject, lock_version FROM issues WHERE id = $1")
            .bind(i1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((subject.as_str(), ver), ("direct", 1));
    let agent_id = d.agent_principal_id;
    let ce: (Uuid, Option<Uuid>, String, Option<Uuid>, Option<i64>) = sqlx::query_as(
        "SELECT actor_id, on_behalf_of_id, origin, delegation_id, policy_version FROM command_executions WHERE id = $1",
    )
    .bind(x)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ce,
        (
            agent_id,
            Some(w.alice),
            "agent_direct".into(),
            Some(d.id),
            Some(1)
        )
    );
    let ev: (
        String,
        String,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT origin, actor_kind, actor_id, on_behalf_of_id, command_execution_id, approver_id
         FROM issue_events WHERE issue_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(i1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ev,
        (
            "agent_direct".into(),
            "agent".into(),
            agent_id,
            Some(w.alice),
            Some(x),
            None
        )
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM command_execution_source_projects WHERE execution_id = '{x}'"
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM execution_field_changes WHERE execution_id = '{x}'")
        )
        .await,
        1,
        "before/after kept for undo (§16.7)"
    );
    // The handle (§16.2).
    let handle: String =
        sqlx::query_scalar("SELECT handle FROM agent_principals WHERE agent_principal_id = $1")
            .bind(agent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(handle, "claude-code@alice");
    assert_eq!(
        agents::handle_base("  Claude  Code!! ", "bob"),
        "claude-code@bob"
    );
    assert_eq!(agents::handle_base("助理", "bob"), "agent@bob");

    // Idempotency: the same key replays without a second write.
    let key = Uuid::now_v7();
    let dd = note_doc(i1, lv(&w, i1).await, "once");
    let a = applied(&submit(&w, agent, key, &dd).await.unwrap());
    let b = applied(&submit(&w, agent, key, &dd).await.unwrap());
    assert_eq!(a, b);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i1}'")
        )
        .await,
        1
    );
    // Same key, other content.
    let e = submit(&w, agent, key, &note_doc(i1, lv(&w, i1).await, "twice"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::IdempotencyKeyReused);
    // require_review = true → a ChangeSet even though everything is direct.
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(i2, lv(&w, i2).await, "asked"),
        true,
        ORIGIN,
    )
    .await
    .unwrap();
    assert_eq!(review(&r).1, "requested");
    // changeset_propose is command_submit(require_review = true).
    changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(i2, lv(&w, i2).await, "p"),
        ORIGIN,
    )
    .await
    .unwrap();
    // A conflict is an error, never a hidden proposal (§16.5).
    let before = count(&pool, "SELECT count(*) FROM changesets").await;
    let e = submit(&w, agent, Uuid::now_v7(), &subject_doc(i1, 0, "stale"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changesets").await,
        before
    );

    // ------------------------------------------------ T-D1: review and off per command
    let v = agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("add_note", Review), ("transition", Off)], &[]),
    )
    .await
    .unwrap();
    assert_eq!(v.policy_version, 2);
    let r = submit(
        &w,
        agent,
        Uuid::now_v7(),
        &note_doc(i2, lv(&w, i2).await, "please review"),
    )
    .await
    .unwrap();
    let (cs, reason) = review(&r);
    assert_eq!(reason, "policy");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i2}'")
        )
        .await,
        0
    );
    let tr = doc(
        vec![
            json!({"op_id": op(), "kind": "transition", "target": tgt(i3), "to_status_id": w.doing}),
        ],
        vec![json!({"target": tgt(i3), "lock_version": lv(&w, i3).await})],
    );
    let e = submit(&w, agent, Uuid::now_v7(), &tr).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // Off also refuses proposing (no hidden way around it).
    let e = changesets::propose(&w.app, agent, Uuid::now_v7(), &tr, ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // A mixed document: anything review → the whole document is reviewed.
    let mixed = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(i2), "set": {"subject": "mixed"}}),
            json!({"op_id": op(), "kind": "add_note", "target": tgt(i2), "body": "and a note"}),
        ],
        vec![json!({"target": tgt(i2), "lock_version": lv(&w, i2).await})],
    );
    assert_eq!(
        review(&submit(&w, agent, Uuid::now_v7(), &mixed).await.unwrap()).1,
        "policy"
    );
    // Turned off after proposing: the approved version is not applied.
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("add_note", Off)], &[]),
    )
    .await
    .unwrap();
    let view = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: view.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await;
    let err = match o {
        Ok(o) => o.execution.unwrap().error.unwrap().code,
        Err(e) => code(&e).as_str().to_string(),
    };
    assert_eq!(err, "forbidden_op");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i2}'")
        )
        .await,
        0
    );

    // ------------------------------------------------ T-D2: field review applies to create
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[], &[("description", Review)]),
    )
    .await
    .unwrap();
    let r = submit(
        &w,
        agent,
        Uuid::now_v7(),
        &create(&w, web, json!({"description": "via create"})),
    )
    .await
    .unwrap();
    assert_eq!(review(&r).1, "policy");
    // Without the field the create is direct (defaults do not count).
    applied(
        &submit(&w, agent, Uuid::now_v7(), &create(&w, web, json!({})))
            .await
            .unwrap(),
    );
    // A field outside the policy rows is off.
    let e = submit(
        &w,
        agent,
        Uuid::now_v7(),
        &create(&w, web, json!({"due_date": "2026-12-01"})),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");

    // ------------------------------------------------ T-D3: review → direct, same key
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[], &[("subject", Review)]),
    )
    .await
    .unwrap();
    let key = Uuid::now_v7();
    let dd = subject_doc(i3, lv(&w, i3).await, "reviewed first");
    let (cs3, _) = review(&submit(&w, agent, key, &dd).await.unwrap());
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[], &[("subject", Direct)]),
    )
    .await
    .unwrap();
    let r = submit(&w, agent, key, &dd).await.unwrap();
    assert_eq!(
        review(&r),
        (cs3, "policy".to_string()),
        "the original answer, not a write"
    );
    let s: String = sqlx::query_scalar("SELECT subject FROM issues WHERE id = $1")
        .bind(i3)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(s, "three");

    // ------------------------------------------------ T-D11: no actor fields
    for k in ["actor", "on_behalf_of", "author_id", "approved"] {
        let mut dd = subject_doc(i3, lv(&w, i3).await, "x");
        dd[k] = json!(w.bob);
        let e = submit(&w, agent, Uuid::now_v7(), &dd).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "{k}: {e:#}");
        let mut dd = subject_doc(i3, lv(&w, i3).await, "x");
        dd["ops"][0][k] = json!(w.bob);
        let e = submit(&w, agent, Uuid::now_v7(), &dd).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "op {k}: {e:#}");
    }
    let mut dd = create(&w, web, json!({}));
    dd["ops"][0]["fields"]["author_id"] = json!(w.bob);
    assert!(submit(&w, agent, Uuid::now_v7(), &dd).await.is_err());

    // ------------------------------------------------ T-D10: private writes the agent cannot read
    let r = submit(
        &w,
        agent,
        Uuid::now_v7(),
        &create(&w, web, json!({"subject": "secret", "is_private": true})),
    )
    .await
    .unwrap();
    let x = applied(&r);
    let SubmitResult::Applied { effects, .. } = &r else {
        unreachable!()
    };
    assert!(
        effects.is_empty(),
        "the agent cannot read the new private issue"
    );
    let (pid, by): (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT i.id, i.created_on_behalf_of_id FROM issues i JOIN issue_events e ON e.issue_id = i.id
         WHERE e.command_execution_id = $1",
    )
    .bind(x)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(by, Some(w.alice));
    let key_p: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        code(&service::get(&w.app, agent, &key_p).await.unwrap_err()),
        ErrorCode::NotFound
    );
    service::get(&w.app, w.alice_ctx, &key_p).await.unwrap();
    // A private note: written, then invisible to the agent; the delegator sees it.
    let pn = doc(
        vec![
            json!({"op_id": op(), "kind": "add_note", "target": tgt(i1), "body": "private by agent", "is_private": true}),
        ],
        vec![json!({"target": tgt(i1), "lock_version": lv(&w, i1).await})],
    );
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("add_note", Direct)], &[]),
    )
    .await
    .unwrap();
    applied(&submit(&w, agent, Uuid::now_v7(), &pn).await.unwrap());
    let (author, on_behalf): (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT author_id, created_on_behalf_of_id FROM issue_notes WHERE issue_id = $1 AND is_private",
    )
    .bind(i1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((author, on_behalf), (agent_id, Some(w.alice)));

    // ------------------------------------------------ I-2: policy cannot be bypassed via another tool
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("update_fields", Off)], &[]),
    )
    .await
    .unwrap();
    for require_review in [false, true] {
        let e = changesets::command_submit(
            &w.app,
            agent,
            Uuid::now_v7(),
            &subject_doc(i1, lv(&w, i1).await, "no"),
            require_review,
            ORIGIN,
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    }
    // A person (not an agent) cannot use command_submit.
    let e = submit(
        &w,
        w.alice_ctx,
        Uuid::now_v7(),
        &subject_doc(i1, lv(&w, i1).await, "no"),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // ------------------------------------------------ T-D4: two contexts create in one project at once
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("update_fields", Direct)], &[]),
    )
    .await
    .unwrap();
    let bob_ctx = sign_in(&pool, &w.policy, "bob").await;
    let db_ = agents::create_delegation(
        &pool,
        &w.policy,
        &bob_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: web,
            proposal_ops: vec!["create_issue".into()],
            writable_fields: vec!["subject".into()],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    let bob_agent = {
        let c = agents::issue_credential(&pool, &w.policy, &bob_ctx, db_.context_id, "t", None)
            .await
            .unwrap();
        oxledger::auth::authenticate_bearer(&pool, &c.token)
            .await
            .unwrap()
            .unwrap()
    };
    let before: i64 = count(
        &pool,
        &format!("SELECT count(*) FROM issues WHERE project_id = '{web}'"),
    )
    .await;
    // One write per context at a time (§16.6), so each context writes in
    // turn while the two contexts race each other.
    let mut tasks = Vec::new();
    for a in [agent, bob_agent] {
        let app = w.app.clone();
        let docs: Vec<Value> = (0..3)
            .map(|n| create(&w, web, json!({"subject": format!("race {n}")})))
            .collect();
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for dd in docs {
                out.push(
                    changesets::command_submit(&app, a, Uuid::now_v7(), &dd, false, ORIGIN).await,
                );
            }
            out
        }));
    }
    for t in tasks {
        for r in t.await.unwrap() {
            applied(&r.unwrap());
        }
    }
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issues WHERE project_id = '{web}'")
        )
        .await,
        before + 6
    );
    assert_eq!(
        count(&pool, &format!(
            "SELECT count(*) - count(DISTINCT number) FROM issue_keys WHERE project_id = '{web}'"
        ))
        .await,
        0,
        "keys are unique"
    );
    let nums: Vec<i32> =
        sqlx::query_scalar("SELECT number FROM issue_keys WHERE project_id = $1 ORDER BY number")
            .bind(web)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        nums.windows(2).all(|p| p[1] == p[0] + 1),
        "consecutive: {nums:?}"
    );
    faults::clear();
}
