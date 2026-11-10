// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.1: the review queue (§15.3) and `review_mode: "queue"`
//! with step 13a (§6.7). T-U1, T-U2, T-U3 (server part), T-U4, T-U9, plus
//! the list's diff, `review_modes`, group filter and cursor. Needs
//! PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::AuthContext;
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput, ProposeResult};
use oxledger::issue_view::{self, GetParams};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use world::*;

async fn propose(w: &World, agent: AuthContext, doc: &Value) -> ProposeResult {
    changesets::propose(&w.app, agent, Uuid::now_v7(), doc, ORIGIN)
        .await
        .unwrap()
}

async fn approve(
    w: &World,
    who: AuthContext,
    key: Uuid,
    p: &ProposeResult,
    mode: &str,
) -> anyhow::Result<changesets::ApplyOutcome> {
    changesets::approve_and_apply(
        &w.app,
        who,
        key,
        p.changeset_id,
        &ApproveInput {
            version: p.version,
            proposal_hash: p.proposal_hash.clone(),
            review_mode: Some(mode.to_string()),
        },
    )
    .await
}

fn assign_doc(i: Uuid, lv: i32, to: Uuid) -> Value {
    json!({"command_schema_version": 1, "title": "assign",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                    "set": {"assigned_to_id": to}}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

async fn rows(w: &World) -> (i64, i64, i64) {
    (
        count(&w.pool, "SELECT count(*) FROM changeset_decisions").await,
        count(&w.pool, "SELECT count(*) FROM changeset_executions").await,
        count(&w.pool, "SELECT count(*) FROM operation_requests").await,
    )
}

#[tokio::test]
async fn s6b_queue() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_token, agent) = w.token(ctx).await;
    let i1 = w.issue(w.web, "queue one").await;
    let i2 = w.issue(w.web, "queue two").await;
    let cited = w.issue(w.web, "cited").await;
    exec(
        &pool,
        &format!("UPDATE issues SET description = 'Safari 17 重現' WHERE id = '{cited}'"),
    )
    .await;

    // ------------------------------------------------ list: diff and review_modes
    let pa = propose(&w, agent, &assign_doc(i1, 0, w.bob)).await;
    let pn = propose(&w, agent, &note_doc(i2, 0, "a note")).await;
    let pd = propose(&w, agent, &subject_doc(i2, 0, "renamed")).await;
    let l = changesets::list(&w.app, w.alice_ctx, None, None, None, None)
        .await
        .unwrap();
    let row = |id: Uuid| {
        l.items
            .iter()
            .find(|r| r.changeset_id == id)
            .unwrap()
            .clone()
    };
    let ra = row(pa.changeset_id);
    assert_eq!(ra.review_modes, vec!["detail", "queue"]);
    assert_eq!(ra.diff.len(), 1);
    assert_eq!(ra.diff[0].field, "assigned_to_id");
    assert_eq!(ra.diff[0].label, "Assignee");
    assert_eq!(ra.diff[0].before, Value::Null);
    assert_eq!(ra.diff[0].after["name"], "bob");
    assert_eq!(ra.diff[0].after["login"], "bob");
    assert_eq!(ra.diff[0].target.subject, "queue one");
    assert_eq!(ra.proposal_hash.as_deref(), Some(pa.proposal_hash.as_str()));
    assert_eq!(ra.validation.as_ref().unwrap().status, "passed");
    assert_eq!(row(pn.changeset_id).review_modes, vec!["detail"]);
    assert_eq!(row(pd.changeset_id).review_modes, vec!["detail"]);

    // -------------------------- T-U1: forged queue approval of a note / a long field
    for p in [&pn, &pd] {
        let before = rows(&w).await;
        let e = approve(&w, w.alice_ctx, Uuid::now_v7(), p, "queue")
            .await
            .unwrap_err();
        assert_eq!(code(&e).as_str(), "review_outdated", "{e:#}");
        assert_eq!(rows(&w).await, before, "transaction A left rows");
    }
    // Detail review still works for them.
    let o = approve(&w, w.alice_ctx, Uuid::now_v7(), &pn, "detail")
        .await
        .unwrap();
    assert_eq!(o.execution.unwrap().status, "succeeded");
    // revise_and_apply is not an approve-and-apply mode.
    let e = approve(&w, w.alice_ctx, Uuid::now_v7(), &pa, "revise_and_apply")
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "invalid_request", "{e:#}");

    // ------------------- T-U2: two queue proposals on one issue → the second conflicts
    let i3 = w.issue(w.web, "queue three").await;
    let q1 = propose(&w, agent, &assign_doc(i3, 0, w.bob)).await;
    let q2 = propose(&w, agent, &assign_doc(i3, 0, w.alice)).await;
    let o1 = approve(&w, w.alice_ctx, Uuid::now_v7(), &q1, "queue")
        .await
        .unwrap();
    assert_eq!(o1.execution.unwrap().status, "succeeded");
    // The stored validation still says passed: transaction A accepts it and
    // B fails with the ordinary conflict.
    let ex = approve(&w, w.alice_ctx, Uuid::now_v7(), &q2, "queue")
        .await
        .unwrap()
        .execution
        .unwrap();
    assert_eq!(ex.status, "failed");
    assert_eq!(ex.error.unwrap().code, "conflict");
    let assigned: Option<Uuid> =
        sqlx::query_scalar("SELECT assigned_to_id FROM issues WHERE id = $1")
            .bind(i3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(assigned, Some(w.bob));

    // --------------------- T-U3: the session expires mid-queue (server part)
    let i4 = w.issue(w.web, "queue four").await;
    let i5 = w.issue(w.web, "queue five").await;
    let s1 = propose(&w, agent, &assign_doc(i4, 0, w.bob)).await;
    let s2 = propose(&w, agent, &assign_doc(i5, 0, w.bob)).await;
    let k1 = Uuid::now_v7();
    let first = approve(&w, w.alice_ctx, k1, &s1, "queue").await.unwrap();
    let first_exec = first.execution_id;
    exec(
        &pool,
        "UPDATE browser_sessions SET idle_expires_at = now() - interval '1 second'",
    )
    .await;
    let before = rows(&w).await;
    let e = approve(&w, w.alice_ctx, Uuid::now_v7(), &s2, "queue")
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "unauthenticated", "{e:#}");
    assert_eq!(rows(&w).await, before);
    // After signing in again, the original key answers the original result.
    let alice2 = sign_in(&pool, &w.policy, "alice").await;
    let again = approve(&w, alice2, k1, &s1, "queue").await.unwrap();
    assert_eq!(again.execution_id, first_exec);

    // ------------------------ T-U4: revised after the tick → version_superseded
    let i6 = w.issue(w.web, "queue six").await;
    let t = propose(&w, agent, &assign_doc(i6, 0, w.bob)).await;
    changesets::revise(
        &w.app,
        agent,
        Uuid::now_v7(),
        t.changeset_id,
        1,
        &assign_doc(i6, 0, w.alice),
        ORIGIN,
    )
    .await
    .unwrap();
    let e = approve(&w, alice2, Uuid::now_v7(), &t, "queue")
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "version_superseded", "{e:#}");

    // ---- T-U9: evidence on another issue changes between A and B → review_outdated
    let got = issue_view::get(
        &w.app,
        agent,
        serde_json::from_value::<GetParams>(json!({
            "key": key_of(&w, cited).await, "fields": ["description"],
            "citations": ["description"]}))
        .unwrap(),
    )
    .await
    .unwrap();
    let got = serde_json::to_value(&got).unwrap();
    let i7 = w.issue(w.web, "queue seven").await;
    let mut doc = assign_doc(i7, 0, w.bob);
    doc["evidence"] = json!([{"citation_id": got["citations"]["description"],
                              "quote": "Safari 17 重現"}]);
    let p9 = propose(&w, agent, &doc).await;
    let l = changesets::list(&w.app, alice2, None, None, None, None)
        .await
        .unwrap();
    let r9 = l
        .items
        .iter()
        .find(|r| r.changeset_id == p9.changeset_id)
        .unwrap();
    assert_eq!(
        r9.review_modes,
        vec!["detail", "queue"],
        "{:?}",
        r9.warnings
    );
    let g = Arc::new(Gate::default());
    faults::arm("approve.after_a", Action::Block(g.clone()));
    let task = {
        let (app, p9) = (w.app.clone(), p9.clone());
        tokio::spawn(async move {
            changesets::approve_and_apply(
                &app,
                alice2,
                Uuid::now_v7(),
                p9.changeset_id,
                &ApproveInput {
                    version: 1,
                    proposal_hash: p9.proposal_hash,
                    review_mode: Some("queue".into()),
                },
            )
            .await
        })
    };
    g.reached.notified().await;
    exec(
        &pool,
        &format!("UPDATE issues SET description = 'changed' WHERE id = '{cited}'"),
    )
    .await;
    g.release.notify_one();
    let o = task.await.unwrap().unwrap();
    let ex = o.execution.unwrap();
    assert_eq!(ex.status, "failed");
    assert_eq!(ex.error.unwrap().code, "review_outdated");
    let assigned: Option<Uuid> =
        sqlx::query_scalar("SELECT assigned_to_id FROM issues WHERE id = $1")
            .bind(i7)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(assigned, None, "zero effects");
    // The list now warns about it.
    let l = changesets::list(&w.app, alice2, None, None, None, None)
        .await
        .unwrap();
    if let Some(r) = l.items.iter().find(|r| r.changeset_id == p9.changeset_id)
        && r.state == "proposed"
    {
        assert_eq!(r.warnings, vec!["evidence_changed"]);
        assert_eq!(r.review_modes, vec!["detail"]);
    }

    // ----------------------------------------- group filter and cursor
    let gl = changesets::propose_in_group(
        &w.app,
        agent,
        Uuid::now_v7(),
        &assign_doc(w.issue(w.web, "grouped").await, 0, w.bob),
        Some(&changesets::GroupRef::Label("本週分流".into())),
        ORIGIN,
    )
    .await
    .unwrap();
    let gid = gl.group_id.unwrap();
    let l = changesets::list(&w.app, alice2, Some("all"), None, Some(gid), None)
        .await
        .unwrap();
    assert_eq!(l.items.len(), 1);
    assert_eq!(l.items[0].group_id, Some(gid));
    let all = count(
        &pool,
        &format!(
            "SELECT count(*) FROM changesets WHERE on_behalf_of_id = '{}'",
            w.alice
        ),
    )
    .await;
    for n in 0..(52 - all) {
        let i = w.issue(w.web, "filler").await;
        let _ = n;
        propose(&w, agent, &assign_doc(i, 0, w.bob)).await;
    }
    let p1 = changesets::list(&w.app, alice2, Some("all"), None, None, None)
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 50);
    let next = p1.next_cursor.clone().expect("a second page");
    let p2 = changesets::list(&w.app, alice2, Some("all"), None, None, Some(&next))
        .await
        .unwrap();
    assert_eq!(p2.items.len() as i64, 52 - 50);
    assert!(p2.next_cursor.is_none());
    assert!(
        p2.items
            .iter()
            .all(|r| p1.items.iter().all(|x| x.changeset_id != r.changeset_id))
    );
    // A cursor of another query is refused.
    assert!(
        changesets::list(&w.app, alice2, Some("awaiting_me"), None, None, Some(&next))
            .await
            .is_err()
    );
    faults::clear();
}

