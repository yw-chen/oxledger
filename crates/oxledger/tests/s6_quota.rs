// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6: agent quotas and the circuit breaker (§8.9, §16.6).
//! The three tiers, replays not counted, agent-wide counting across
//! contexts, one write per context at a time, T-D5 (both breaker triggers),
//! the delegator's pause and resume. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents;
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, SubmitResult, faults};
use oxledger::error::{ErrorCode, NextAction, classify};
use oxledger::quota::limits;
use oxledger::service::{self, SearchParams};
use serde_json::Value;
use uuid::Uuid;
use world::*;

async fn lv(w: &World, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn submit(w: &World, agent: AuthContext, doc: &Value) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), doc, false, ORIGIN).await
}

async fn write(w: &World, agent: AuthContext, i: Uuid, s: &str) -> anyhow::Result<SubmitResult> {
    let d = subject_doc(i, lv(w, i).await, s);
    submit(w, agent, &d).await
}

/// Earlier writes by `agent`, `ago` seconds back (so a test does not wait).
async fn backfill(
    w: &World,
    agent: Uuid,
    context: Uuid,
    kind: &str,
    issue: Option<Uuid>,
    n: i64,
    ago: i64,
) {
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind, issue_id, at)
         SELECT $1, $2, $3, $4, clock_timestamp() - make_interval(secs => $5) FROM generate_series(1, $6)",
    )
    .bind(agent)
    .bind(context)
    .bind(kind)
    .bind(issue)
    .bind(ago as f64)
    .bind(n as i32)
    .execute(&w.pool)
    .await
    .unwrap();
}

async fn clear_log(w: &World) {
    exec(&w.pool, "DELETE FROM agent_write_log").await;
}

