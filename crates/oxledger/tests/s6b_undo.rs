// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.4: undo of agent writes, incident recovery, the activity
//! page and the summary card (§16.7, §16.10): T-D6, T-D7, T-D8, T-D12, the
//! four field conditions, transitions, "close as created by mistake", undo
//! of an undo, the transitive source snapshot and who may undo (B6).
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands;
use oxledger::issue_view;
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::undo::{self, UndoInput};
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

async fn mk(w: &World, subject: &str) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn subject(w: &World, i: Uuid) -> String {
    sqlx::query_scalar("SELECT subject FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "agent write", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

/// The agent writes directly (default policy direct); the execution id.
async fn direct(w: &World, agent: AuthContext, d: Value) -> Uuid {
    match changesets::command_submit(&w.app, agent, Uuid::now_v7(), &d, false, ORIGIN)
        .await
        .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("not applied: {other:?}"),
    }
}

/// The agent proposes, alice approves and applies; the changeset execution id.
async fn applied(w: &World, agent: AuthContext, d: Value) -> Uuid {
    let p = changesets::propose(&w.app, agent, Uuid::now_v7(), &d, ORIGIN)
        .await
        .unwrap();
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
    assert_eq!(o.execution.as_ref().unwrap().status, "succeeded");
    o.execution_id
}

async fn human(w: &World, who: AuthContext, d: Value) {
    commands::submit(&w.app, who, Uuid::now_v7(), &d)
        .await
        .unwrap();
}

fn set(i: Uuid, k: &str, v: Value) -> Value {
    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {k: v}})
}

