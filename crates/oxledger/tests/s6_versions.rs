// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6.8: agent version commands (§17.9a) through
//! `command_submit`: the document rules, permission and policy, the audience
//! (public destination, M8), attribution and undo copies, the review path,
//! name collisions, replays, and the version quotas and breaker.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput, PolicyInput};
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands;
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::quota::limits;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

async fn submit(w: &World, agent: AuthContext, doc: &Value) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), doc, false, ORIGIN).await
}

fn create(project: Uuid, fields: Value) -> Value {
    json!({"command_schema_version": 1, "title": "milestone",
           "ops": [{"op_id": op(), "kind": "create_version", "project_id": project, "fields": fields}]})
}

fn update(v: Uuid, lv: i32, set: Value) -> Value {
    json!({"command_schema_version": 1, "title": "milestone",
           "ops": [{"op_id": op(), "kind": "update_version", "target": {"type": "version", "id": v}, "set": set}],
           "preconditions": [{"target": {"type": "version", "id": v}, "lock_version": lv}]})
}

fn version_effect(r: &SubmitResult) -> commands::VersionEffect {
    match r {
        SubmitResult::Applied {
            versions, effects, ..
        } => {
            assert!(effects.is_empty());
            assert_eq!(versions.len(), 1, "{r:?}");
            versions[0].clone()
        }
        other => panic!("expected applied, got {other:?}"),
    }
}