fn suspended_reason(e: &anyhow::Error) -> String {
    let a = classify(e).unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(a.code, ErrorCode::AgentSuspended, "{e:#}");
    let d = a.details.unwrap();
    assert_eq!(d["scope"], "context");
    assert_eq!(
        a.code.next_actions(),
        [NextAction::AskHuman, NextAction::Stop]
    );
    d["reason"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn s6_quota() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let (web, auth_p) = (w.web, w.auth_p);
    let i1 = w.issue(web, "one").await;
    let i2 = w.issue(web, "two").await;
    let ia = w.issue(auth_p, "auth").await;
    let d = w.delegate(web).await.unwrap();
    let ctx = d.context_id;
    let agent_id = d.agent_principal_id;
    let (_, agent) = w.token(ctx).await;
    // Every context has its control row.
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_context_write_controls WHERE context_id = '{ctx}'"
            )
        )
        .await,
        1
    );

    // ------------------------------------------------ tier 1: per-minute rate → rate_limited
    backfill(
        &w,
        agent_id,
        ctx,
        "direct_write",
        None,
        limits::DIRECT_PER_MINUTE - 1,
        5,
    )
    .await;
    let key = Uuid::now_v7();
    let doc = subject_doc(i1, lv(&w, i1).await, "the 10th");
    changesets::command_submit(&w.app, agent, key, &doc, false, ORIGIN)
        .await
        .unwrap();
    // A replay is not counted again.
    changesets::command_submit(&w.app, agent, key, &doc, false, ORIGIN)
        .await
        .unwrap();
    let e = write(&w, agent, i2, "the 11th").await.unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(a.code, ErrorCode::RateLimited, "{e:#}");
    assert_eq!(a.retry_after_ms, Some(60_000));
    assert_eq!(a.code.next_actions(), [NextAction::WaitAndRetry]);
    assert!(a.code.retryable());
    // Agent-wide: a new context does not reset it (§16.6).
    let ctx2 = w.new_context().await;
    w.delegate(web).await.unwrap();
    let (_, agent) = w.token(ctx2).await;
    assert_eq!(
        code(&write(&w, agent, i2, "new context").await.unwrap_err()),
        ErrorCode::RateLimited
    );
    let ctx = ctx2;

    // ------------------------------------------------ tier 2: hourly quotas → quota_exceeded, no suspension
    clear_log(&w).await;
    backfill(
        &w,
        agent_id,
        ctx,
        "direct_write",
        None,
        limits::DIRECT_PER_HOUR,
        1800,
    )
    .await;
    let e = write(&w, agent, i2, "hourly").await.unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(
        (a.code, a.details.as_ref().unwrap()["kind"].as_str()),
        (ErrorCode::QuotaExceeded, Some("direct_writes_per_hour"))
    );
    assert_eq!(
        a.code.next_actions(),
        [NextAction::AskHuman, NextAction::Stop]
    );
    assert!(!a.code.retryable());
    clear_log(&w).await;
    backfill(
        &w,
        agent_id,
        ctx,
        "issue_create",
        None,
        limits::CREATES_PER_HOUR,
        1800,
    )
    .await;
    let create = serde_json::json!({"command_schema_version": 1, "title": "t",
        "ops": [{"op_id": op(), "kind": "create_issue", "project_id": web, "tracker_id": w.bug, "fields": {"subject": "n"}}]});
    let e = submit(&w, agent, &create).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded);
    // Updates still go (only creating is used up), and nothing was suspended.
    write(&w, agent, i2, "still fine").await.unwrap();
    let susp: Option<String> = sqlx::query_scalar(
        "SELECT suspension_reason FROM agent_context_write_controls WHERE context_id = $1",
    )
    .bind(ctx)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(susp, None, "a normal quota never suspends");
    // Proposals: per delegation per hour.
    clear_log(&w).await;
    let deleg: Uuid = sqlx::query_scalar("SELECT id FROM delegations WHERE context_id = $1")
        .bind(ctx)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
         SELECT $1, $2, $3, 'propose' FROM generate_series(1, $4)",
    )
    .bind(agent_id)
    .bind(ctx)
    .bind(deleg)
    .bind(limits::PROPOSALS_PER_HOUR as i32)
    .execute(&pool)
    .await
    .unwrap();
    let e = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, lv(&w, i1).await, "p"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded);
    clear_log(&w).await;

    // ------------------------------------------------ one write per context at a time
    let mut hold = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM agent_context_write_controls WHERE context_id = $1 FOR UPDATE")
        .bind(ctx)
        .execute(&mut *hold)
        .await
        .unwrap();
    let e = write(&w, agent, i2, "concurrent").await.unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(a.code, ErrorCode::RateLimited);
    assert_eq!(a.details.as_ref().unwrap()["kind"], "concurrent_write");
    hold.rollback().await.unwrap();

    // ------------------------------------------------ T-D5 (a): one issue written too often
    backfill(
        &w,
        agent_id,
        ctx,
        "issue_write",
        Some(i2),
        limits::ISSUE_WRITES_PER_10_MIN,
        300,
    )
    .await;
    let notes_before = count(&pool, "SELECT count(*) FROM issue_events").await;
    let e = write(&w, agent, i2, "burst").await.unwrap_err();
    assert_eq!(suspended_reason(&e), "issue_write_burst");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_events").await,
        notes_before,
        "no domain change"
    );
    // The suspension committed: every write is refused, proposals too …
    let e = write(&w, agent, i1, "other issue").await.unwrap_err();
    assert_eq!(suspended_reason(&e), "issue_write_burst");
    let e = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i1, lv(&w, i1).await, "p"),
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(suspended_reason(&e), "issue_write_burst");
    // … reads go on.
    service::search(&w.app, agent, SearchParams::default())
        .await
        .unwrap();
    // Only the delegator resumes it (a step-up session; the agent cannot).
    assert!(
        agents::resume_context(&pool, &w.policy, &agent, ctx)
            .await
            .is_err()
    );
    agents::resume_context(&pool, &w.policy, &w.alice_ctx, ctx)
        .await
        .unwrap();
    clear_log(&w).await;
    write(&w, agent, i1, "resumed").await.unwrap();

    // ------------------------------------------------ T-D5 (b): three audience refusals in an hour
    w.delegate(auth_p).await.unwrap(); // R = {WEB, AUTH}: WEB's public fields are blocked
    for n in 1..=limits::AUDIENCE_BLOCKS_PER_HOUR {
        let e = write(&w, agent, i1, "leak").await.unwrap_err();
        if n < limits::AUDIENCE_BLOCKS_PER_HOUR {
            assert_eq!(code(&e), ErrorCode::AudienceExpansionForbidden);
        } else {
            assert_eq!(suspended_reason(&e), "audience_blocked");
        }
    }
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM agent_write_log WHERE kind = 'audience_blocked'"
        )
        .await,
        limits::AUDIENCE_BLOCKS_PER_HOUR
    );
    // Writes to AUTH (allowed by audience) are refused too: the context is paused.
    let e = write(&w, agent, ia, "auth").await.unwrap_err();
    assert_eq!(suspended_reason(&e), "audience_blocked");
    // Concurrent writes of a suspended context do not deadlock.
    let mut ts = Vec::new();
    for n in 0..4 {
        let (app, d) = (
            w.app.clone(),
            subject_doc(ia, lv(&w, ia).await, &format!("x{n}")),
        );
        ts.push(tokio::spawn(async move {
            changesets::command_submit(&app, agent, Uuid::now_v7(), &d, false, ORIGIN).await
        }));
    }
    for t in ts {
        // No deadlock: every submit ends (bounded, the timeout reports the state).
        let e = common::join("a concurrent submit", t).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::AgentSuspended);
    }

    // ------------------------------------------------ the delegator's pause button
    agents::resume_context(&pool, &w.policy, &w.alice_ctx, ctx)
        .await
        .unwrap();
    clear_log(&w).await;
    write(&w, agent, ia, "after resume").await.unwrap();
    agents::suspend_context(&pool, &w.alice_ctx, ctx)
        .await
        .unwrap();
    assert_eq!(
        suspended_reason(&write(&w, agent, ia, "paused").await.unwrap_err()),
        "manual"
    );
    // P1 gap G3: the delegation page sees the pause.
    let views = agents::list_contexts(&pool, &w.alice_ctx).await.unwrap();
    let v = views.iter().find(|c| c.id == ctx).unwrap();
    assert!(v.suspended_at.is_some());
    assert_eq!(v.suspension_reason.as_deref(), Some("manual"));
    faults::clear();
}
