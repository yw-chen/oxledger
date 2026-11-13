// SPDX-License-Identifier: GPL-2.0-or-later
//! P2 backend: API gaps from the P1 frontend (docs/impl/p1-backend-gaps.md).
//!
//! * G12 / G13: `GET /changesets/{id}` returns the queue's diff, with the
//!   baseline the server recorded when the agent proposed (only fields the
//!   agent could read; shown only while the viewer can read them; cleared
//!   by redaction).
//! * G14: listing an agent's tokens (`/ai-team/{p}/credentials`; the
//!   `/agent-contexts/{id}/credentials` alias was removed in M2.0) and
//!   `last_used_at`.
//! * G24: the activity lists' filters and signed cursor; what the caller
//!   cannot read changes neither the page nor the cursor.
//!
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::{self, SubmitResult, faults};
use oxledger::error::ErrorCode;
use oxledger::undo::{self, ActivityQuery};
use oxledger::{agents, ai_team, review_queue};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use world::*;

/// Every test resets the database (World::build): one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn assign_doc(i: Uuid, lv: i32, to: Uuid) -> Value {
    json!({"command_schema_version": 1, "title": "assign",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                    "set": {"assigned_to_id": to}}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

fn transition_doc(i: Uuid, lv: i32, to: Uuid) -> Value {
    json!({"command_schema_version": 1, "title": "start",
           "ops": [{"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

async fn key_of(w: &World, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(&w.pool)
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

#[tokio::test]
async fn g12_g13_detail_diff_with_baseline() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let i1 = w.issue(w.web, "assign me").await;
    let i2 = w.issue(w.web, "start me").await;

    let pa = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &assign_doc(i1, 0, w.bob),
        ORIGIN,
    )
    .await
    .unwrap();
    let pt = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &transition_doc(i2, 0, w.doing),
        ORIGIN,
    )
    .await
    .unwrap();
    // The issue changes after the proposal (carol takes it).
    sqlx::query(
        "UPDATE issues SET assigned_to_id = $2, lock_version = lock_version + 1 WHERE id = $1",
    )
    .bind(i1)
    .bind(w.carol)
    .execute(&pool)
    .await
    .unwrap();

    // G12: the detail carries the queue's diff with display values.
    let v = changesets::get(&w.app, w.alice_ctx, pa.changeset_id, None)
        .await
        .unwrap();
    assert_eq!(v.diff.len(), 1, "{:?}", v.diff);
    let r = &v.diff[0];
    assert_eq!(r.field, "assigned_to_id");
    assert_eq!(r.label, "Assignee");
    assert_eq!(r.target.key, key_of(&w, i1).await);
    assert_eq!(r.before["name"], "carol", "{r:?}");
    assert_eq!(r.after["name"], "bob", "{r:?}");
    // G13: the value when the agent proposed (unassigned).
    assert!(r.has_original);
    assert_eq!(r.original, Value::Null);
    // The list shows the same rows.
    let l = changesets::list(&w.app, w.alice_ctx, None, None, None, None)
        .await
        .unwrap();
    let row = l
        .items
        .iter()
        .find(|x| x.changeset_id == pa.changeset_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&row.diff).unwrap(),
        serde_json::to_value(&v.diff).unwrap()
    );

    let t = changesets::get(&w.app, w.alice_ctx, pt.changeset_id, None)
        .await
        .unwrap();
    let s = &t.diff[0];
    assert_eq!(s.field, "status");
    assert!(s.has_original);
    assert_eq!(s.original["name"], "New");
    assert_eq!(s.after["name"], "Doing");
    let stored: Value = sqlx::query_scalar(
        "SELECT c.baseline FROM changeset_versions v JOIN proposal_contents c ON c.id = v.content_id
         WHERE v.changeset_id = $1",
    )
    .bind(pt.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        json!({i2.to_string(): {"lock_version": 0, "fields": {"status": w.new}}})
    );

    // The guard: the baseline neither changes nor is cleared without
    // redacting the content.
    for set in ["baseline = NULL", "baseline = '{}'::jsonb"] {
        let e = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE proposal_contents SET {set} WHERE baseline IS NOT NULL"
        )))
        .execute(&pool)
        .await
        .expect_err(set);
        assert!(e.to_string().contains("redaction"), "{set}: {e}");
    }

    // Redaction clears the baseline with the body.
    let mut c = pool.acquire().await.unwrap();
    oxledger::redaction::derived(&mut c, w.web, w.alice)
        .await
        .unwrap();
    drop(c);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proposal_contents WHERE baseline IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
    let v = changesets::get(&w.app, w.alice_ctx, pa.changeset_id, None)
        .await
        .unwrap();
    assert!(v.diff.is_empty());
    assert_eq!(v.content_unavailable.as_deref(), Some("redacted"));
}

