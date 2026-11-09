// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S8: read-your-writes for search (design 03 §4): T-S1, T-W4's
//! "frontier moves → satisfied" half, and the token checks of 03 §4.11
//! (format, epoch, wait cap, waiter limits, SQL-only). Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ApiError, ErrorCode};
use oxledger::issue_view::{self, GetParams};
use oxledger::search::{MAX_WAITERS_PER_CALLER, Wait};
use oxledger::service::{self, SearchParams, SearchResult, parse_token};
use serde_json::json;
use std::time::Duration;
use uuid::Uuid;
use world::*;

/// The index worker, by hand: rounds until F passes `x` (another
/// database's open transaction can hold H back for a while; playbook).
async fn catch_up(w: &World, x: u64) {
    for _ in 0..500 {
        w.app.index.sync(&w.pool).await.unwrap();
        if x < w.app.index.frontier() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("frontier did not pass {x} within 10 s");
}

async fn lv(pool: &sqlx::PgPool, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn set_subject(w: &World, i: Uuid, subject: &str) -> CommandResult {
    let lv = lv(&w.pool, i).await;
    let d = json!({"command_schema_version": 1,
        "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": subject}}],
        "preconditions": [{"target": tgt(i), "lock_version": lv}]});
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
        .await
        .unwrap()
}

fn params(q: Option<&str>, token: &str, wait_ms: u32) -> SearchParams {
    SearchParams {
        q: q.map(String::from),
        project: Some("AUTH".into()),
        consistent_after: Some(token.into()),
        wait_ms: Some(wait_ms),
        ..Default::default()
    }
}

async fn search(w: &World, p: SearchParams) -> anyhow::Result<SearchResult> {
    service::search(&w.app, w.alice_ctx, p).await
}

