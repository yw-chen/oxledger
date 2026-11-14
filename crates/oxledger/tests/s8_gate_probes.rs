// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S8 gate, from the independent verification: in-flight
//! transactions and the frontier (M06/M11/M12), waiter slots (M09/M10), the
//! reload-before-publish barrier (M01), cursors bound to version filters,
//! version:next skipping deleted / locked versions, input errors winning over
//! lagging, and the reserved name "next". Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::{Break, Continue, WAIT_TIMEOUT, poll_until, test_pool, wait_for};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::error::{ApiError, ErrorCode};
use oxledger::search::{MAX_WAITERS, MAX_WAITERS_PER_CALLER, Wait};
use oxledger::service::{self, SearchParams, SearchResult};
use oxledger::versions::{self, NewVersion};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use world::*;

async fn version(w: &World, project: &str, name: &str) -> Uuid {
    versions::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        project,
        &NewVersion {
            name: name.into(),
            effective_date: None,
            description: None,
            status: None,
        },
    )
    .await
    .unwrap()
    .version
    .id
}

async fn search(w: &World, p: SearchParams) -> anyhow::Result<SearchResult> {
    service::search(&w.app, w.alice_ctx, p).await
}

fn shape(e: &anyhow::Error) -> (String, Value, String) {
    let a = e.downcast_ref::<ApiError>().expect("api error");
    (
        a.code.as_str().to_string(),
        a.details.clone().unwrap_or(Value::Null),
        a.message.clone(),
    )
}

async fn ops_project(w: &World) -> Uuid {
    let ops = id(
        &w.pool,
        "INSERT INTO projects (key, name) VALUES ('OPS','Ops') RETURNING id",
    )
    .await;
    exec(
        &w.pool,
        &format!("INSERT INTO enabled_modules VALUES ('{ops}', 'issue_tracking')"),
    )
    .await;
    ops
}

/// Visibility: `version` by name in an invisible project vs a missing one,
/// and by id for an invisible version vs a random uuid.
#[tokio::test]
async fn version_visibility_shapes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let ops = ops_project(&w).await;
    let o20 = id(
        &pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, created_by, last_event_seq)
             VALUES ('{ops}', 'v2.0', 'v2.0', '{}', 0) RETURNING id",
            w.alice
        ),
    )
    .await;
    let p = |project: Option<&str>, v: &str| SearchParams {
        project: project.map(String::from),
        version: Some(v.into()),
        ..Default::default()
    };
    let a = search(&w, p(Some("OPS"), "v2.0")).await.unwrap_err();
    let b = search(&w, p(Some("ZZZ"), "v2.0")).await.unwrap_err();
    let (ca, da, ma) = shape(&a);
    let (cb, db, mb) = shape(&b);
    println!("invisible: {ca} {da} {ma}\nmissing: {cb} {db} {mb}");
    assert_eq!(ca, cb);
    assert_eq!(
        da.to_string().replace("OPS", "X"),
        db.to_string().replace("ZZZ", "X")
    );
    // ids: invisible version vs random uuid.
    let r1 = search(&w, p(None, &o20.to_string())).await.unwrap();
    let r2 = search(&w, p(None, &Uuid::now_v7().to_string()))
        .await
        .unwrap();
    assert!(r1.items.is_empty() && r2.items.is_empty());
    assert_eq!(
        serde_json::to_value(&r1).unwrap(),
        serde_json::to_value(&r2).unwrap()
    );
    // Filter AST with the invisible id.
    let f = SearchParams {
        filter: Some(serde_json::json!({"field": "fixed_version_id", "op": "eq", "value": o20})),
        ..Default::default()
    };
    assert!(search(&w, f).await.unwrap().items.is_empty());
}

/// A page cursor from one version filter must not continue another.
#[tokio::test]
async fn cursor_bound_to_version() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let v1 = version(&w, "AUTH", "p1").await;
    let v2 = version(&w, "AUTH", "p2").await;
    for (v, n) in [(v1, 3), (v2, 3)] {
        for k in 0..n {
            let _ = k;
            let i = w.issue(w.auth_p, "cursor probe").await;
            sqlx::query("UPDATE issues SET fixed_version_id = $1 WHERE id = $2")
                .bind(v)
                .bind(i)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    let p = |v: Uuid, cursor: Option<String>| SearchParams {
        project: Some("AUTH".into()),
        version: Some(v.to_string()),
        limit: Some(1),
        cursor,
        ..Default::default()
    };
    let r = search(&w, p(v1, None)).await.unwrap();
    let c = r.next_cursor.clone().expect("cursor");
    let e = search(&w, p(v2, Some(c.clone()))).await;
    assert!(
        e.is_err(),
        "cursor from version p1 accepted for p2: {:?}",
        e.map(|r| r.items.len())
    );
    let o = SearchParams {
        project: Some("AUTH".into()),
        overdue: Some(true),
        limit: Some(1),
        cursor: Some(c),
        version: Some(v1.to_string()),
        ..Default::default()
    };
    assert!(
        search(&w, o).await.is_err(),
        "cursor accepted after adding overdue"
    );
}

/// Per-caller slots come back after each wait (M09).
#[tokio::test]
async fn per_caller_slots_released() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    for n in 0..MAX_WAITERS_PER_CALLER * 2 {
        let r = w
            .app
            .index
            .wait_for(Some(w.alice), u64::MAX / 2, Duration::from_millis(20))
            .await;
        assert!(
            matches!(r, Wait::Lagging { waited_ms } if waited_ms >= 20),
            "wait #{n} returned at once: {r:?} (per-caller slot leaked)"
        );
    }
}

