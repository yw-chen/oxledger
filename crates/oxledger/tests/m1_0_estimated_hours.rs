// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.0: `estimated_hours` through the command engine, describe,
//! events, reads, workflow rules, D20 policies (02 §16.7b T-D1, T-D2, T-D3),
//! undo (T-D6, T-D7) and D12 derived copies (T-D9).
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, PolicyInput};
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands;
use oxledger::describe;
use oxledger::error::ErrorCode;
use oxledger::issue_view::{self, GetParams};
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::review_queue;
use oxledger::service;
use oxledger::undo::{self, UndoInput};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// The stored value, as the engine reads it.
async fn hours(w: &World, i: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT trim_scale(estimated_hours)::text FROM issues WHERE id = $1")
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

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "hours", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

fn set(i: Uuid, v: Value) -> Value {
    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"estimated_hours": v}})
}

async fn set_doc(w: &World, i: Uuid, v: Value) -> Value {
    doc(vec![set(i, v)], vec![(i, lv(w, i).await)])
}

async fn human(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<commands::CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

async fn agent_submit(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN).await
}

fn applied(r: &SubmitResult) -> Uuid {
    match r {
        SubmitResult::Applied { execution_id, .. } => *execution_id,
        other => panic!("not applied: {other:?}"),
    }
}

fn reviewed(r: &SubmitResult) -> (Uuid, String, String) {
    match r {
        SubmitResult::ReviewRequired { reason, proposal } => (
            proposal.changeset_id,
            reason.clone(),
            proposal.proposal_hash.clone(),
        ),
        other => panic!("expected review_required, got {other:?}"),
    }
}

fn detail(e: &anyhow::Error) -> Value {
    oxledger::error::classify(e)
        .and_then(|a| a.details)
        .unwrap_or(Value::Null)
}

fn fields(m: &[(&str, PolicyMode)]) -> PolicyInput {
    PolicyInput {
        commands: BTreeMap::new(),
        fields: m.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
    }
}