#[tokio::test]
async fn search_waits_for_its_write() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.auth_p, "first subject").await;
    w.app.index.sync_all(&pool).await.unwrap();

    // T-S1: the write succeeds while the index worker is stopped.
    let r = set_subject(&w, i, "platypuszebra rollout").await;
    assert_eq!(r.status, "succeeded");
    let token = r.search.consistent_after.clone();
    let (_, x) = parse_token(&token).expect("03 token format (T-S10)");
    let key = r.effects[0].key.clone();
    let got = issue_view::get(
        &w.app,
        w.alice_ctx,
        GetParams {
            key: key.clone(),
            fields: None,
            citations: None,
            include: None,
            note_limit: None,
            event_limit: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        got.subject.as_deref(),
        Some("platypuszebra rollout"),
        "get reads it"
    );

    // With the token: lagging, no items, no cursor, not complete.
    for wait in [0, 120] {
        let started = std::time::Instant::now();
        let s = search(&w, params(Some("platypuszebra"), &token, wait))
            .await
            .unwrap();
        assert_eq!(s.outcome, "search_lagging");
        assert!(!s.complete);
        assert!(s.items.is_empty() && s.next_cursor.is_none());
        assert_eq!(s.consistency.status, "lagging");
        assert_eq!(s.consistency.retry_after_ms, Some(500));
        assert!(started.elapsed() >= Duration::from_millis(wait as u64));
        assert!(started.elapsed() < Duration::from_millis(wait as u64 + 1500));
    }
    // on_lag: error.
    let mut p = params(Some("platypuszebra"), &token, 0);
    p.on_lag = Some("error".into());
    let e = search(&w, p).await.unwrap_err();
    let api = e.downcast_ref::<ApiError>().unwrap();
    assert_eq!(api.code, ErrorCode::SearchNotCaughtUp);
    assert_eq!(api.retry_after_ms, Some(500));
    assert!(api.code.retryable());

    // Without the token the old index answers (eventual), never an error.
    let mut p = params(Some("platypuszebra"), &token, 0);
    p.consistent_after = None;
    let s = search(&w, p).await.unwrap();
    assert_eq!(
        (s.outcome.as_str(), s.consistency.status.as_str()),
        ("ok", "eventual")
    );
    assert!(s.items.is_empty(), "the index has not seen it");

    // SQL only: not_applicable, no wait, reads its own commit.
    let s = search(&w, params(None, &token, 2000)).await.unwrap();
    assert_eq!(s.consistency.status, "not_applicable");
    assert_eq!(s.consistency.reason.as_deref(), Some("sql_only"));
    assert!(s.items.iter().any(|it| it.key == key));

    // A waiting search passes as soon as the worker publishes F > X, and
    // then shows the write.
    let app = w.app.clone();
    let ctx = w.alice_ctx;
    let t = token.clone();
    let waiting = tokio::spawn(async move {
        service::search(&app, ctx, params(Some("platypuszebra"), &t, 2000)).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    catch_up(&w, x).await;
    let s = waiting.await.unwrap().unwrap();
    assert_eq!(s.outcome, "ok");
    assert_eq!(s.consistency.status, "satisfied");
    assert_eq!(s.items.len(), 1);
    assert_eq!(s.items[0].key, key);
    // And from now on at once.
    let s = search(&w, params(Some("platypuszebra"), &token, 0))
        .await
        .unwrap();
    assert_eq!(s.consistency.status, "satisfied");
    assert_eq!(s.consistency.waited_ms, Some(0));

    // T-W4 / 03 §4.11: an all-no_effect write still gets a token that the
    // frontier satisfies (zero outbox rows).
    let r = set_subject(&w, i, "platypuszebra rollout").await;
    assert!(r.effects.iter().all(|e| !e.changed));
    let (_, x0) = parse_token(&r.search.consistent_after).unwrap();
    catch_up(&w, x0).await;
    let s = search(
        &w,
        params(Some("platypuszebra"), &r.search.consistent_after, 0),
    )
    .await
    .unwrap();
    assert_eq!(s.consistency.status, "satisfied");
}

#[tokio::test]
async fn token_checks_and_waiter_limits() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.auth_p, "token checks").await;
    let r = set_subject(&w, i, "token checks two").await;
    let token = r.search.consistent_after.clone();
    let (epoch, x) = parse_token(&token).unwrap();

    // Format.
    for bad in [
        "",
        "c1",
        "c2.AAAAAAAAAAAAAAAAAAAAAA.1",
        "c1.not-base64!.1",
        "c1.AAAA.1",
        &format!("{}.x", token.rsplit_once('.').unwrap().0),
        &format!("{}.-1", token.rsplit_once('.').unwrap().0),
        &format!(
            "{}.99999999999999999999999",
            token.rsplit_once('.').unwrap().0
        ),
        &format!("{token}.1"),
    ] {
        let e = search(&w, params(Some("token"), bad, 0)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidConsistencyToken, "{bad:?}");
    }
    // Epoch: another data epoch is not this world's (also without q).
    let other = oxledger::commands::token(Uuid::now_v7(), &x.to_string());
    for q in [Some("token"), None] {
        let e = search(&w, params(q, &other, 0)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::ConsistencyEpochChanged);
        assert!(!code(&e).retryable());
    }
    // Rotating the epoch (03 §4.3) turns the old token into the same error.
    exec(&pool, "UPDATE search_settings SET data_epoch = uuidv7()").await;
    let e = search(&w, params(Some("token"), &token, 0))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ConsistencyEpochChanged);
    sqlx::query("UPDATE search_settings SET data_epoch = $1")
        .bind(epoch)
        .execute(&pool)
        .await
        .unwrap();

    // Parameters.
    let e = search(&w, params(Some("token"), &token, 2001))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let mut p = params(Some("token"), &token, 0);
    p.on_lag = Some("later".into());
    assert_eq!(
        code(&search(&w, p).await.unwrap_err()),
        ErrorCode::InvalidArgument
    );

    // A forged huge X only waits wait_ms; per-caller waiters are capped and
    // released however the wait ends.
    let huge = oxledger::commands::token(epoch, &(u64::MAX / 2).to_string());
    let mut held = Vec::new();
    for _ in 0..MAX_WAITERS_PER_CALLER {
        let app = w.app.clone();
        let ctx = w.alice_ctx;
        let t = huge.clone();
        held.push(tokio::spawn(async move {
            service::search(&app, ctx, params(Some("token"), &t, 1500)).await
        }));
    }
    for _ in 0..100 {
        if w.app.index.waiting() == MAX_WAITERS_PER_CALLER {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(w.app.index.waiting(), MAX_WAITERS_PER_CALLER);
    let started = std::time::Instant::now();
    let s = search(&w, params(Some("token"), &huge, 1500))
        .await
        .unwrap();
    assert_eq!(s.outcome, "search_lagging");
    assert_eq!(s.consistency.waited_ms, Some(0));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "no slot: at once"
    );
    // Someone else still gets a slot.
    let other_caller = w
        .app
        .index
        .wait_for(Some(w.bob), u64::MAX / 2, Duration::from_millis(30));
    assert!(matches!(other_caller.await, Wait::Lagging { waited_ms } if waited_ms >= 30));
    // Cancelling a waiter frees its slot.
    held.pop().unwrap().abort();
    for _ in 0..100 {
        if w.app.index.waiting() < MAX_WAITERS_PER_CALLER {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(w.app.index.waiting(), MAX_WAITERS_PER_CALLER - 1);
    for h in held {
        let s = h.await.unwrap().unwrap();
        assert_eq!(s.outcome, "search_lagging");
        assert!(s.consistency.waited_ms.unwrap() >= 1500);
    }
    assert_eq!(w.app.index.waiting(), 0);
}

/// 03 §4.11 frontier: one transaction with more than a batch of outbox rows
/// (500, 501 and 1,500) never lets F pass it before all are applied.
#[tokio::test]
async fn frontier_never_passes_a_partly_applied_xid() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.auth_p, "frontier probe").await;
    w.app.index.sync_all(&pool).await.unwrap();
    for n in [500, 501, 1500] {
        let mut tx = pool.begin().await.unwrap();
        let x: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO outbox (topic, entity_id) SELECT 'issue', $1 FROM generate_series(1, $2)",
        )
        .bind(i)
        .bind(n)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let x: u64 = x.parse().unwrap();
        let mut rounds = 0;
        loop {
            // One round at a time, so a partly applied xid is observable.
            w.app.index.sync_round(&pool).await.unwrap();
            rounds += 1;
            let left: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM outbox o, outbox_cursors c
                 WHERE c.consumer LIKE 'search@%' AND c.state = 'active' AND o.topic = 'issue' AND (o.xid, o.id) > (c.last_xid, c.last_id)
                   AND o.xid = $1::text::xid8",
            )
            .bind(x.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if left > 0 {
                assert!(
                    w.app.index.frontier() <= x,
                    "F passed xid {x} with {left} rows of it left (n = {n})"
                );
            } else if x < w.app.index.frontier() {
                break;
            }
            assert!(rounds < 500, "stuck at n = {n}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