/// The baseline holds only what the agent reads; the diff shows a recorded
/// value only while the viewer can read the field.
#[tokio::test]
async fn g13_baseline_follows_read_scopes() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let field = |name: &'static str, visible: bool| {
        let pool = pool.clone();
        let bug = w.bug;
        async move {
            let id: i32 = sqlx::query_scalar(
                "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ($1, 'string', true, $2)
                 RETURNING id",
            )
            .bind(name)
            .bind(visible)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO field_trackers VALUES ($1, $2)")
                .bind(id)
                .bind(bug)
                .execute(&pool)
                .await
                .unwrap();
            id
        }
    };
    let tag = field("Tag", true).await;
    let secret = field("Secret", false).await;
    // After the fields: adding them moves the projects' security epoch.
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let i = w.issue(w.web, "with fields").await;
    let hidden = w.issue(w.auth_p, "not delegated").await;
    for (f, v) in [(tag, "blue"), (secret, "s3cret")] {
        sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_text) VALUES ($1, $2, $3)")
            .bind(i)
            .bind(f)
            .bind(v)
            .execute(&pool)
            .await
            .unwrap();
    }
    let doc = json!({"ops": [
        {"kind": "update_fields", "target": tgt(i),
         "set": {format!("cf:{tag}"): "red", format!("cf:{secret}"): "x", "subject": "s"}},
        {"kind": "update_fields", "target": tgt(hidden), "set": {"subject": "h"}}]});
    let mut c = pool.acquire().await.unwrap();
    let b = review_queue::baseline(&mut c, agent, &doc).await.unwrap();
    // The agent reads visible fields only, and only delegated projects.
    assert_eq!(
        b,
        json!({i.to_string(): {"lock_version": 0,
               "fields": {format!("cf:{tag}"): "blue", "subject": "with fields"}}})
    );
    // A recorded value of a field the viewer cannot read is not shown.
    // (Secret is restricted with no role: administrators only.)
    let forged = json!({i.to_string(): {"lock_version": 0,
        "fields": {format!("cf:{tag}"): "blue", format!("cf:{secret}"): "s3cret", "subject": "with fields"}}});
    let keys = BTreeMap::from([(i, key_of(&w, i).await)]);
    let only_i = json!({"ops": [doc["ops"][0].clone()]});
    let rows = review_queue::diff_with_baseline(&mut c, w.alice_ctx, &only_i, &keys, Some(&forged))
        .await
        .unwrap();
    let by = |f: String| rows.iter().find(|r| r.field == f).unwrap().clone();
    let t = by(format!("cf:{tag}"));
    assert!(t.has_original);
    assert_eq!(t.original, json!("blue"));
    let s = by(format!("cf:{secret}"));
    assert!(!s.has_original);
    assert_eq!(s.original, Value::Null);
    assert_eq!(s.before, Value::Null);
    assert!(!serde_json::to_string(&rows).unwrap().contains("s3cret"));
    let subj = by("subject".into());
    assert!(subj.has_original);
    assert_eq!(subj.original, json!("with fields"));
    // Without a baseline nothing is claimed.
    let rows = review_queue::diff(&mut c, w.alice_ctx, &only_i, &keys)
        .await
        .unwrap();
    assert!(rows.iter().all(|r| !r.has_original));
}