#[tokio::test]
async fn s6b_undo() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    for (from, to) in [(w.doing, w.new), (w.new, w.done), (w.doing, w.done)] {
        sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
            .bind(w.bug)
            .bind(w.dev)
            .bind(from)
            .bind(to)
            .execute(&pool)
            .await
            .unwrap();
    }
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, mut agent) = w.token(ctx).await;
    let a = w.alice_ctx;
    let agent_id = agent.principal_id().unwrap();

    // ------------------------------------------------ a direct write, undone
    let (i1, k1) = mk(&w, "original").await;
    let (i2, _) = mk(&w, "the original issue").await;
    let e1 = direct(
        &w,
        agent,
        doc(
            vec![
                set(i1, "subject", json!("agent subject")),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i1), "body": "AGENT-NOTE"}),
            ],
            vec![(i1, 0)],
        ),
    )
    .await;
    // duplicate_of is the only op of its document.
    let e_dup = direct(
        &w,
        agent,
        doc(
            vec![json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(i1), "target": tgt(i2)})],
            vec![(i1, lv(&w, i1).await), (i2, 0)],
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e1).await.unwrap();
    assert!(!pv.expired);
    assert_eq!(pv.fields.len(), 1, "{pv:?}");
    assert!(pv.fields[0].restorable, "{pv:?}");
    assert_eq!(pv.fields[0].before, json!("original"));
    assert_eq!(pv.notes.len(), 1);
    assert!(pv.notes[0].restorable, "{pv:?}");
    let pd = undo::preview(&w.app, a, e_dup).await.unwrap();
    assert_eq!(pd.relations.len(), 1);
    assert!(pd.relations[0].restorable, "{pd:?}");
    let ud = undo::apply(&w.app, a, Uuid::now_v7(), e_dup, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(ud.relations_removed, 1);

    // B6: only the delegator.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = undo::preview(&w.app, bob, e1).await.unwrap_err();
    assert_eq!(code(&e).as_str(), "not_found", "{e:#}");
    // An agent cannot undo.
    let e = undo::apply(&w.app, agent, Uuid::now_v7(), e1, &UndoInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "human_session_required", "{e:#}");

    let key = Uuid::now_v7();
    let u1 = undo::apply(&w.app, a, key, e1, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(
        (u1.fields_restored, u1.notes_retracted, u1.relations_removed),
        (1, 1, 0)
    );
    assert_eq!(subject(&w, i1).await, "original");
    // Same key → the same result, nothing more.
    let again = undo::apply(&w.app, a, key, e1, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(again, u1);
    let (reverts, origin): (Option<Uuid>, String) =
        sqlx::query_as("SELECT reverts_execution_id, origin FROM command_executions WHERE id = $1")
            .bind(u1.execution_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((reverts, origin.as_str()), (Some(e1), "human_direct"));
    // The source snapshot is the reverted one's (transitive).
    let src = |e: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Uuid, i64)>(
                "SELECT project_id, security_epoch FROM command_execution_source_projects
                 WHERE execution_id = $1 ORDER BY project_id",
            )
            .bind(e)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert!(!src(e1).await.is_empty());
    assert_eq!(src(u1.execution_id).await, src(e1).await);
    // The note is retracted: readers see who retracted it, not the body.
    let g = issue_view::get(
        &w.app,
        a,
        serde_json::from_value(
            json!({"key": k1, "fields": ["subject"], "include": ["recent_notes"]}),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let g = serde_json::to_value(&g).unwrap();
    let n = &g["recent_notes"]["items"][0];
    assert_eq!(n["retracted"], true, "{g}");
    assert_eq!(n["retracted_by"], "alice", "{g}");
    assert_eq!(n["body"], "");
    assert!(n.get("citation_id").is_none());
    assert!(!g.to_string().contains("AGENT-NOTE"));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_duplicates").await,
        0
    );
    // A second undo of the same execution: nothing left.
    let e = undo::apply(&w.app, a, Uuid::now_v7(), e1, &UndoInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "undo_not_applicable", "{e:#}");

    // Undo of the undo: the subject goes back to the agent's value.
    let u2 = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        u1.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u2.fields_restored, 1);
    assert_eq!(subject(&w, i1).await, "agent subject");
    assert_eq!(src(u2.execution_id).await, src(e1).await);

    // -------------------------------------- T-D6: changed and changed back
    let (i3, _) = mk(&w, "t-d6").await;
    let e3 = direct(
        &w,
        agent,
        doc(vec![set(i3, "subject", json!("high"))], vec![(i3, 0)]),
    )
    .await;
    human(
        &w,
        a,
        doc(
            vec![set(i3, "subject", json!("mid"))],
            vec![(i3, lv(&w, i3).await)],
        ),
    )
    .await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    human(
        &w,
        carol,
        doc(
            vec![set(i3, "subject", json!("high"))],
            vec![(i3, lv(&w, i3).await)],
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e3).await.unwrap();
    assert_eq!(
        pv.fields[0].reason.as_deref(),
        Some("changed_since"),
        "{pv:?}"
    );
    let e = undo::apply(&w.app, a, Uuid::now_v7(), e3, &UndoInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e).as_str(), "undo_not_applicable");
    assert_eq!(subject(&w, i3).await, "high");

    // Permission: alice loses edit_issues → `permission`.
    let (i4, _) = mk(&w, "perm").await;
    let e4 = applied(
        &w,
        agent,
        doc(vec![set(i4, "subject", json!("agent"))], vec![(i4, 0)]),
    )
    .await;
    exec(
        &pool,
        &format!(
            "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'edit_issues'",
            w.dev
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e4).await.unwrap();
    assert_eq!(pv.fields[0].reason.as_deref(), Some("permission"), "{pv:?}");
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permissions VALUES ('{}', 'edit_issues')",
            w.dev
        ),
    )
    .await;
    // Permission changes bump the epoch: the agent needs a new work context.
    let renew = async || {
        let ctx = w.new_context().await;
        w.delegate(w.web).await.unwrap();
        w.token(ctx).await.1
    };
    agent = renew().await;
    // The old value is no longer valid: the original assignee left the project.
    let (i5, _) = mk(&w, "valid").await;
    human(
        &w,
        a,
        doc(
            vec![set(i5, "assigned_to_id", json!(w.carol))],
            vec![(i5, 0)],
        ),
    )
    .await;
    let e5 = applied(
        &w,
        agent,
        doc(
            vec![set(i5, "assigned_to_id", json!(w.bob))],
            vec![(i5, lv(&w, i5).await)],
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "DELETE FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.carol
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e5).await.unwrap();
    assert_eq!(
        pv.fields[0].reason.as_deref(),
        Some("value_no_longer_valid"),
        "{pv:?}"
    );
    agent = renew().await;

    // ---------------------------------------- T-D8: the relation instance
    let (i6, _) = mk(&w, "dup source").await;
    let (i7, _) = mk(&w, "dup target").await;
    let e6 = applied(
        &w,
        agent,
        doc(
            vec![json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(i6), "target": tgt(i7)})],
            vec![(i6, 0), (i7, 0)],
        ),
    )
    .await;
    exec(
        &pool,
        &format!("DELETE FROM issue_duplicates WHERE source_issue_id = '{i6}'"),
    )
    .await;
    human(
        &w,
        bob,
        doc(
            vec![json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(i6), "target": tgt(i7)})],
            vec![(i6, lv(&w, i6).await), (i7, lv(&w, i7).await)],
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e6).await.unwrap();
    assert_eq!(
        pv.relations[0].reason.as_deref(),
        Some("changed_since"),
        "{pv:?}"
    );
    assert!(
        undo::apply(&w.app, a, Uuid::now_v7(), e6, &UndoInput::default())
            .await
            .is_err()
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_duplicates WHERE source_issue_id = '{i6}'")
        )
        .await,
        1,
        "Bob's relation stays"
    );

    // ----------------------- transitions: back to the old status, by choice
    let (i8, _) = mk(&w, "transition").await;
    let e8 = direct(
        &w,
        agent,
        doc(
            vec![json!({"op_id": op(), "kind": "transition", "target": tgt(i8), "to_status_id": w.doing})],
            vec![(i8, 0)],
        ),
    )
    .await;
    let pv = undo::preview(&w.app, a, e8).await.unwrap();
    let c = &pv.choices[0];
    assert_eq!(c.kind, "transition_back");
    assert_eq!(
        c.options.iter().map(|o| o.to_status_id).collect::<Vec<_>>(),
        vec![w.new]
    );
    // Not done without the choice.
    assert!(
        undo::apply(&w.app, a, Uuid::now_v7(), e8, &UndoInput::default())
            .await
            .is_err()
    );
    let e = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        e8,
        &UndoInput {
            transitions: [(i8, w.done)].into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&e).as_str(),
        "invalid_argument",
        "only an offered option"
    );
    let u8 = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        e8,
        &UndoInput {
            transitions: [(i8, w.new)].into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(u8.transitions, 1);
    let st: Uuid = sqlx::query_scalar("SELECT status_id FROM issues WHERE id = $1")
        .bind(i8)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, w.new);

    // ----------------- create_issue: never deleted, "close as created by mistake"
    let e9 = direct(
        &w,
        agent,
        json!({"command_schema_version": 1, "title": "create",
               "ops": [{"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug,
                        "fields": {"subject": "agent created"}}]}),
    )
    .await;
    let pv = undo::preview(&w.app, a, e9).await.unwrap();
    let c = pv
        .choices
        .iter()
        .find(|c| c.kind == "close_as_mistaken")
        .unwrap();
    assert_eq!(
        c.options.iter().map(|o| o.to_status_id).collect::<Vec<_>>(),
        vec![w.done]
    );
    let created = c.issue_id;
    undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        e9,
        &UndoInput {
            transitions: [(created, w.done)].into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issues WHERE id = '{created}' AND status_id = '{}'",
                w.done
            )
        )
        .await,
        1
    );

    // ------------------------------------ T-D7: an applied proposal, undone
    let (i10, _) = mk(&w, "proposal").await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(vec![set(i10, "subject", json!("proposed"))], vec![(i10, 0)]),
        ORIGIN,
    )
    .await
    .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        a,
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
    let ce = o.execution_id;
    let u10 = undo::apply(&w.app, a, Uuid::now_v7(), ce, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(u10.reverts_execution_id, ce);
    assert_eq!(subject(&w, i10).await, "proposal");
    let kind: String = sqlx::query_scalar(
        "SELECT r.kind FROM command_executions c JOIN execution_refs r ON r.id = c.reverts_execution_id
         WHERE c.id = $1",
    )
    .bind(u10.execution_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind, "changeset");

    // ---------------------------- T-D12: incident recovery, a frozen list
    let since: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (j1, _) = mk(&w, "incident one").await;
    let (j2, _) = mk(&w, "incident two").await;
    let f1 = direct(
        &w,
        agent,
        doc(vec![set(j1, "subject", json!("bad 1"))], vec![(j1, 0)]),
    )
    .await;
    let f2 = direct(
        &w,
        agent,
        doc(vec![set(j2, "subject", json!("bad 2"))], vec![(j2, 0)]),
    )
    .await;
    let ip = undo::incident_preview(&w.app, a, agent_id, since)
        .await
        .unwrap();
    let listed: Vec<Uuid> = ip.items.iter().map(|i| i.execution_id).collect();
    assert_eq!(listed, vec![f2, f1], "newest first");
    // The agent writes again after the list was frozen.
    let f3 = direct(
        &w,
        agent,
        doc(
            vec![set(j1, "subject", json!("bad 3"))],
            vec![(j1, lv(&w, j1).await)],
        ),
    )
    .await;
    let mut results = Vec::new();
    for e in &listed {
        results.push(
            undo::apply(&w.app, a, Uuid::now_v7(), *e, &UndoInput::default())
                .await
                .map(|r| r.fields_restored),
        );
    }
    // f2 restores; f1 no longer holds (f3 wrote over it): partial success.
    assert_eq!(results[0].as_ref().unwrap(), &1);
    assert_eq!(
        code(results[1].as_ref().unwrap_err()).as_str(),
        "undo_not_applicable"
    );
    assert!(!listed.contains(&f3));
    assert_eq!(subject(&w, j2).await, "incident two");
    assert_eq!(subject(&w, j1).await, "bad 3");

    // ---------------------------------------- the activity page and summary
    let act = undo::activity(&w.app, a, agent_id, None, None, None)
        .await
        .unwrap();
    let kinds: Vec<&str> = act.items.iter().map(|i| i.kind.as_str()).collect();
    for k in ["direct", "proposal", "undo"] {
        assert!(kinds.contains(&k), "{k} in {kinds:?}");
    }
    let row = act
        .items
        .iter()
        .find(|i| i.execution_id == Some(f3))
        .unwrap();
    assert_eq!(row.diff[0].after, json!("bad 3"));
    assert!(row.undoable);
    let only = undo::activity(&w.app, a, agent_id, None, None, Some(&k1))
        .await
        .unwrap();
    assert!(only.items.iter().all(|i| i.keys.contains(&k1)), "{only:?}");
    // Bob sees nothing of alice's agent.
    let none = undo::activity(&w.app, bob, agent_id, None, None, None)
        .await
        .unwrap();
    assert!(none.items.iter().all(|i| i.kind != "direct"), "{none:?}");
    let s = undo::summary(&w.app, a).await.unwrap();
    assert_eq!(s.today.len(), 1);
    assert!(s.today[0].issues_changed >= 8, "{s:?}");
    assert!(s.suspended.is_empty());
}
