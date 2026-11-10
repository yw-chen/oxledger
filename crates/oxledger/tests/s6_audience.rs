// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6.2: audience sets and the containment check (§3.8 M4–M9).
//! T-Q1, T-Q5, T-Q7, T-X1–T-X6, T-X8–T-X11, T-X13, T-X18, T-X19, I-4.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput};
use oxledger::audience;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::{self, ApproveInput, ProposeResult, faults};
use oxledger::error::{ErrorCode, classify};
use oxledger::service;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use world::*;

async fn project(
    pool: &PgPool,
    w: &World,
    key: &str,
    public: bool,
    members: &[(Uuid, Uuid)],
) -> Uuid {
    let p: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (key, name, is_public) VALUES ($1, $1, $2) RETURNING id",
    )
    .bind(key)
    .bind(public)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO enabled_modules VALUES ($1, 'issue_tracking')")
        .bind(p)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_trackers VALUES ($1,$2)")
        .bind(p)
        .bind(w.bug)
        .execute(pool)
        .await
        .unwrap();
    for (u, r) in members {
        member(pool, p, *u, *r).await;
    }
    p
}

async fn key_of(pool: &PgPool, issue: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn lv(pool: &PgPool, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn propose(w: &World, agent: AuthContext, doc: &Value) -> anyhow::Result<ProposeResult> {
    changesets::propose(&w.app, agent, Uuid::now_v7(), doc, ORIGIN).await
}

async fn subj(w: &World, agent: AuthContext, i: Uuid, s: &str) -> anyhow::Result<ProposeResult> {
    let v = lv(&w.pool, i).await;
    propose(w, agent, &subject_doc(i, v, s)).await
}

/// The refusal, then the breaker's log of it cleared: these tests refuse on
/// purpose more often than the circuit breaker allows (§16.6, s6_quota).
async fn forbidden(pool: &PgPool, r: anyhow::Result<ProposeResult>, k: &str) -> anyhow::Error {
    exec(
        pool,
        "DELETE FROM agent_write_log WHERE kind = 'audience_blocked'",
    )
    .await;
    let e = r.expect_err("must be rejected");
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::AudienceExpansionForbidden, k),
        "{e:#}"
    );
    e
}

/// The execution's error code ("" when it succeeded).
async fn apply(w: &World, cs: Uuid, version: i32, hash: &str) -> String {
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version,
            proposal_hash: hash.to_string(),
            review_mode: None,
        },
    )
    .await;
    match o {
        Ok(o) => {
            let x = o.execution.expect("finished");
            x.error.map(|e| e.code).unwrap_or_default()
        }
        Err(e) => code(&e).as_str().to_string(),
    }
}

fn create_doc(project: Uuid, tracker: Uuid, subject: &str) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "create_issue", "project_id": project, "tracker_id": tracker,
                    "fields": {"subject": subject}}]})
}

fn private_note_doc(i: Uuid, lv: i32) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "private", "is_private": true}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