/// Global cap (M10): 1000 waiters from distinct callers, the next one lags at once.
#[tokio::test]
async fn global_waiter_cap() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let mut held = Vec::new();
    // The waiters outlive the bounded wait below; they are aborted at the end.
    for _ in 0..MAX_WAITERS {
        let app = w.app.clone();
        held.push(tokio::spawn(async move {
            app.index
                .wait_for(Some(Uuid::now_v7()), u64::MAX / 2, WAIT_TIMEOUT * 2)
                .await
        }));
    }
    poll_until("every waiter to hold a slot", WAIT_TIMEOUT, async || {
        let n = w.app.index.waiting();
        if n == MAX_WAITERS {
            Break(())
        } else {
            Continue(format!("{n} waiting"))
        }
    })
    .await;
    let r = w
        .app
        .index
        .wait_for(
            Some(Uuid::now_v7()),
            u64::MAX / 2,
            Duration::from_millis(300),
        )
        .await;
    for h in held {
        h.abort();
    }
    assert_eq!(r, Wait::Lagging { waited_ms: 0 });
}

/// X < F, never X <= F; H is the snapshot xmin (M06, M11): an in-progress
/// transaction's token is not satisfied, and neither are later commits
/// while it is open; an older open transaction's rows are not skipped (M12).
#[tokio::test]
async fn inflight_transactions() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.auth_p, "inflight a").await;
    let b = w.issue(w.auth_p, "inflight b").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Older transaction A stays open.
    let mut ta = pool.begin().await.unwrap();
    let xa: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
        .fetch_one(&mut *ta)
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET subject = 'quokkaalpha' WHERE id = $1")
        .bind(a)
        .execute(&mut *ta)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox (topic, entity_id) VALUES ('issue', $1)")
        .bind(a)
        .execute(&mut *ta)
        .await
        .unwrap();
    // Newer transaction B commits.
    let mut tb = pool.begin().await.unwrap();
    let xb: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
        .fetch_one(&mut *tb)
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET subject = 'quokkabeta' WHERE id = $1")
        .bind(b)
        .execute(&mut *tb)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox (topic, entity_id) VALUES ('issue', $1)")
        .bind(b)
        .execute(&mut *tb)
        .await
        .unwrap();
    tb.commit().await.unwrap();
    for _ in 0..3 {
        w.app.index.sync(&pool).await.unwrap();
    }
    let xa: u64 = xa.parse().unwrap();
    let xb: u64 = xb.parse().unwrap();
    let f = w.app.index.frontier();
    println!("xa={xa} xb={xb} F={f}");
    assert!(f <= xa, "F={f} passed open xid {xa}");
    let s = |q: &str, x: u64| SearchParams {
        q: Some(q.into()),
        project: Some("AUTH".into()),
        consistent_after: Some(oxledger::commands::token(epoch, &x.to_string())),
        wait_ms: Some(0),
        ..Default::default()
    };
    let r = search(&w, s("quokkaalpha", xa)).await.unwrap();
    assert_eq!(
        r.outcome, "search_lagging",
        "token of an open transaction satisfied"
    );
    let r = search(&w, s("quokkabeta", xb)).await.unwrap();
    assert_eq!(
        r.outcome, "search_lagging",
        "B satisfied while older A open"
    );
    ta.commit().await.unwrap();
    poll_until(
        &format!("the frontier to pass xid {xb}"),
        WAIT_TIMEOUT,
        async || {
            w.app.index.sync(&pool).await.unwrap();
            let f = w.app.index.frontier();
            if xb < f {
                Break(())
            } else {
                Continue(format!("F={f}"))
            }
        },
    )
    .await;
    let r = search(&w, s("quokkaalpha", xa)).await.unwrap();
    assert_eq!(r.consistency.status, "satisfied");
    assert_eq!(r.items.len(), 1, "A's event was skipped");
    let r = search(&w, s("quokkabeta", xb)).await.unwrap();
    assert_eq!(r.items.len(), 1);
}