#[tokio::test]
async fn g14_list_agent_tokens() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (token, agent) = w.token(ctx).await;
    let agent_id = agent.principal_id().unwrap();
    let (_t2, _) = w.token(ctx).await;

    let l = agents::list_context_credentials(&pool, &w.alice_ctx, ctx, None)
        .await
        .unwrap();
    assert_eq!(l.items.len(), 2);
    assert!(l.items.iter().all(|c| c.state == "active" && !c.revoked));
    assert!(
        l.items[0].created_at >= l.items[1].created_at,
        "newest first"
    );
    // Authenticating (w.token did) records the last use.
    assert!(l.items.iter().all(|c| c.last_used_at.is_some()));
    let text = serde_json::to_string(&l).unwrap();
    assert!(!text.contains(&token) && !text.contains("sha256") && !text.contains("\"token\""));
    // Exactly the documented fields, and never the stored hash.
    for item in serde_json::to_value(&l).unwrap()["items"]
        .as_array()
        .unwrap()
    {
        let keys: std::collections::BTreeSet<&str> = item
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "label",
                "context_id",
                "agent_principal_id",
                "created_at",
                "expires_at",
                "last_used_at",
                "revoked",
                "revoked_at",
                "state"
            ]
            .into_iter()
            .collect()
        );
    }
    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_sha256 FROM agent_credentials")
        .fetch_all(&pool)
        .await
        .unwrap();
    for h in hashes {
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!text.to_lowercase().contains(&hex));
    }
    let same = agents::list_context_credentials(&pool, &w.alice_ctx, ctx, Some(agent_id))
        .await
        .unwrap();
    assert_eq!(same.items, l.items);
    let e = agents::list_context_credentials(&pool, &w.alice_ctx, ctx, Some(w.bob))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);

    // last_used_at: at most one write a minute. `token` is the first one.
    let mine: Uuid =
        sqlx::query_scalar("SELECT id FROM agent_credentials ORDER BY created_at, id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(l.items[1].id, mine);
    let used = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
                "SELECT last_used_at FROM agent_credentials WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    exec(
        &pool,
        &format!("UPDATE agent_credentials SET last_used_at = now() - interval '30 seconds' WHERE id = '{mine}'"),
    )
    .await;
    let before = used(mine).await;
    auth::authenticate_bearer(&pool, &token)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(used(mine).await, before, "within a minute: no write");
    exec(
        &pool,
        &format!("UPDATE agent_credentials SET last_used_at = now() - interval '2 minutes' WHERE id = '{mine}'"),
    )
    .await;
    let before = used(mine).await;
    auth::authenticate_bearer(&pool, &token)
        .await
        .unwrap()
        .unwrap();
    assert!(used(mine).await > before);
    // A locked token row is skipped, never waited for; the bearer still
    // authenticates.
    exec(
        &pool,
        &format!("UPDATE agent_credentials SET last_used_at = now() - interval '2 minutes' WHERE id = '{mine}'"),
    )
    .await;
    let before = used(mine).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM agent_credentials WHERE id = $1 FOR UPDATE")
        .bind(mine)
        .execute(&mut *lock)
        .await
        .unwrap();
    let a = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        auth::authenticate_bearer(&pool, &token),
    )
    .await
    .expect("authentication does not wait for the row lock")
    .unwrap();
    assert!(a.is_some());
    lock.rollback().await.unwrap();
    assert_eq!(used(mine).await, before, "skipped while locked");

    // Revoked tokens stay listed, marked.
    agents::revoke_credential(&pool, &w.alice_ctx, mine)
        .await
        .unwrap();
    let l = ai_team::list_credentials(&w.app, w.alice_ctx, agent_id)
        .await
        .unwrap();
    let r = l.items.iter().find(|c| c.id == mine).unwrap();
    assert!(r.revoked && r.revoked_at.is_some());
    assert_eq!(r.state, "revoked");
    assert_eq!(l.items.len(), 2);

    // Not the sponsor: a member of the delegated project is told so (as
    // POST); someone who cannot see the role gets not_found; someone else's
    // context is not_found.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = ai_team::list_credentials(&w.app, bob, agent_id)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let dave = sign_in(&pool, &w.policy, "dave").await;
    let e = ai_team::list_credentials(&w.app, dave, agent_id)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = agents::list_context_credentials(&pool, &bob, ctx, None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // A site administrator reads the role's tokens (not the context alias,
    // which is the sponsor's own).
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let r = ai_team::list_credentials(&w.app, root, agent_id)
        .await
        .unwrap();
    assert_eq!(r.items, l.items);
    let e = agents::list_context_credentials(&pool, &root, ctx, None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

async fn direct(w: &World, agent: AuthContext, i: Uuid, subject: &str) -> Uuid {
    let d = json!({"command_schema_version": 1, "title": "agent write",
        "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": subject}}],
        "preconditions": [{"target": tgt(i), "lock_version": lv(w, i).await}]});
    match changesets::command_submit(&w.app, agent, Uuid::now_v7(), &d, false, ORIGIN)
        .await
        .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("not applied: {other:?}"),
    }
}