async fn delegation(
    w: &World,
    project: Uuid,
    fields: &[&str],
) -> anyhow::Result<agents::DelegationView> {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: project,
            proposal_ops: ["create_issue", "update_fields", "add_note"]
                .map(String::from)
                .to_vec(),
            writable_fields: fields.iter().map(|s| s.to_string()).collect(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
}

#[tokio::test]
async fn s6_audience() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let (web, auth_p) = (w.web, w.auth_p);

    // ------------------------------------------------ configuration (bumps epochs; before any context)
    let dave = auth::create_user(&pool, "dave", "dave", PW, true)
        .await
        .unwrap(); // admin, no membership
    let erin = auth::create_user(&pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    // carol reads WEB's non-private issues only, without private notes.
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE member_roles SET role_id = '{viewer}' WHERE member_id =
               (SELECT id FROM members WHERE project_id = '{web}' AND principal_id = '{}')",
            w.carol
        ),
    )
    .await;
    let anon = id(&pool, "INSERT INTO roles (name, issues_visibility, builtin) VALUES ('Anonymous','default','anonymous') RETURNING id").await;
    grant(&pool, anon, &["view_issues"]).await;
    let secret = id(&pool, "INSERT INTO trackers (name, position, default_status_id, private_by_default)
                           SELECT 'Secret', 2, id, true FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    exec(
        &pool,
        &format!("INSERT INTO project_trackers VALUES ('{web}', '{secret}')"),
    )
    .await;
    let dev = w.dev;
    let same_a = project(&pool, &w, "SAMEA", false, &[(w.alice, dev), (w.bob, dev)]).await;
    let same_b = project(&pool, &w, "SAMEB", false, &[(w.alice, dev), (w.carol, dev)]).await;
    let priv_p = project(&pool, &w, "PRIV", false, &[(w.alice, dev), (w.bob, dev)]).await;
    let pub_p = project(&pool, &w, "PUB", true, &[(w.alice, dev), (w.bob, dev)]).await;
    let note_p = project(
        &pool,
        &w,
        "NOTE",
        false,
        &[(w.alice, dev), (w.bob, dev), (erin, dev)],
    )
    .await;
    // T-Q1 projects.
    let own = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Own','own') RETURNING id",
    )
    .await;
    grant(&pool, own, &["view_issues"]).await;
    let trk = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Trk','all') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO role_permissions VALUES ('{trk}', 'view_issues', false)"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permission_trackers VALUES ('{trk}', 'view_issues', '{}')",
            w.bug
        ),
    )
    .await;
    let trk_empty = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('TrkEmpty','all') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO role_permissions VALUES ('{trk_empty}', 'view_issues', false)"),
    )
    .await;
    let own_p = project(&pool, &w, "OWNP", false, &[(w.alice, dev), (w.bob, own)]).await;
    let trk_p = project(&pool, &w, "TRKP", false, &[(w.alice, dev), (w.bob, trk)]).await;
    let trk_e = project(
        &pool,
        &w,
        "TRKE",
        false,
        &[(w.alice, dev), (w.bob, trk_empty)],
    )
    .await;

    let i_web = w.issue(web, "web one").await;
    let i_web2 = w.issue(web, "web two").await;
    let i_web3 = w.issue(web, "web three").await;
    let i_auth = w.issue(auth_p, "auth one").await;
    let i_auth_priv = create_issue(
        &pool,
        NewIssue {
            project_id: auth_p,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "auth secret",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: true,
            parent_id: None,
        },
    )
    .await
    .unwrap()
    .0;
    let i_same_a = w.issue(same_a, "a").await;
    let i_same_b = w.issue(same_b, "b").await;
    let i_priv = w.issue(priv_p, "p").await;
    let i_pub = w.issue(pub_p, "q").await;
    let i_note = w.issue(note_p, "n").await;

    // ------------------------------------------------ the sets themselves (T-X9, T-X10)
    {
        let mut c = pool.acquire().await.unwrap();
        let a_web = audience::project_audience(&mut c, web).await.unwrap();
        let a_auth = audience::project_audience(&mut c, auth_p).await.unwrap();
        assert!(
            a_web.users.contains(&dave) && a_auth.users.contains(&dave),
            "admins read every project"
        );
        assert!(a_auth.is_subset(&a_web) && !a_web.is_subset(&a_auth));
        assert_eq!(a_web.users.len(), 4, "alice, bob, carol, dave");
        let a_pub = audience::project_audience(&mut c, pub_p).await.unwrap();
        let a_priv = audience::project_audience(&mut c, priv_p).await.unwrap();
        assert!(a_pub.anonymous && !a_priv.anonymous);
        assert_eq!(
            a_pub.users, a_priv.users,
            "no Non member role: members and admins only"
        );
        // Archived or without issue tracking: no readers.
        let a_off = {
            exec(
                &pool,
                "INSERT INTO projects (key, name) VALUES ('OFF','Off')",
            )
            .await;
            let off = id(&pool, "SELECT id FROM projects WHERE key = 'OFF'").await;
            audience::project_audience(&mut c, off).await.unwrap()
        };
        assert!(a_off.users.is_empty() && !a_off.anonymous);
    }

    // ------------------------------------------------ T-Q1: audience-inconsistent projects
    for p in [own_p, trk_p, trk_e] {
        let e = w.delegate(p).await.unwrap_err();
        assert_eq!(
            (code(&e), kind(&e).as_str()),
            (ErrorCode::ForbiddenOp, "project_not_audience_consistent"),
            "{e:#}"
        );
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM delegations").await, 0);

    // ------------------------------------------------ phase A: R = {WEB} then {WEB, AUTH}
    let d_web = w.delegate(web).await.unwrap();
    let (_, agent) = w.token(d_web.context_id).await;
    let p_old = subj(&w, agent, i_web, "old one").await.unwrap();
    let p_old2 = subj(&w, agent, i_web2, "old two").await.unwrap();
    let p_old3 = subj(&w, agent, i_web3, "old three").await.unwrap();
    let d_auth = w.delegate(auth_p).await.unwrap();
    // T-Q7: one context; a second WEB delegation is refused.
    assert_eq!(d_auth.context_id, d_web.context_id);
    assert!(w.delegate(web).await.is_err());

    // T-X1 / T-X4 / I-4: WEB's public field, AUTH never read.
    let e = forbidden(
        &w.pool,
        subj(&w, agent, i_web, "leak").await,
        "cross_project",
    )
    .await;
    let a = classify(&e).unwrap();
    assert_eq!(a.details.as_ref().unwrap()["op_index"], 0);
    assert_eq!(
        a.code.next_actions(),
        [
            oxledger::error::NextAction::AskHuman,
            oxledger::error::NextAction::Stop
        ]
    );
    forbidden(
        &w.pool,
        propose(
            &w,
            agent,
            &note_doc(i_web, lv(&pool, i_web).await, "public note"),
        )
        .await,
        "cross_project",
    )
    .await;
    forbidden(
        &w.pool,
        propose(&w, agent, &create_doc(web, w.bug, "new public")).await,
        "cross_project",
    )
    .await;
    // Nothing stored for a rejection.
    assert_eq!(count(&pool, "SELECT count(*) FROM changesets").await, 3);
    // T-X2: AUTH passes.
    subj(&w, agent, i_auth, "auth ok").await.unwrap();
    // M9: private destinations count actual readers. carol reads WEB but
    // neither private notes nor private issues there, so these pass.
    propose(&w, agent, &private_note_doc(i_web, lv(&pool, i_web).await))
        .await
        .unwrap();
    propose(
        &w,
        agent,
        &create_doc(web, secret, "secret by tracker default"),
    )
    .await
    .unwrap();
    // A private issue assigned to carol: she becomes a reader → blocked.
    let doc = json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": secret,
                    "fields": {"subject": "for carol", "assigned_to_id": w.carol}}]});
    forbidden(
        &w.pool,
        propose(&w, agent, &doc).await,
        "private_destination",
    )
    .await;
    // T-Q5: an existing private issue the agent cannot read.
    let e = propose(&w, agent, &note_doc(i_auth_priv, 0, "x"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");

    // T-X3: the old WEB-only version applies (M6) …
    assert_eq!(
        apply(&w, p_old.changeset_id, 1, &p_old.proposal_hash).await,
        ""
    );
    let s: String = sqlx::query_scalar("SELECT subject FROM issues WHERE id = $1")
        .bind(i_web)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(s, "old one");
    // … but the agent's revision snapshots R = {WEB, AUTH}: blocked.
    let v = lv(&pool, i_web2).await;
    let r = changesets::revise(
        &w.app,
        agent,
        Uuid::now_v7(),
        p_old2.changeset_id,
        1,
        &subject_doc(i_web2, v, "rev"),
        ORIGIN,
    )
    .await;
    forbidden(&w.pool, r, "cross_project").await;

    // T-X6: an owner revision of a WEB-only version takes the current R and
    // is blocked when applied.
    let v = lv(&pool, i_web3).await;
    let view = changesets::owner_revise(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        p_old3.changeset_id,
        &json!({"expected_current_version": 1, "proposal": subject_doc(i_web3, v, "owner rev")}),
    )
    .await
    .unwrap();
    let srcs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM changeset_version_source_projects WHERE changeset_id = $1 AND version = 2 ORDER BY project_id",
    )
    .bind(p_old3.changeset_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut want = vec![web, auth_p];
    want.sort();
    assert_eq!(srcs, want);
    assert_eq!(
        apply(&w, p_old3.changeset_id, 2, &view.proposal_hash).await,
        "audience_expansion_forbidden"
    );
    let s: String = sqlx::query_scalar("SELECT subject FROM issues WHERE id = $1")
        .bind(i_web3)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(s, "web three");
    let st: String = sqlx::query_scalar("SELECT state FROM changesets WHERE id = $1")
        .bind(p_old3.changeset_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "failed");

    // T-X5: revoking AUTH does not lift it (M4), and the message does not say so.
    agents::revoke_delegation(&pool, &w.alice_ctx, d_auth.id)
        .await
        .unwrap();
    let e = forbidden(
        &w.pool,
        subj(&w, agent, i_web, "still").await,
        "cross_project",
    )
    .await;
    assert!(!format!("{e:#}").to_lowercase().contains("revok"), "{e:#}");

    // ------------------------------------------------ phase B: T-X18, T-X19 (one token)
    let ctx_b = w.new_context().await;
    delegation(&w, web, &["subject"]).await.unwrap();
    let (_, agent) = w.token(ctx_b).await;
    let k_auth = key_of(&pool, i_auth).await;
    let e = service::get(&w.app, agent, &k_auth).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let d_auth = delegation(&w, auth_p, &["description"]).await.unwrap();
    assert_eq!(d_auth.context_id, ctx_b);
    service::get(&w.app, agent, &k_auth).await.unwrap();
    // AUTH's own field list applies, not WEB's.
    let e = subj(&w, agent, i_auth, "not allowed").await.unwrap_err();
    assert_ne!(code(&e), ErrorCode::AudienceExpansionForbidden, "{e:#}");
    let desc = |i: Uuid, v: i32| {
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"description": "d"}}],
               "preconditions": [{"target": tgt(i), "lock_version": v}]})
    };
    propose(&w, agent, &desc(i_auth, lv(&pool, i_auth).await))
        .await
        .unwrap();
    // T-X19: WEB revoked → not readable, not proposable; AUTH as before.
    let d_web_b: Uuid =
        sqlx::query_scalar("SELECT id FROM delegations WHERE context_id = $1 AND project_id = $2")
            .bind(ctx_b)
            .bind(web)
            .fetch_one(&pool)
            .await
            .unwrap();
    agents::revoke_delegation(&pool, &w.alice_ctx, d_web_b)
        .await
        .unwrap();
    let e = service::get(&w.app, agent, &key_of(&pool, i_web).await)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = subj(&w, agent, i_web, "gone").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable, "{e:#}");
    propose(&w, agent, &desc(i_auth, lv(&pool, i_auth).await))
        .await
        .unwrap();

    // ------------------------------------------------ T-X8: same size, different people
    let ctx = w.new_context().await;
    w.delegate(same_a).await.unwrap();
    w.delegate(same_b).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    forbidden(
        &w.pool,
        subj(&w, agent, i_same_a, "x").await,
        "cross_project",
    )
    .await;
    forbidden(
        &w.pool,
        subj(&w, agent, i_same_b, "x").await,
        "cross_project",
    )
    .await;

    // ------------------------------------------------ T-X10: anonymous
    let ctx = w.new_context().await;
    w.delegate(priv_p).await.unwrap();
    w.delegate(pub_p).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    forbidden(
        &w.pool,
        subj(&w, agent, i_pub, "to the public").await,
        "cross_project",
    )
    .await;
    subj(&w, agent, i_priv, "members only").await.unwrap();

    // ------------------------------------------------ T-X11: private is not automatically narrower
    let ctx = w.new_context().await;
    w.delegate(web).await.unwrap();
    w.delegate(note_p).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    forbidden(
        &w.pool,
        propose(
            &w,
            agent,
            &private_note_doc(i_note, lv(&pool, i_note).await),
        )
        .await,
        "private_destination",
    )
    .await;
    forbidden(&w.pool, subj(&w, agent, i_note, "x").await, "cross_project").await;

    // ------------------------------------------------ T-X13: AUTH delegation ∥ propose WEB
    for round in 0..6 {
        exec(
            &pool,
            "DELETE FROM agent_write_log WHERE kind = 'audience_blocked'",
        )
        .await;
        let ctx = w.new_context().await;
        w.delegate(web).await.unwrap();
        let (_, agent) = w.token(ctx).await;
        let i = w.issue(web, "race").await;
        let doc = subject_doc(i, lv(&pool, i).await, &format!("race {round}"));
        let (d, p) = tokio::join!(w.delegate(auth_p), propose(&w, agent, &doc));
        d.unwrap();
        match p {
            Ok(p) => {
                let srcs: Vec<Uuid> = sqlx::query_scalar(
                    "SELECT project_id FROM changeset_version_source_projects WHERE changeset_id = $1",
                )
                .bind(p.changeset_id)
                .fetch_all(&pool)
                .await
                .unwrap();
                assert_eq!(srcs, vec![web], "propose committed first: WEB only");
                assert_eq!(apply(&w, p.changeset_id, 1, &p.proposal_hash).await, "");
            }
            Err(e) => assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden, "{e:#}"),
        }
    }
    faults::clear();
}