/// version:next skips deleted and locked/closed versions (M26, M27), and
/// get shows status / effective_date (M39, M40).
#[tokio::test]
async fn next_skips_deleted_and_locked() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let t: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
            .fetch_one(&pool)
            .await
            .unwrap();
    let mk = |name: &str, days: i64, status: &str, deleted: bool| {
        format!(
            "INSERT INTO versions (project_id, name, name_key, created_by, last_event_seq, effective_date, status, deleted_at)
             VALUES ('{}', '{name}', '{name}', '{}', 0, '{}', '{status}', {}) RETURNING id",
            w.auth_p,
            w.alice,
            t + chrono::Duration::days(days),
            if deleted { "now()" } else { "NULL" }
        )
    };
    let _gone = id(&pool, &mk("gone", 1, "open", true)).await;
    let _locked = id(&pool, &mk("locked", 2, "locked", false)).await;
    let _closed = id(&pool, &mk("closed", 3, "closed", false)).await;
    let real = id(&pool, &mk("real", 9, "open", false)).await;
    let i = w.issue(w.auth_p, "next probe").await;
    sqlx::query("UPDATE issues SET fixed_version_id = $1 WHERE id = $2")
        .bind(real)
        .bind(i)
        .execute(&pool)
        .await
        .unwrap();
    let r = search(
        &w,
        SearchParams {
            project: Some("AUTH".into()),
            version: Some("next".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r.items.len(), 1);
    let key: String =
        sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap();
    let got = oxledger::issue_view::get(
        &w.app,
        w.alice_ctx,
        oxledger::issue_view::GetParams {
            key,
            fields: Some(vec!["fixed_version".into()]),
            citations: None,
            include: None,
            note_limit: None,
            event_limit: None,
        },
    )
    .await
    .unwrap();
    let fv = got.fixed_version.unwrap().unwrap();
    assert_eq!(fv.status, "open");
    assert_eq!(
        fv.effective_date,
        Some((t + chrono::Duration::days(9)).to_string())
    );
}

/// A lagging token does not hide an invalid request: the input error comes
/// back, not "wait and retry" (verification finding 5).
#[tokio::test]
async fn input_errors_win_over_lagging() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    let huge = oxledger::commands::token(epoch, &(u64::MAX / 2).to_string());
    for on_lag in [None, Some("error")] {
        let p = SearchParams {
            q: Some("anything".into()),
            project: Some("NOPE".into()),
            consistent_after: Some(huge.clone()),
            wait_ms: Some(0),
            on_lag: on_lag.map(String::from),
            ..Default::default()
        };
        let e = search(&w, p).await.expect_err("an input error");
        let (code, _, _) = shape(&e);
        assert_ne!(code, ErrorCode::SearchNotCaughtUp.as_str());
    }
    // A valid request still lags.
    let p = SearchParams {
        q: Some("anything".into()),
        project: Some("AUTH".into()),
        consistent_after: Some(huge),
        wait_ms: Some(0),
        ..Default::default()
    };
    assert_eq!(search(&w, p).await.unwrap().outcome, "search_lagging");
}

/// "next" is the keyword of version:next, so no version may take that name
/// (verification finding 6).
#[tokio::test]
async fn next_is_a_reserved_version_name() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    for name in ["next", "Next", " NEXT "] {
        let e = versions::create(
            &w.app,
            w.alice_ctx,
            Uuid::now_v7(),
            "AUTH",
            &NewVersion {
                name: name.into(),
                effective_date: None,
                description: None,
                status: None,
            },
        )
        .await
        .expect_err("reserved");
        let (code, d, _) = shape(&e);
        assert_eq!(code, ErrorCode::InvalidValue.as_str());
        assert_eq!(d["kind"], json!("reserved_name"), "{d}");
    }
    version(&w, "AUTH", "next-gen").await;
}

/// 03 §4.11: after the reader shows the new documents but before F is
/// published, a waiter does not pass (M01).
#[tokio::test]
async fn no_waiter_passes_before_publish() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    faults::clear();
    w.app.index.sync_all(&pool).await.unwrap();
    let i = w.issue(w.auth_p, "barrier wombatquill").await;
    let x: String = sqlx::query_scalar(
        "SELECT max(xid)::text FROM outbox WHERE topic = 'issue' AND entity_id = $1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    let x: u64 = x.parse().unwrap();
    // The round below only reaches the gate if it applies this row, and it
    // skips rows at or above H (03 §4.7): any open transaction on the
    // cluster older than x would make it a no-op and leave the gate
    // unreached (T1b; the 9-hour hang on main). Wait until H passes x
    // first; H never moves back, so the round then sees the row.
    common::wait_horizon_past(&pool, x).await;
    let g = Arc::new(Gate::default());
    faults::arm("search.before_publish", Action::Block(g.clone()));
    let (app, p2) = (w.app.clone(), pool.clone());
    let worker = tokio::spawn(async move { app.index.sync_round(&p2).await });
    g.reached.notified().await;
    // The reader already has the document; F does not cover it yet.
    assert!(w.app.index.frontier() <= x);
    let r = w
        .app
        .index
        .wait_for(Some(w.alice), x, Duration::from_millis(80))
        .await;
    assert!(matches!(r, Wait::Lagging { .. }), "{r:?}");
    g.release.notify_one();
    wait_for("the released sync round to finish", WAIT_TIMEOUT, worker)
        .await
        .unwrap()
        .unwrap();
    let r = w
        .app
        .index
        .wait_for(Some(w.alice), x, Duration::from_millis(2000))
        .await;
    assert!(matches!(r, Wait::Satisfied { .. }), "{r:?}");
    faults::clear();
}