/// The latest event of an issue: (kind, payload).
async fn last_event(w: &World, i: Uuid) -> (String, Value) {
    sqlx::query_as(
        "SELECT kind, payload FROM issue_events WHERE issue_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(i)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn m1_0_estimated_hours() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    assert!(oxledger::api::COMMAND_FEATURES.contains(&"estimated_hours"));

    // ------------------------------------------------ create, canonical form, events, reads
    let r = human(
        &w,
        a,
        &doc(
            vec![json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "estimate me", "estimated_hours": "1.50"}})],
            vec![],
        ),
    )
    .await
    .unwrap();
    let i1 = r.effects[0].issue_id;
    let k1 = r.effects[0].key.clone();
    assert_eq!(hours(&w, i1).await.as_deref(), Some("1.5"));
    let (ev_kind, payload) = last_event(&w, i1).await;
    assert_eq!(ev_kind, "created");
    assert_eq!(
        payload["fields"]["estimated_hours"],
        json!("1.5"),
        "{payload}"
    );
    let d = service::get(&w.app, a, &k1).await.unwrap();
    assert_eq!(d.estimated_hours.as_deref(), Some("1.5"));
    let g = issue_view::get(
        &w.app,
        a,
        serde_json::from_value::<GetParams>(json!({"key": k1, "fields": ["estimated_hours"]}))
            .unwrap(),
    )
    .await
    .unwrap();
    let g = serde_json::to_value(&g).unwrap();
    assert_eq!(g["estimated_hours"], json!("1.5"), "{g}");
    // The full detail through issue_view too.
    let g = issue_view::get(
        &w.app,
        a,
        serde_json::from_value::<GetParams>(json!({"key": k1})).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(&g).unwrap()["estimated_hours"],
        json!("1.5")
    );

    // update, no_effect on an equal value, clear
    let r = human(&w, a, &set_doc(&w, i1, json!("2.25")).await)
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "applied");
    let (ev_kind, payload) = last_event(&w, i1).await;
    assert_eq!(ev_kind, "fields_changed");
    assert_eq!(
        payload["changes"]["estimated_hours"],
        json!({"old": "1.5", "new": "2.25"})
    );
    let r = human(&w, a, &set_doc(&w, i1, json!("2.250")).await)
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "no_effect");
    let r = human(&w, a, &set_doc(&w, i1, Value::Null).await)
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "applied");
    assert_eq!(hours(&w, i1).await, None);
    assert_eq!(
        last_event(&w, i1).await.1["changes"]["estimated_hours"],
        json!({"old": "2.25", "new": null})
    );
    // Edges: 0 and the maximum.
    human(&w, a, &set_doc(&w, i1, json!("0")).await)
        .await
        .unwrap();
    assert_eq!(hours(&w, i1).await.as_deref(), Some("0"));
    human(&w, a, &set_doc(&w, i1, json!("99999.99")).await)
        .await
        .unwrap();
    assert_eq!(hours(&w, i1).await.as_deref(), Some("99999.99"));

    // Zeros that do not count, beyond canonical_number's length limits.
    human(&w, a, &set_doc(&w, i1, json!("1.00000000000")).await)
        .await
        .unwrap();
    assert_eq!(hours(&w, i1).await.as_deref(), Some("1"));
    human(
        &w,
        a,
        &set_doc(&w, i1, json!("000000000000000000002")).await,
    )
    .await
    .unwrap();
    assert_eq!(hours(&w, i1).await.as_deref(), Some("2"));
    let r = human(
        &w,
        a,
        &set_doc(&w, i1, json!("000000000000000000002.000000000000")).await,
    )
    .await
    .unwrap();
    assert_eq!(r.items[0].status, "no_effect");

    // invalid values: nothing written
    let before = lv(&w, i1).await;
    for (v, code_, k) in [
        (json!("1.255"), ErrorCode::InvalidValue, "too_many_decimals"),
        (
            json!("1.123456789012"),
            ErrorCode::InvalidValue,
            "too_many_decimals",
        ),
        (
            json!("123456789012345678901"),
            ErrorCode::InvalidValue,
            "out_of_range",
        ),
        (json!("-1"), ErrorCode::InvalidValue, "out_of_range"),
        (json!("100000"), ErrorCode::InvalidValue, "out_of_range"),
        (json!(1.5), ErrorCode::InvalidRequest, ""),
        (json!(""), ErrorCode::InvalidRequest, ""),
        (json!("1h30m"), ErrorCode::InvalidRequest, ""),
        (json!("1e2"), ErrorCode::InvalidRequest, ""),
    ] {
        let e = human(&w, a, &set_doc(&w, i1, v.clone()).await)
            .await
            .unwrap_err();
        assert_eq!(code(&e), code_, "{v}: {e:#}");
        assert_eq!(kind(&e), k, "{v}");
    }
    assert_eq!(lv(&w, i1).await, before);

    // ------------------------------------------------ describe
    let s = describe::tracker_schema(&w.app, a, "WEB", "Bug")
        .await
        .unwrap();
    let f = s
        .fields
        .iter()
        .find(|f| f.key == "estimated_hours")
        .expect("estimated_hours in the schema");
    assert_eq!(f.field_type, "decimal");
    assert_eq!(
        f.constraints,
        Some(json!({"min": "0", "max": "99999.99", "max_decimals": 2}))
    );
    assert_eq!(f.ops, ["create", "update"]);
    let acts = describe::issue_actions(&w.app, a, &k1).await.unwrap();
    let fa = acts
        .fields
        .iter()
        .find(|f| f.key == "estimated_hours")
        .expect("estimated_hours in the actions");
    assert!(fa.editable && !fa.required, "{fa:?}");

    // ------------------------------------------------ workflow field rules
    let i2 = w.issue(w.web, "rules").await;
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_field_rules VALUES ('{}','{}','{}','estimated_hours','required')",
            w.bug, w.dev, w.new
        ),
    )
    .await;
    let e = human(
        &w,
        a,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i2), "set": {"subject": "x"}}),
            ],
            vec![(i2, 0)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing, "{e:#}");
    assert_eq!(detail(&e)["fields"], json!(["estimated_hours"]));
    let acts = describe::issue_actions(&w.app, a, &key_of(&w, i2).await)
        .await
        .unwrap();
    assert!(
        acts.fields
            .iter()
            .any(|f| f.key == "estimated_hours" && f.required)
    );
    human(&w, a, &set_doc(&w, i2, json!("4")).await)
        .await
        .unwrap();
    exec(
        &pool,
        "UPDATE workflow_field_rules SET rule = 'readonly' WHERE field_key = 'estimated_hours'",
    )
    .await;
    let e = human(&w, a, &set_doc(&w, i2, json!("5")).await)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReadonlyField, "{e:#}");
    exec(
        &pool,
        "DELETE FROM workflow_field_rules WHERE field_key = 'estimated_hours'",
    )
    .await;

    // ------------------------------------------------ the review diff shows it
    let body = json!({"ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i2),
                               "set": {"estimated_hours": "6.5"}}]});
    let mut conn = pool.acquire().await.unwrap();
    let rows = review_queue::diff(
        &mut conn,
        a,
        &body,
        &BTreeMap::from([(i2, key_of(&w, i2).await)]),
    )
    .await
    .unwrap();
    drop(conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].label.as_str(), &rows[0].before, &rows[0].after),
        ("Estimated time", &json!("4"), &json!("6.5"))
    );

    // ------------------------------------------------ D20 policies (T-D1, T-D2, T-D3)
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    // A new field is not in an existing delegation (02 §16.3 rule 1) …
    let i3 = w.issue(w.web, "agent").await;
    let e = agent_submit(&w, agent, &set_doc(&w, i3, json!("3")).await)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(detail(&e)["reason"], "policy", "{e:#}");
    assert_eq!(detail(&e)["field"], "estimated_hours");
    // … but offered.
    let p = agents::delegation_policies(&pool, &a, d.id).await.unwrap();
    assert!(p.available_fields.contains(&"estimated_hours".to_string()));
    let set_policy = |m: PolicyMode| {
        let w = &w;
        async move {
            agents::set_delegation_policies(
                &w.pool,
                &w.policy,
                &w.alice_ctx,
                d.id,
                fields(&[("estimated_hours", m)]),
            )
            .await
            .unwrap()
        }
    };
    let v = set_policy(Direct).await;
    assert_eq!(v.fields.get("estimated_hours"), Some(&Direct));
    assert!(!v.available_fields.contains(&"estimated_hours".to_string()));

    // T-D1 direct: applied, with an execution and undo copies.
    let e3 = applied(
        &agent_submit(&w, agent, &set_doc(&w, i3, json!("3.0")).await)
            .await
            .unwrap(),
    );
    assert_eq!(hours(&w, i3).await.as_deref(), Some("3"));
    let copies: (Value, Value) = sqlx::query_as(
        "SELECT before_value, after_value FROM execution_field_changes
         WHERE execution_id = $1 AND field_key = 'estimated_hours'",
    )
    .bind(e3)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(copies, (Value::Null, json!("3")));
    // create_issue with the field, direct.
    let r = agent_submit(
        &w,
        agent,
        &doc(
            vec![json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "agent made", "estimated_hours": "2"}})],
            vec![],
        ),
    )
    .await
    .unwrap();
    applied(&r);

    // T-D1 review / T-D2: the whole document goes to review, nothing written.
    set_policy(Review).await;
    let lv3 = lv(&w, i3).await;
    let op_review = Uuid::now_v7();
    let d_review = set_doc(&w, i3, json!("9")).await;
    let r = changesets::command_submit(&w.app, agent, op_review, &d_review, false, ORIGIN)
        .await
        .unwrap();
    let (cs, reason, hash) = reviewed(&r);
    assert_eq!(reason, "policy");
    assert_eq!(hours(&w, i3).await.as_deref(), Some("3"));
    assert_eq!(lv(&w, i3).await, lv3);
    let r = agent_submit(
        &w,
        agent,
        &doc(
            vec![json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "t-d2", "estimated_hours": "1"}})],
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(reviewed(&r).1, "policy");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issues WHERE subject = 't-d2'").await,
        0
    );

    // T-D3: review → direct, the same operation_id replays the review.
    set_policy(Direct).await;
    let r = changesets::command_submit(&w.app, agent, op_review, &d_review, false, ORIGIN)
        .await
        .unwrap();
    assert_eq!(reviewed(&r).0, cs);
    assert_eq!(hours(&w, i3).await.as_deref(), Some("3"));

    // T-D1 off: refused, nothing written.
    set_policy(Off).await;
    let e = agent_submit(&w, agent, &set_doc(&w, i3, json!("7")).await)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(detail(&e)["reason"], "policy");
    assert_eq!(detail(&e)["field"], "estimated_hours");
    assert_eq!(hours(&w, i3).await.as_deref(), Some("3"));
    set_policy(Direct).await;

    // ------------------------------------------------ undo (T-D7 direct and reviewed, T-D6)
    let pv = undo::preview(&w.app, a, e3).await.unwrap();
    let uf = pv
        .fields
        .iter()
        .find(|f| f.field == "estimated_hours")
        .unwrap();
    assert!(uf.restorable, "{pv:?}");
    assert_eq!((&uf.before, &uf.after), (&Value::Null, &json!("3")));
    let u = undo::apply(&w.app, a, Uuid::now_v7(), e3, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(u.fields_restored, 1);
    assert_eq!(hours(&w, i3).await, None);

    // A reviewed write, approved and applied, then undone.
    let _ = hash;
    set_policy(Review).await;
    let r = agent_submit(&w, agent, &set_doc(&w, i3, json!("12.5")).await)
        .await
        .unwrap();
    let (cs2, _, hash2) = reviewed(&r);
    let o = changesets::approve_and_apply(
        &w.app,
        a,
        Uuid::now_v7(),
        cs2,
        &ApproveInput {
            version: 1,
            proposal_hash: hash2,
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(hours(&w, i3).await.as_deref(), Some("12.5"));
    let u = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        o.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.reverts_execution_id, o.execution_id);
    assert_eq!(hours(&w, i3).await, None);
    set_policy(Direct).await;

    // T-D6: agent sets 8 → alice 3 → carol 8: not restored (origin moved on).
    let e6 = applied(
        &agent_submit(&w, agent, &set_doc(&w, i3, json!("8")).await)
            .await
            .unwrap(),
    );
    human(&w, a, &set_doc(&w, i3, json!("3")).await)
        .await
        .unwrap();
    let carol = sign_in(&pool, &w.policy, "carol").await;
    human(&w, carol, &set_doc(&w, i3, json!("8")).await)
        .await
        .unwrap();
    let pv = undo::preview(&w.app, a, e6).await.unwrap();
    assert_eq!(pv.fields[0].reason.as_deref(), Some("changed_since"));
    let e = undo::apply(&w.app, a, Uuid::now_v7(), e6, &UndoInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    assert_eq!(hours(&w, i3).await.as_deref(), Some("8"));

    // ------------------------------------------------ T-D9: D12 derived copies
    let e9 = applied(
        &agent_submit(&w, agent, &set_doc(&w, i3, json!("10")).await)
            .await
            .unwrap(),
    );
    // alice manages WEB (granting a permission bumps every epoch).
    grant(&pool, w.dev, &["manage_project"]).await;
    let note = human(&w, a, &note_doc(i3, lv(&w, i3).await, "customer secret"))
        .await
        .unwrap();
    let _ = note;
    let note_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(i3)
    .fetch_one(&pool)
    .await
    .unwrap();
    redaction::redact(
        &w.app,
        a,
        &w.policy,
        Uuid::now_v7(),
        &Target::Note {
            key: key_of(&w, i3).await,
            note: note_id,
        },
        &RedactInput {
            reason_code: "confidential".into(),
            reason: "pasted by mistake".into(),
            attachments: Vec::new(),
        },
    )
    .await
    .unwrap();
    let copies: (Value, Value) = sqlx::query_as(
        "SELECT before_value, after_value FROM execution_field_changes
         WHERE execution_id = $1 AND field_key = 'estimated_hours'",
    )
    .bind(e9)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        copies,
        (json!(redaction::REDACTED), json!(redaction::REDACTED))
    );
    let pv = undo::preview(&w.app, a, e9).await.unwrap();
    assert_eq!(pv.fields[0].reason.as_deref(), Some("redacted"), "{pv:?}");
    assert!(!serde_json::to_string(&pv).unwrap().contains("\"8\""));
}
