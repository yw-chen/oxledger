// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6.1: security_epoch triggers and project_catalog (§3.7, §3.8).
//! Per-trigger bumps, T-Q2, T-Q3, T-X7, T-X12, T-X14, T-X15, T-X16, T-X20,
//! T-X23, T-X24, T-R6, T-R11. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth;
use oxledger::changesets::{
    self, ApproveInput,
    faults::{self, Action, Gate},
};
use oxledger::error::ErrorCode;
use oxledger::service::{self, SearchParams};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use world::*;

/// (WEB, AUTH) epoch deltas caused by `sql`.
async fn delta(pool: &PgPool, w: &World, sql: &str) -> (i64, i64) {
    let (a, b) = (epoch(pool, w.web).await, epoch(pool, w.auth_p).await);
    exec(pool, sql).await;
    (
        epoch(pool, w.web).await - a,
        epoch(pool, w.auth_p).await - b,
    )
}

#[tokio::test]
async fn s6_epoch() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let (web, auth_p) = (w.web, w.auth_p);
    let i1 = w.issue(web, "one").await;
    let a1 = w.issue(auth_p, "auth one").await;

    // ---------------------------------------------- each trigger (§3.8 table)
    let both = (1, 1);
    let web_only = (1, 0);
    let none = (0, 0);
    let carol = w.carol;
    let dev = w.dev;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!(
                "INSERT INTO members (project_id, principal_id) VALUES ('{auth_p}', '{carol}')"
            )
        )
        .await,
        (0, 1),
        "members → that project"
    );
    assert_eq!(
        delta(&pool, &w, &format!("INSERT INTO member_roles SELECT id, '{dev}' FROM members WHERE project_id = '{auth_p}' AND principal_id = '{carol}'")).await,
        (0, 1),
        "member_roles → that project"
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!(
                "DELETE FROM members WHERE project_id = '{auth_p}' AND principal_id = '{carol}'"
            )
        )
        .await,
        (0, 1)
    );
    // Groups: the group's projects only.
    let g = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'G') RETURNING id",
    )
    .await;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO group_members VALUES ('{g}', '{carol}')")
        )
        .await,
        none,
        "a group in no project"
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO members (project_id, principal_id) VALUES ('{auth_p}', '{g}')")
        )
        .await,
        (0, 1)
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("DELETE FROM group_members WHERE group_id = '{g}'")
        )
        .await,
        (0, 1),
        "group_members → the group's projects"
    );
    // Roles, permissions: every project.
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE roles SET issues_visibility = 'default' WHERE id = '{dev}'")
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!(
                "UPDATE roles SET issues_visibility = 'default', position = 3 WHERE id = '{dev}'"
            )
        )
        .await,
        none,
        "same visibility, other column"
    );
    exec(
        &pool,
        &format!("UPDATE roles SET issues_visibility = 'all' WHERE id = '{dev}'"),
    )
    .await;
    assert_eq!(
        delta(&pool, &w, &format!("INSERT INTO role_permissions (role_id, permission) VALUES ('{dev}', 'edit_own_issues')")).await,
        both
    );
    assert_eq!(
        delta(&pool, &w, &format!("UPDATE role_permissions SET all_trackers = false WHERE role_id = '{dev}' AND permission = 'edit_own_issues'")).await,
        both
    );
    let bug = w.bug;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!(
                "INSERT INTO role_permission_trackers VALUES ('{dev}', 'edit_own_issues', '{bug}')"
            )
        )
        .await,
        both
    );
    exec(&pool, &format!("DELETE FROM role_permissions WHERE role_id = '{dev}' AND permission = 'edit_own_issues'")).await;
    // Projects: is_public, status, modules → that project; names do not.
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE projects SET is_public = true WHERE id = '{web}'")
        )
        .await,
        web_only
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE projects SET is_public = true, name = 'Web!' WHERE id = '{web}'")
        )
        .await,
        none
    );
    exec(
        &pool,
        &format!("UPDATE projects SET is_public = false WHERE id = '{web}'"),
    )
    .await;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE projects SET status = 'closed' WHERE id = '{web}'")
        )
        .await,
        web_only
    );
    exec(
        &pool,
        &format!("UPDATE projects SET status = 'active' WHERE id = '{web}'"),
    )
    .await;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO enabled_modules VALUES ('{web}', 'wiki')")
        )
        .await,
        web_only
    );
    // Private flags → that project; subject and assignment do not.
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE issues SET is_private = true WHERE id = '{i1}'")
        )
        .await,
        web_only
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE issues SET is_private = true, subject = 'x' WHERE id = '{i1}'")
        )
        .await,
        none
    );
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = false, subject = 'one' WHERE id = '{i1}'"),
    )
    .await;
    let bob = w.bob;
    let note = id(&pool, &format!("INSERT INTO issue_notes (issue_id, author_id, body) VALUES ('{a1}', '{bob}', 'n') RETURNING id")).await;
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE issue_notes SET is_private = true WHERE id = '{note}'")
        )
        .await,
        (0, 1)
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE issue_notes SET body = 'n2' WHERE id = '{note}'")
        )
        .await,
        none,
        "note bodies do not"
    );
    // Custom fields → every project.
    exec(
        &pool,
        "INSERT INTO field_defs (name, ftype, is_for_all) VALUES ('Budget', 'string', true)",
    )
    .await;
    let fid: i32 = sqlx::query_scalar("SELECT id FROM field_defs WHERE name = 'Budget'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE field_defs SET visible = false WHERE id = {fid}")
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("UPDATE field_defs SET description = 'd' WHERE id = {fid}")
        )
        .await,
        none
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO field_roles VALUES ({fid}, '{dev}')")
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO field_trackers VALUES ({fid}, '{bug}')")
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            &format!("INSERT INTO field_projects VALUES ({fid}, '{web}')")
        )
        .await,
        both
    );
    // Users → every project; agents, groups, names do not.
    assert_eq!(
        delta(
            &pool,
            &w,
            "INSERT INTO principals (kind, login, name) VALUES ('user', 'dave', 'dave')"
        )
        .await,
        both,
        "creating a user"
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            "INSERT INTO principals (kind, name) VALUES ('agent', 'some agent')"
        )
        .await,
        none,
        "agents are not readers"
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            "INSERT INTO principals (kind, name) VALUES ('group', 'G2')"
        )
        .await,
        none
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            "UPDATE principals SET status = 'locked' WHERE login = 'dave'"
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            "UPDATE principals SET is_admin = true WHERE login = 'dave'"
        )
        .await,
        both
    );
    assert_eq!(
        delta(
            &pool,
            &w,
            "UPDATE principals SET name = 'Dave' WHERE login = 'dave'"
        )
        .await,
        none
    );
    assert_eq!(
        delta(&pool, &w, "DELETE FROM principals WHERE login = 'dave'").await,
        both
    );
    let e = sqlx::query("UPDATE principals SET kind = 'group', login = NULL WHERE login = 'carol'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("cannot change kind"), "{e}");

    // ---------------------------------------------- T-Q2: the cross-time counterexample
    let d = w.delegate(web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap();
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, 0, "budget is 5"),
        ORIGIN,
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = true WHERE id = {fid}"),
    )
    .await;
    let e = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, 0, "again"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::ContextUnavailable, "quarantined")
    );
    let o = approve(&w, p.changeset_id, &p.proposal_hash).await;
    let err = match o {
        Ok(o) => o.execution.unwrap().error.unwrap().code,
        Err(e) => code(&e).as_str().to_string(),
    };
    assert_eq!(
        err, "context_unavailable",
        "the old proposal cannot be applied"
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i1}'")
        )
        .await,
        0
    );

    // ---------------------------------------------- T-Q3: changed and changed back
    w.new_context().await;
    let d = w.delegate(web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    exec(
        &pool,
        &format!("UPDATE roles SET issues_visibility = 'own' WHERE id = '{dev}'"),
    )
    .await;
    exec(
        &pool,
        &format!("UPDATE roles SET issues_visibility = 'all' WHERE id = '{dev}'"),
    )
    .await;
    let e = service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::ContextUnavailable, "quarantined")
    );

    // ---------------------------------------------- T-X7: AUTH's epoch, a WEB-only proposal
    w.new_context().await;
    let d = w.delegate(web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, 0, "web only"),
        ORIGIN,
    )
    .await
    .unwrap();
    w.delegate(auth_p).await.unwrap();
    exec(
        &pool,
        &format!("INSERT INTO enabled_modules VALUES ('{auth_p}', 'wiki')"),
    )
    .await;
    let o = approve(&w, p.changeset_id, &p.proposal_hash).await.unwrap();
    let ex = o.execution.unwrap();
    let err = ex.error.unwrap();
    assert_eq!(
        (ex.status.as_str(), err.code.as_str()),
        ("failed", "context_unavailable")
    );
    assert_eq!(err.details["kind"], "quarantined");
    let e = service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable, "reads too");

    // ---------------------------------------------- T-X12: creating a user
    w.new_context().await;
    let d = w.delegate(web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    auth::create_user(&pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    let e = service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);

    // ---------------------------------------------- T-X16, T-X20: a new context closes the old one
    w.new_context().await;
    let d = w.delegate(web).await.unwrap();
    let (old_token, agent) = w.token(d.context_id).await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, 0, "old context"),
        ORIGIN,
    )
    .await
    .unwrap();
    let fresh = w.new_context().await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM agent_work_contexts WHERE id = $1")
            .bind(d.context_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "closed"
    );
    let e = approve(&w, p.changeset_id, &p.proposal_hash)
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::ContextUnavailable, "closed")
    );
    let e = auth::authenticate_bearer(&pool, &old_token)
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::ContextUnavailable, "closed")
    );
    let e = service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), kind(&e).as_str()),
        (ErrorCode::ContextUnavailable, "closed")
    );
    // The new context needs its own token and delegation.
    let d = w.delegate(web).await.unwrap();
    assert_eq!(d.context_id, fresh);
    let (_, agent) = w.token(fresh).await;

    // ---------------------------------------------- T-X14: locking a user ∥ an apply
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, lv, "x14"),
        ORIGIN,
    )
    .await
    .unwrap();
    let g = Arc::new(Gate::default());
    faults::arm("apply.after_locks", Action::Block(g.clone()));
    let applying = {
        let (app, alice, cs, h) = (
            w.app.clone(),
            w.alice_ctx,
            p.changeset_id,
            p.proposal_hash.clone(),
        );
        tokio::spawn(async move { approve_with(&app, alice, cs, &h).await })
    };
    g.reached.notified().await;
    let locking = {
        let pool = pool.clone();
        let bob = w.bob;
        tokio::spawn(async move { auth::set_user_status(&pool, bob, false).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !locking.is_finished(),
        "the all-project bump waits for the apply's FOR SHARE"
    );
    g.release.notify_one();
    // No deadlock: both finish (bounded, the timeout reports the state).
    let o = common::join("the applying request", applying)
        .await
        .unwrap();
    assert_eq!(
        o.execution.unwrap().status,
        "succeeded",
        "finished before the bump"
    );
    common::join("the locking request", locking).await.unwrap();
    let e = service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::ContextUnavailable,
        "and then quarantined"
    );
    auth::set_user_status(&pool, w.bob, true).await.unwrap();

    // ---------------------------------------------- T-X15: delegating ∥ an epoch change in R
    w.new_context().await;
    let d = w.delegate(web).await.unwrap();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO enabled_modules VALUES ($1, 'news')")
        .bind(web)
        .execute(&mut *holder)
        .await
        .unwrap();
    let delegating = {
        let (pool, policy, alice, client, auth_p) = (
            pool.clone(),
            w.policy.clone(),
            w.alice_ctx,
            w.client,
            auth_p,
        );
        tokio::spawn(async move {
            oxledger::agents::create_delegation(
                &pool,
                &policy,
                &alice,
                oxledger::agents::DelegationInput {
                    agent_client_id: client,
                    project_id: auth_p,
                    proposal_ops: vec!["add_note".into()],
                    writable_fields: vec![],
                    max_ops: None,
                    expires_in_days: Some(7),
                    policies: None,
                    read_own_time: false,
                    read_own_time_ack: false,
                },
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!delegating.is_finished(), "waits for WEB's row");
    holder.commit().await.unwrap();
    let e = common::join("the delegation", delegating)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable, "{e:#}");
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM agent_context_source_projects WHERE context_id = '{}' AND project_id = '{auth_p}'", d.context_id)).await,
        0,
        "no R row with another epoch"
    );

    // ---------------------------------------------- T-X23: group lock before its projects
    let g3 = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'G3') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO group_members VALUES ('{g3}', '{carol}')"),
    )
    .await;
    let before = epoch(&pool, auth_p).await;
    let mut t1 = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM group_members WHERE group_id = $1")
        .bind(g3)
        .execute(&mut *t1)
        .await
        .unwrap();
    let t2 = {
        let pool = pool.clone();
        tokio::spawn(async move {
            sqlx::query("INSERT INTO members (project_id, principal_id) VALUES ($1, $2)")
                .bind(auth_p)
                .bind(g3)
                .execute(&pool)
                .await
                .unwrap();
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !t2.is_finished(),
        "adding the group waits for the group's lock"
    );
    t1.commit().await.unwrap();
    common::join("the waiting bump", t2).await;
    assert!(epoch(&pool, auth_p).await > before);

    // ---------------------------------------------- T-X24: creating a project waits for an all-project bump
    let mut t1 = pool.begin().await.unwrap();
    sqlx::query(
        "DELETE FROM role_permissions WHERE role_id = $1 AND permission = 'view_private_notes'",
    )
    .bind(dev)
    .execute(&mut *t1)
    .await
    .unwrap();
    let t2 = {
        let pool = pool.clone();
        tokio::spawn(async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO projects (key, name) VALUES ('NEW', 'New') RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !t2.is_finished(),
        "project creation waits for project_catalog"
    );
    t1.commit().await.unwrap();
    let np = common::join("the project creation", t2).await;
    assert_eq!(
        epoch(&pool, np).await,
        1,
        "created after the bump, from the new settings"
    );
    exec(&pool, &format!("INSERT INTO role_permissions (role_id, permission) VALUES ('{dev}', 'view_private_notes')")).await;

    // ---------------------------------------------- T-R6, T-R11: admin change ∥ apply
    for (label, change) in [
        (
            "T-R6 role downgrade",
            format!(
                "DELETE FROM role_permissions WHERE role_id = '{dev}' AND permission = 'manage_versions'; INSERT INTO role_permissions (role_id, permission) VALUES ('{dev}', 'manage_versions')"
            ),
        ),
        (
            "T-R11 field visibility",
            format!("UPDATE field_defs SET visible = NOT visible WHERE id = {fid}"),
        ),
    ] {
        w.new_context().await;
        let d = w.delegate(web).await.unwrap();
        let (_, agent) = w.token(d.context_id).await;
        let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
            .bind(i1)
            .fetch_one(&pool)
            .await
            .unwrap();
        let notes_before = count(
            &pool,
            &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i1}'"),
        )
        .await;
        let p = changesets::propose(
            &w.app,
            agent,
            Uuid::now_v7(),
            &note_doc(i1, lv, label),
            ORIGIN,
        )
        .await
        .unwrap();
        // The admin change commits first: zero effect.
        exec(&pool, &change).await;
        let o = approve(&w, p.changeset_id, &p.proposal_hash).await.unwrap();
        assert_eq!(
            o.execution.unwrap().error.unwrap().code,
            "context_unavailable",
            "{label}"
        );
        assert_eq!(
            count(
                &pool,
                &format!("SELECT count(*) FROM issue_notes WHERE issue_id = '{i1}'")
            )
            .await,
            notes_before,
            "{label}: zero effect"
        );
        // The apply past its locks first: the change waits, no deadlock.
        w.new_context().await;
        let d = w.delegate(web).await.unwrap();
        let (_, agent) = w.token(d.context_id).await;
        let p = changesets::propose(
            &w.app,
            agent,
            Uuid::now_v7(),
            &note_doc(i1, lv, label),
            ORIGIN,
        )
        .await
        .unwrap();
        let g = Arc::new(Gate::default());
        faults::arm("apply.after_locks", Action::Block(g.clone()));
        let applying = {
            let (app, alice, cs, h) = (
                w.app.clone(),
                w.alice_ctx,
                p.changeset_id,
                p.proposal_hash.clone(),
            );
            tokio::spawn(async move { approve_with(&app, alice, cs, &h).await })
        };
        g.reached.notified().await;
        let changing = {
            let (pool, change) = (pool.clone(), change.clone());
            tokio::spawn(async move { exec(&pool, &change).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !changing.is_finished(),
            "{label}: the bump waits for the apply"
        );
        g.release.notify_one();
        // No deadlock: both finish (bounded, the timeout reports the state).
        let o = common::join(&format!("{label}: the applying request"), applying)
            .await
            .unwrap();
        assert_eq!(
            o.execution.unwrap().status,
            "succeeded",
            "{label}: the accepted window"
        );
        common::join(&format!("{label}: the epoch bump"), changing).await;
    }
    faults::clear();
}

async fn approve(w: &World, cs: Uuid, hash: &str) -> anyhow::Result<changesets::ApplyOutcome> {
    approve_with(&w.app, w.alice_ctx, cs, hash).await
}

async fn approve_with(
    app: &oxledger::service::App,
    alice: oxledger::auth::AuthContext,
    cs: Uuid,
    hash: &str,
) -> anyhow::Result<changesets::ApplyOutcome> {
    changesets::approve_and_apply(
        app,
        alice,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: hash.to_string(),
            review_mode: None,
        },
    )
    .await
}