async fn now(w: &World) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn ids(a: &undo::Activity) -> Vec<Uuid> {
    a.items.iter().filter_map(|i| i.execution_id).collect()
}

#[tokio::test]
async fn g24_activity_filters_and_cursor() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(ctx).await;
    let agent_id = agent.principal_id().unwrap();
    let a = w.alice_ctx;
    let mut issues = Vec::new();
    let mut execs = Vec::new();
    let mut times = Vec::new();
    for n in 0..6 {
        let i = w.issue(w.web, "activity").await;
        execs.push(direct(&w, agent, i, &format!("written {n}")).await);
        times.push(now(&w).await);
        issues.push(i);
    }
    // e0 and e3 become unreadable for alice (archived issues).
    for k in [0, 3] {
        sqlx::query("UPDATE issues SET archived_at = now() WHERE id = $1")
            .bind(issues[k])
            .execute(&pool)
            .await
            .unwrap();
    }
    let visible = vec![execs[5], execs[4], execs[2], execs[1]];

    let page = |cursor: Option<String>| ActivityQuery {
        limit: Some(2),
        cursor,
        ..ActivityQuery::default()
    };
    let p1 = undo::activity_page(&w.app, a, agent_id, &page(None))
        .await
        .unwrap();
    assert_eq!(ids(&p1), visible[..2]);
    let c1 = p1.next_cursor.clone().expect("a second page");
    let p2 = undo::activity_page(&w.app, a, agent_id, &page(Some(c1.clone())))
        .await
        .unwrap();
    // e3 is skipped without shortening the page; the only item after e1 is
    // unreadable, so there is no third page.
    assert_eq!(ids(&p2), visible[2..]);
    assert_eq!(p2.next_cursor, None);
    let all = undo::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            limit: Some(4),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&all), visible);
    assert_eq!(all.next_cursor, None, "an unreadable item makes no page");

    // The cursor is signed, bound to the caller and to the filters.
    let mut forged = c1.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    for (who, q) in [
        (a, page(Some(forged))),
        (
            a,
            ActivityQuery {
                issue: Some(key_of(&w, issues[1]).await),
                ..page(Some(c1.clone()))
            },
        ),
        (
            sign_in(&pool, &w.policy, "bob").await,
            page(Some(c1.clone())),
        ),
    ] {
        let e = undo::activity_page(&w.app, who, agent_id, &q)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidCursor, "{e:#}");
    }

    // Time and key filters.
    let since = undo::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            from: Some(times[3]),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&since), vec![execs[5], execs[4]]);
    let until = undo::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            to: Some(times[2]),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&until), vec![execs[2], execs[1]]);
    let one = undo::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            issue: Some(key_of(&w, issues[4]).await),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&one), vec![execs[4]]);
    // An unreadable issue's key finds nothing.
    let gone = undo::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            issue: Some(key_of(&w, issues[3]).await),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert!(gone.items.is_empty() && gone.next_cursor.is_none());

    // The AI team page pages the same way.
    let t1 = ai_team::activity_page(&w.app, a, agent_id, &page(None))
        .await
        .unwrap();
    assert_eq!(ids(&t1), visible[..2]);
    let t2 = ai_team::activity_page(&w.app, a, agent_id, &page(t1.next_cursor.clone()))
        .await
        .unwrap();
    assert_eq!(ids(&t2), visible[2..]);
    let k = ai_team::activity_page(
        &w.app,
        a,
        agent_id,
        &ActivityQuery {
            issue: Some(key_of(&w, issues[2]).await),
            ..ActivityQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&k), vec![execs[2]]);

    // A cursor is bound to from and to as well.
    for q in [
        ActivityQuery {
            from: Some(times[0]),
            ..page(Some(c1.clone()))
        },
        ActivityQuery {
            to: Some(times[5]),
            ..page(Some(c1.clone()))
        },
    ] {
        let e = undo::activity_page(&w.app, a, agent_id, &q)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidCursor, "{e:#}");
    }

    // Ties on `at` are ordered by id (newest id first) and a page boundary
    // inside a tie neither skips nor repeats. The four readable writes and a
    // refusal (its id is the newest) all get the same time.
    let tie = times[5];
    let blocked: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind, at)
         VALUES ($1, $2, 'audience_blocked', $3) RETURNING id",
    )
    .bind(agent_id)
    .bind(ctx)
    .bind(tie)
    .fetch_one(&pool)
    .await
    .unwrap();
    // command_executions is append-only; the test rewrites the times only.
    let mut t = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE command_executions DISABLE TRIGGER command_executions_append_only")
        .execute(&mut *t)
        .await
        .unwrap();
    sqlx::query("UPDATE command_executions SET created_at = $2 WHERE id = ANY($1)")
        .bind(&visible)
        .bind(tie)
        .execute(&mut *t)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE command_executions ENABLE TRIGGER command_executions_append_only")
        .execute(&mut *t)
        .await
        .unwrap();
    t.commit().await.unwrap();
    let mut by_id = visible.clone();
    by_id.sort_by(|x, y| y.cmp(x));
    let mut seen_ids = Vec::new();
    let mut seen_blocked = Vec::new();
    let mut cursor = None;
    loop {
        let p = undo::activity_page(
            &w.app,
            a,
            agent_id,
            &ActivityQuery {
                limit: Some(2),
                cursor: cursor.take(),
                ..ActivityQuery::default()
            },
        )
        .await
        .unwrap();
        for it in &p.items {
            assert_eq!(it.at, tie);
            match it.execution_id {
                Some(id) => seen_ids.push(id),
                None => seen_blocked.push(seen_ids.len()),
            }
        }
        match p.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert!(blocked > by_id[0], "uuidv7: the refusal is newest");
    assert_eq!(
        seen_blocked,
        vec![0],
        "the refusal (largest id) comes first"
    );
    assert_eq!(seen_ids, by_id, "id order, nothing skipped or repeated");

    // limit: default 50, at most 200.
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind, at)
         SELECT $1, $2, 'audience_blocked', $3 - make_interval(secs => g)
         FROM generate_series(1, 260) g",
    )
    .bind(agent_id)
    .bind(ctx)
    .bind(tie)
    .execute(&pool)
    .await
    .unwrap();
    for (limit, n) in [(None, 50), (Some(1000), 200), (Some(200), 200)] {
        let p = undo::activity_page(
            &w.app,
            a,
            agent_id,
            &ActivityQuery {
                limit,
                ..ActivityQuery::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(p.items.len(), n, "{limit:?}");
        assert!(p.next_cursor.is_some());
    }
}