async fn key_of(w: &World, id: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(id)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// §15.3 rules that need the database: a long (`text`) custom field and a
/// validation that did not pass keep a proposal out of the queue.
#[tokio::test]
async fn eligibility_needs_short_fields_and_validation() {
    use oxledger::review_queue::eligibility;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let long: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype) VALUES ('Notes long', 'text') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let short: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype) VALUES ('Tag', 'string') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let alice = sign_in(&pool, &w.policy, "alice").await;
    let t = json!({"type": "issue", "id": Uuid::nil()});
    let body =
        |k: String| json!({"ops": [{"kind": "update_fields", "target": t, "set": {k: "x"}}]});
    let mut c = pool.acquire().await.unwrap();
    let ok = eligibility(&mut c, alice, &body(format!("cf:{short}")), "passed", None)
        .await
        .unwrap();
    assert!(ok.queue_ok(), "{:?}", ok.reasons);
    let e = eligibility(&mut c, alice, &body(format!("cf:{long}")), "passed", None)
        .await
        .unwrap();
    assert_eq!(e.reasons, vec![format!("long_field:cf:{long}")]);
    let e = eligibility(&mut c, alice, &body(format!("cf:{short}")), "failed", None)
        .await
        .unwrap();
    assert_eq!(e.reasons, vec!["validation:failed".to_string()]);
    assert_eq!(e.review_modes(), vec!["detail".to_string()]);
}
