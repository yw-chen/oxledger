// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.2: the conflict page (§15.4): `revision-previews` and
//! `revise-and-apply`. T-U5, T-U6, T-U7, T-U10, T-U11, T-U12. Needs
//! PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::AuthContext;
use oxledger::changesets::faults::{self, Action};
use oxledger::changesets::{self, ApproveInput, PreviewInput, ProposeResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

fn assign_doc(i: Uuid, lv: i32, to: Uuid) -> Value {
    json!({"command_schema_version": 1, "title": "assign",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                    "set": {"assigned_to_id": to, "subject": "agent subject"}}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

async fn bump(w: &World, i: Uuid) {
    exec(
        &w.pool,
        &format!("UPDATE issues SET lock_version = lock_version + 1 WHERE id = '{i}'"),
    )
    .await;
}

async fn counts(w: &World) -> (i64, i64, i64) {
    (
        count(&w.pool, "SELECT count(*) FROM changeset_versions").await,
        count(&w.pool, "SELECT count(*) FROM changeset_decisions").await,
        count(&w.pool, "SELECT count(*) FROM changeset_executions").await,
    )
}

/// A proposal whose approval failed with a conflict (the conflict page).
async fn conflicted(w: &World, agent: AuthContext, subject: &'static str) -> (Uuid, ProposeResult) {
    let i = w.issue(w.web, subject).await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &assign_doc(i, 0, w.bob),
        ORIGIN,
    )
    .await
    .unwrap();
    bump(w, i).await;
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: p.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(o.execution.unwrap().error.unwrap().code, "conflict");
    (i, p)
}

async fn preview(
    w: &World,
    who: AuthContext,
    cs: Uuid,
    choices: &[(&str, &str)],
) -> changesets::RevisionPreview {
    changesets::preview_revision(
        &w.app,
        who,
        cs,
        &PreviewInput {
            version: 1,
            choices: choices
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        },
    )
    .await
    .unwrap()
}

fn body(pv: &changesets::RevisionPreview) -> Value {
    json!({"version": 1, "expected_current_version": 1, "proposal": pv.proposal,
           "revision_review_token": pv.revision_review_token})
}

#[tokio::test]
async fn s6b_revise() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;

    // ---------------------------------------------- T-U7: success in one step
    let (i1, p1) = conflicted(&w, agent, "revise one").await;
    let op_assign = p1.changeset_id;
    let doc_ops = changesets::get(&w.app, w.alice_ctx, op_assign, None)
        .await
        .unwrap();
    let doc_ops = serde_json::to_value(&doc_ops).unwrap();
    let op_id = doc_ops["proposal"]["ops"][0]["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Keep the current subject, take the proposed assignee.
    let pv = preview(
        &w,
        w.alice_ctx,
        p1.changeset_id,
        &[(&format!("{op_id}/subject"), "current")],
    )
    .await;
    assert_eq!(pv.validation.status, "passed", "{:?}", pv.validation);
    assert_eq!(pv.proposal["preconditions"][0]["lock_version"], 1);
    assert!(pv.proposal["ops"][0]["set"].get("subject").is_none());
    assert_eq!(pv.proposal["ops"][0]["set"]["assigned_to_id"], json!(w.bob));
    let before = counts(&w).await;
    let (exp_before, ceil_before): (chrono::DateTime<chrono::Utc>, Value) = sqlx::query_as(
        "SELECT c.expires_at, v.authority_ceiling FROM changesets c
         JOIN changeset_versions v ON v.changeset_id = c.id AND v.version = 1 WHERE c.id = $1",
    )
    .bind(p1.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let o = changesets::revise_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p1.changeset_id,
        &body(&pv),
    )
    .await
    .unwrap();
    assert_eq!(o.execution.unwrap().status, "succeeded");
    let after = counts(&w).await;
    assert_eq!(after, (before.0 + 1, before.1 + 1, before.2 + 1));
    let (mode, method, exp_after, ceil_after): (
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        Value,
    ) = sqlx::query_as(
        "SELECT d.review_mode, v.edit_method, c.expires_at, v.authority_ceiling
             FROM changesets c JOIN changeset_versions v ON v.changeset_id = c.id AND v.version = 2
             JOIN changeset_decisions d ON d.changeset_id = c.id AND d.version = 2 WHERE c.id = $1",
    )
    .bind(p1.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (mode.as_str(), method.as_str()),
        ("revise_and_apply", "owner_revise")
    );
    assert_eq!(exp_after, exp_before);
    assert_eq!(ceil_after, ceil_before);
    let a: Option<Uuid> = sqlx::query_scalar("SELECT assigned_to_id FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(a, Some(w.bob));

    // ------------------------------- T-U5 / T-U10: the issue changed after the preview
    let (i2, p2) = conflicted(&w, agent, "revise two").await;
    let pv = preview(&w, w.alice_ctx, p2.changeset_id, &[]).await;
    bump(&w, i2).await;
    let before = counts(&w).await;
    let e = changesets::revise_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p2.changeset_id,
        &body(&pv),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e).as_str(), "review_outdated", "{e:#}");
    assert_eq!(
        counts(&w).await,
        before,
        "no version, decision or execution"
    );
    // The document differs from the preview → also outdated.
    let pv = preview(&w, w.alice_ctx, p2.changeset_id, &[]).await;
    let mut b = body(&pv);
    b["proposal"]["title"] = json!("edited after the preview");
    let e = changesets::revise_and_apply(&w.app, w.alice_ctx, Uuid::now_v7(), p2.changeset_id, &b)
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "review_outdated", "{e:#}");

    // ---------------------------- T-U6: a token for another proposal or owner
    let (_i3, p3) = conflicted(&w, agent, "revise three").await;
    let pv2 = preview(&w, w.alice_ctx, p2.changeset_id, &[]).await;
    let mut b = body(&pv2);
    b["proposal"] = changesets::get(&w.app, w.alice_ctx, p3.changeset_id, None)
        .await
        .map(|v| serde_json::to_value(v).unwrap()["proposal"].clone())
        .unwrap();
    let e = changesets::revise_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p3.changeset_id,
        &body(&pv2),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e).as_str(), "invalid_request", "{e:#}");
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = changesets::revise_and_apply(&w.app, bob, Uuid::now_v7(), p2.changeset_id, &body(&pv2))
        .await
        .unwrap_err();
    assert!(
        matches!(code(&e).as_str(), "invalid_request" | "not_found"),
        "{e:#}"
    );
    let e = changesets::revise_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p2.changeset_id,
        &json!({"version": 1, "expected_current_version": 1, "proposal": pv2.proposal,
                "revision_review_token": "not-a-token"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e).as_str(), "invalid_request", "{e:#}");

    // ------------- T-U11: A commits, the process dies, the token expires, same key
    changesets::set_preview_ttl_secs(1);
    let pv = preview(&w, w.alice_ctx, p2.changeset_id, &[]).await;
    let key = Uuid::now_v7();
    faults::arm("revise.after_a", Action::Fail);
    let o = changesets::revise_and_apply(&w.app, w.alice_ctx, key, p2.changeset_id, &body(&pv))
        .await
        .unwrap();
    assert!(o.pending, "the outcome is unknown after A");
    let mid = counts(&w).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let o2 = changesets::revise_and_apply(&w.app, w.alice_ctx, key, p2.changeset_id, &body(&pv))
        .await
        .unwrap();
    assert_eq!(o2.execution_id, o.execution_id);
    assert_eq!(o2.execution.unwrap().status, "succeeded");
    let end = counts(&w).await;
    assert_eq!(
        (end.0, end.1, end.2),
        (mid.0, mid.1, mid.2),
        "no second version"
    );
    // A new key with the expired token → outdated.
    let e = changesets::revise_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p3.changeset_id,
        &body(&pv),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(code(&e).as_str(), "review_outdated" | "invalid_request"),
        "{e:#}"
    );
    changesets::set_preview_ttl_secs(600);

    // ------------------- T-U12: same key, revise-and-apply then approve-and-apply
    let pv = preview(&w, w.alice_ctx, p3.changeset_id, &[]).await;
    let key = Uuid::now_v7();
    changesets::revise_and_apply(&w.app, w.alice_ctx, key, p3.changeset_id, &body(&pv))
        .await
        .unwrap();
    let e = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        key,
        p3.changeset_id,
        &ApproveInput {
            version: 2,
            proposal_hash: p3.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e).as_str(), "idempotency_key_reused", "{e:#}");

    // Choices name only fields of the proposal.
    let (_i4, p4) = conflicted(&w, agent, "revise four").await;
    let e = changesets::preview_revision(
        &w.app,
        w.alice_ctx,
        p4.changeset_id,
        &PreviewInput {
            version: 1,
            choices: [("nope/subject".to_string(), "current".to_string())].into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e).as_str(), "invalid_argument", "{e:#}");
    let _ = b;
    faults::clear();
}