async fn vlv(w: &World, v: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
        .bind(v)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn modes(c: &[(&str, PolicyMode)]) -> PolicyInput {
    PolicyInput {
        commands: c
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
        fields: BTreeMap::new(),
    }
}

async fn delegate_versions(w: &World, project: Uuid) -> agents::DelegationView {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: project,
            proposal_ops: ["add_note", "create_version", "update_version"]
                .map(String::from)
                .to_vec(),
            writable_fields: vec![],
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

#[tokio::test]
async fn s6_versions() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let ctx = w.new_context().await;
    let d = delegate_versions(&w, w.web).await;
    let (_, agent) = w.token(ctx).await;
    // §17.9a: a new delegation of a delegator with manage_versions gets
    // direct rows for both commands.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT command_kind, mode::text FROM delegation_command_policies
         WHERE delegation_id = $1 AND command_kind LIKE '%version' ORDER BY 1",
    )
    .bind(d.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("create_version".to_string(), "direct".to_string()),
            ("update_version".to_string(), "direct".to_string())
        ]
    );

    // ------------------------------------------------ document rules
    let note = json!({"op_id": op(), "kind": "add_note", "target": {"type": "issue", "id": Uuid::now_v7()}, "body": "x"});
    let mut mixed = create(w.web, json!({"name": "v1"}));
    mixed["ops"].as_array_mut().unwrap().push(note);
    for (doc, want) in [
        (mixed, ErrorCode::InvalidRequest),
        (create(w.web, json!({})), ErrorCode::RequiredFieldMissing),
        (
            create(w.web, json!({"name": "v1", "status": "locked"})),
            ErrorCode::InvalidRequest,
        ),
        (
            create(w.web, json!({"name": "v1", "sharing": "system"})),
            ErrorCode::InvalidRequest,
        ),
        (
            {
                let mut d = create(w.web, json!({"name": "v1"}));
                d["preconditions"] = json!([{"target": {"type": "version", "id": Uuid::now_v7()}, "lock_version": 0}]);
                d
            },
            ErrorCode::InvalidRequest,
        ),
        (
            {
                let mut d = update(Uuid::now_v7(), 0, json!({"name": "x"}));
                d["preconditions"] = json!([]);
                d
            },
            ErrorCode::InvalidRequest,
        ),
        (
            update(Uuid::now_v7(), 0, json!({"deleted_at": "2026-01-01"})),
            ErrorCode::InvalidRequest,
        ),
        (
            update(Uuid::now_v7(), 0, json!({"name": "x"})),
            ErrorCode::NotFound,
        ),
        (
            create(w.web, json!({"name": "v1", "effective_date": "2300-01-01"})),
            ErrorCode::InvalidValue,
        ),
        (
            create(w.web, json!({"name": "  "})),
            ErrorCode::InvalidValue,
        ),
    ] {
        let e = submit(&w, agent, &doc).await.unwrap_err();
        assert_eq!(code(&e), want, "{doc} → {e:#}");
    }
    // A project outside the delegation looks missing.
    assert_eq!(
        code(
            &submit(&w, agent, &create(w.auth_p, json!({"name": "v1"})))
                .await
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    // People write versions through /versions.
    let e = commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &create(w.web, json!({"name": "v1"})),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UnsupportedOperation);

    // ------------------------------------------------ create (direct)
    let key = Uuid::now_v7();
    let doc = create(
        w.web,
        json!({"name": "v2.1", "effective_date": "2026-12-18", "description": "next"}),
    );
    let r = changesets::command_submit(&w.app, agent, key, &doc, false, ORIGIN)
        .await
        .unwrap();
    let e1 = version_effect(&r);
    assert!(e1.created && e1.changed);
    assert_eq!((e1.project_id, e1.lock_version), (w.web, 0));
    let v = e1.version_id;
    // A replay returns the same result and writes nothing.
    let again = changesets::command_submit(&w.app, agent, key, &doc, false, ORIGIN)
        .await
        .unwrap();
    assert_eq!(version_effect(&again), e1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM versions").await,
        1,
        "one version"
    );
    // Attribution (§17.9a, §16.2).
    let (by, obo, status): (Uuid, Option<Uuid>, String) = sqlx::query_as(
        "SELECT created_by, created_on_behalf_of_id, status FROM versions WHERE id = $1",
    )
    .bind(v)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (by, obo, status.as_str()),
        (d.agent_principal_id, Some(w.alice), "open")
    );
    let ev: (
        String,
        String,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT origin, actor_kind, actor_id, on_behalf_of_id, delegation_id, approver_id
         FROM version_events WHERE version_id = $1 AND seq = 1",
    )
    .bind(v)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ev,
        (
            "agent_direct".into(),
            "agent".into(),
            d.agent_principal_id,
            Some(w.alice),
            Some(d.id),
            None
        )
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM execution_version_changes WHERE version_id = '{v}' AND field_key = '_created'"
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM version_field_origins WHERE version_id = '{v}'")
        )
        .await,
        4
    );
    let origin: String = sqlx::query_scalar(
        "SELECT ce.origin FROM command_executions ce JOIN version_events e ON e.execution_id = ce.id
         WHERE e.version_id = $1",
    )
    .bind(v)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(origin, "agent_direct");
    // R8: the same name (casefolded) is taken; no automatic suffix.
    let e = submit(&w, agent, &create(w.web, json!({"name": "V2.1"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::VersionNameTaken);

    // ------------------------------------------------ update (direct)
    let r = submit(
        &w,
        agent,
        &update(
            v,
            0,
            json!({"effective_date": "2026-11-27", "status": "locked"}),
        ),
    )
    .await
    .unwrap();
    let e2 = version_effect(&r);
    assert!(!e2.created && e2.changed);
    assert_eq!(e2.lock_version, 1);
    assert_eq!(vlv(&w, v).await, 1);
    let changed: Vec<(String, Value, Value)> = sqlx::query_as(
        "SELECT c.field_key, c.before_value, c.after_value FROM execution_version_changes c
         JOIN version_events e ON e.execution_id = c.execution_id
         WHERE e.version_id = $1 AND e.seq = 2 ORDER BY 1",
    )
    .bind(v)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        changed,
        [
            (
                "effective_date".to_string(),
                json!("2026-12-18"),
                json!("2026-11-27")
            ),
            ("status".to_string(), json!("open"), json!("locked")),
        ]
    );
    // A stale lock_version.
    let e = submit(&w, agent, &update(v, 0, json!({"name": "x"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // Nothing to change: no_effect, lock_version kept.
    let r = submit(&w, agent, &update(v, 1, json!({"status": "locked"})))
        .await
        .unwrap();
    assert!(!version_effect(&r).changed);
    assert_eq!(vlv(&w, v).await, 1);

    // ------------------------------------------------ policy: review, then off
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("create_version", Review)]),
    )
    .await
    .unwrap();
    let r = submit(&w, agent, &create(w.web, json!({"name": "v3"})))
        .await
        .unwrap();
    let SubmitResult::ReviewRequired { reason, proposal } = r else {
        panic!("expected review_required: {r:?}")
    };
    assert_eq!(reason, "policy");
    let view = changesets::get(&w.app, w.alice_ctx, proposal.changeset_id, None)
        .await
        .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        proposal.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: view.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    let ex = o.execution.unwrap();
    assert!(ex.error.is_none(), "{ex:?}");
    let (v3, origin, approver): (Uuid, String, Option<Uuid>) = sqlx::query_as(
        "SELECT v.id, e.origin, e.approver_id FROM versions v JOIN version_events e ON e.version_id = v.id
         WHERE v.name = 'v3'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (origin.as_str(), approver),
        ("external_agent", Some(w.alice))
    );
    let by: Uuid = sqlx::query_scalar("SELECT created_by FROM versions WHERE id = $1")
        .bind(v3)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(by, d.agent_principal_id);
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("create_version", Off), ("update_version", Direct)]),
    )
    .await
    .unwrap();
    let e = submit(&w, agent, &create(w.web, json!({"name": "v4"})))
        .await
        .unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(
        (
            a.code,
            a.details.unwrap()["reason"].as_str().map(String::from)
        ),
        (ErrorCode::ForbiddenOp, Some("policy".into()))
    );
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        modes(&[("create_version", Direct)]),
    )
    .await
    .unwrap();

    // ------------------------------------------------ quotas and the breaker
    exec(&pool, "DELETE FROM agent_write_log").await;
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind)
         SELECT $1, $2, 'version_create' FROM generate_series(1, $3)",
    )
    .bind(d.agent_principal_id)
    .bind(ctx)
    .bind(limits::VERSION_CREATES_PER_HOUR as i32)
    .execute(&pool)
    .await
    .unwrap();
    let e = submit(&w, agent, &create(w.web, json!({"name": "v5"})))
        .await
        .unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(
        (
            a.code,
            a.details.unwrap()["kind"].as_str().map(String::from)
        ),
        (
            ErrorCode::QuotaExceeded,
            Some("version_creates_per_hour".into())
        )
    );
    exec(&pool, "DELETE FROM agent_write_log").await;
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind, version_id)
         SELECT $1, $2, 'version_write', $3 FROM generate_series(1, $4)",
    )
    .bind(d.agent_principal_id)
    .bind(ctx)
    .bind(v)
    .bind(limits::VERSION_WRITES_PER_10_MIN as i32)
    .execute(&pool)
    .await
    .unwrap();
    let e = submit(&w, agent, &update(v, 1, json!({"name": "v2.1b"})))
        .await
        .unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(a.code, ErrorCode::AgentSuspended, "{e:#}");
    assert_eq!(a.details.unwrap()["reason"], "version_write_burst");
    agents::resume_context(&pool, &w.policy, &w.alice_ctx, ctx)
        .await
        .unwrap();
    exec(&pool, "DELETE FROM agent_write_log").await;
    // Counted after a successful write.
    submit(&w, agent, &update(v, 1, json!({"name": "v2.1b"})))
        .await
        .unwrap();
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_write_log WHERE version_id = '{v}' AND kind = 'version_write'"
            )
        )
        .await,
        1
    );

    // ------------------------------------------------ audience: a public destination (M8)
    // R = {WEB, AUTH}: carol reads WEB but not AUTH, so a WEB milestone would
    // show content read in AUTH to her.
    w.delegate(w.auth_p).await.unwrap();
    let e = submit(&w, agent, &create(w.web, json!({"name": "leak"})))
        .await
        .unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(a.code, ErrorCode::AudienceExpansionForbidden);
    assert_eq!(a.details.unwrap()["kind"], "cross_project");
    faults::clear();
}
