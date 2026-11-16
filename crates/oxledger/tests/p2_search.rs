// SPDX-License-Identifier: GPL-2.0-or-later
//! P2 search wrap-up (design 03 §4.2, §4.8–4.9, §17.15 fast fields,
//! `search_rev`): durable payload and restart, blue-green rebuild, outbox
//! GC, index pre-filters. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::error::ErrorCode;
use oxledger::search::{Cursor, SearchIndex, Wait, switch_ready};
use oxledger::service::{self, App, SearchParams};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use world::*;

fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("oxledger-p2-{}", Uuid::now_v7()))
}

/// An app on `index`, sharing the world's database.
fn app_with(w: &World, index: SearchIndex) -> Arc<App> {
    Arc::new(App {
        pool: w.pool.clone(),
        index,
        wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
    })
}

async fn found(app: &App, w: &World, q: &str) -> Vec<String> {
    let mut keys: Vec<String> = service::search(
        app,
        w.alice_ctx,
        SearchParams {
            q: Some(q.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
    .into_iter()
    .map(|i| i.key)
    .collect();
    keys.sort();
    keys
}

async fn key_of(pool: &sqlx::PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn consumer_row(pool: &sqlx::PgPool, g: Uuid) -> Option<(String, String, i64)> {
    sqlx::query_as("SELECT state, last_xid::text, last_id FROM outbox_cursors WHERE consumer = $1")
        .bind(format!("search@{g}"))
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn xid_now(pool: &sqlx::PgPool) -> u64 {
    let x: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
        .fetch_one(pool)
        .await
        .unwrap();
    x.parse().unwrap()
}

/// 03 §4.2 restart table: F and the cursor come back from the payload;
/// PostgreSQL's cursor follows the payload whether it was ahead or behind.
#[tokio::test]
async fn restart_resumes_from_the_payload() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tmp_dir();
    let a = w.issue(w.auth_p, "kangaroo first").await;
    let (g1, c1, f1) = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_all(&pool).await.unwrap();
        let b = w.issue(w.auth_p, "kangaroo second").await;
        let _ = b;
        idx.sync_all(&pool).await.unwrap();
        let (c, f) = idx.durable_state().unwrap();
        assert!(f > 0 && c.xid > 0);
        (idx.generation().unwrap(), c, f)
    };
    // PostgreSQL's cursor ran ahead (must not skip), and a new write waits.
    exec(
        &pool,
        "UPDATE outbox_cursors SET last_xid = '999999999999'::xid8, last_id = 999999 WHERE consumer LIKE 'search@%'",
    )
    .await;
    let c = w.issue(w.auth_p, "kangaroo third").await;
    let idx = SearchIndex::open(Some(&dir)).unwrap();
    assert_eq!(idx.frontier(), 0, "nothing serves before the epoch check");
    idx.sync_round(&pool).await.unwrap();
    assert_eq!(idx.generation(), Some(g1), "the same generation resumes");
    assert!(idx.frontier() >= f1);
    let (state, xid, _) = consumer_row(&pool, g1).await.unwrap();
    assert_eq!(state, "active");
    assert!(
        xid.parse::<u64>().unwrap() < 999999999999,
        "PG cursor reset to the payload"
    );
    idx.sync_all(&pool).await.unwrap();
    let app = app_with(&w, idx);
    let mut want = vec![key_of(&pool, a).await, key_of(&pool, c).await];
    want.push(
        sqlx::query_scalar(
            "SELECT k.key FROM issue_keys k JOIN issues i ON i.id = k.issue_id WHERE i.subject = 'kangaroo second'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
    );
    want.sort();
    assert_eq!(
        found(&app, &w, "kangaroo").await,
        want,
        "replayed the third"
    );
    let (c2, _) = app.index.durable_state().unwrap();
    assert!(c2 > c1);
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Frontier-only progress is made durable at most every interval (03 §4.2).
#[tokio::test]
async fn empty_rounds_commit_the_payload_only_now_and_then() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let idx = SearchIndex::open(None).unwrap();
    idx.set_durable_interval(Duration::from_secs(3600));
    idx.sync_all(&pool).await.unwrap();
    let (_, d0) = idx.durable_state().unwrap();
    // Another transaction moves H on; no outbox rows. An older transaction
    // elsewhere on the cluster can hold H back for a while (G21), so the
    // empty rounds repeat until F moves; every one of them must leave the
    // payload's F alone.
    for _ in 0..3 {
        xid_now(&pool).await;
    }
    let idx = &idx;
    common::poll_until(
        "the served F to move past d0",
        common::WAIT_TIMEOUT,
        async || {
            idx.sync(&pool).await.unwrap();
            let (_, d1) = idx.durable_state().unwrap();
            assert_eq!(d1, d0, "no empty commit inside the interval");
            let f = idx.frontier();
            if f > d0 {
                common::Break(())
            } else {
                common::Continue(format!("F={f} d0={d0}"))
            }
        },
    )
    .await;
    idx.set_durable_interval(Duration::ZERO);
    xid_now(&pool).await;
    // F is past d0 now, so the next empty round commits it.
    idx.sync(&pool).await.unwrap();
    let (_, d2) = idx.durable_state().unwrap();
    assert!(d2 > d0, "durable after the interval");
    let _ = w;
}

/// A generation of another data_epoch, or one with no usable payload, is
/// never served: the first round builds a new one from a snapshot.
#[tokio::test]
async fn unusable_generations_are_rebuilt() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "echidna one").await;
    let dir = tmp_dir();
    let g1 = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_all(&pool).await.unwrap();
        idx.generation().unwrap()
    };
    exec(&pool, "UPDATE search_settings SET data_epoch = uuidv7()").await;
    let g2 = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_round(&pool).await.unwrap();
        let g2 = idx.generation().unwrap();
        assert_ne!(g2, g1, "epoch changed: rebuilt");
        let app = app_with(&w, idx);
        assert_eq!(found(&app, &w, "echidna").await.len(), 1);
        g2
    };
    let (state, _, _) = consumer_row(&pool, g1).await.unwrap();
    assert_eq!(state, "retained");
    let cur = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
    assert_eq!(cur.trim(), format!("gen-{g2}"));
    // A CURRENT that names nothing usable: rebuilt as well.
    std::fs::write(dir.join("CURRENT"), "gen-not-a-uuid\n").unwrap();
    let idx = SearchIndex::open(Some(&dir)).unwrap();
    idx.sync_round(&pool).await.unwrap();
    assert_ne!(idx.generation(), Some(g2));
    drop(idx);
    let _ = std::fs::remove_dir_all(&dir);
}

/// P2-D2 and the brief's rule: an old token is never satisfied by an empty
/// index. Before anything serves, text search is `search_unavailable` and
/// waits lag; once the snapshot generation serves, the write is in it.
#[tokio::test]
async fn no_token_is_satisfied_before_a_generation_serves() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.auth_p, "quokka early").await;
    let x = xid_now(&pool).await - 1;
    let fresh = app_with(&w, SearchIndex::open(None).unwrap());
    assert_eq!(fresh.index.frontier(), 0);
    assert!(matches!(
        fresh
            .index
            .wait_for(None, x, Duration::from_millis(30))
            .await,
        Wait::Lagging { .. }
    ));
    let e = service::search(
        &fresh,
        w.alice_ctx,
        SearchParams {
            q: Some("quokka".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::SearchUnavailable);
    // The first round builds the snapshot generation, whose F is the
    // snapshot's H: another open transaction on the cluster older than x
    // keeps it at or below x for a while (T1b). Poll rounds, bounded.
    common::poll_until(
        &format!("the new generation's frontier to pass xid {x}"),
        common::WAIT_TIMEOUT,
        async || {
            fresh.index.sync_round(&fresh.pool).await.unwrap();
            let f = fresh.index.frontier();
            if f > x {
                common::Break(())
            } else {
                common::Continue(format!("F={f}"))
            }
        },
    )
    .await;
    assert!(fresh.index.frontier() > x);
    assert_eq!(
        found(&fresh, &w, "quokka").await,
        vec![key_of(&pool, i).await]
    );
}

#[test]
fn switch_needs_frontier_and_cursor() {
    let c = |xid, id| Cursor { xid, id };
    assert!(switch_ready(10, c(9, 5), 10, c(9, 5)));
    assert!(switch_ready(11, c(10, 1), 10, c(9, 5)));
    // Same F, shorter cursor: the old view may show content the new lacks.
    assert!(!switch_ready(10, c(9, 4), 10, c(9, 5)));
    assert!(!switch_ready(9, c(9, 9), 10, c(9, 5)));
}

/// 03 §4.8: the old generation serves during the rebuild, F never moves
/// back, old tokens stay satisfied, and the new one replaces it.
#[tokio::test]
async fn blue_green_rebuild_keeps_serving() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tmp_dir();
    let a = w.issue(w.auth_p, "numbat before").await;
    let app = app_with(&w, SearchIndex::open(Some(&dir)).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let g1 = app.index.generation().unwrap();
    let x_old = xid_now(&pool).await - 1;
    app.index.sync_all(&pool).await.unwrap();
    let f1 = app.index.frontier();

    let gate = Arc::new(Gate::default());
    faults::arm("search.before_switch", Action::Block(gate.clone()));
    let a2 = app.clone();
    let p2 = pool.clone();
    let job = tokio::spawn(async move { a2.index.rebuild(&p2).await });
    gate.reached.notified().await;
    // Mid-rebuild: the old generation answers and keeps syncing.
    assert_eq!(app.index.generation(), Some(g1));
    let b = w.issue(w.auth_p, "numbat during").await;
    app.index.sync_all(&pool).await.unwrap();
    let mut want = vec![key_of(&pool, a).await, key_of(&pool, b).await];
    want.sort();
    assert_eq!(found(&app, &w, "numbat").await, want);
    let f_mid = app.index.frontier();
    assert!(f_mid >= f1);
    gate.release.notify_one();
    let r = common::join("the rebuild job", job).await.unwrap();
    assert_eq!(r.previous, Some(g1));
    assert_ne!(r.generation, g1);
    assert_eq!(app.index.generation(), Some(r.generation));
    assert!(app.index.frontier() >= f_mid, "F did not move back");
    assert_eq!(
        app.index.wait_for(None, x_old, Duration::ZERO).await,
        Wait::Satisfied { waited_ms: 0 },
        "an old token is still satisfied"
    );
    assert_eq!(found(&app, &w, "numbat").await, want);
    assert_eq!(
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim(),
        format!("gen-{}", r.generation)
    );
    assert_eq!(consumer_row(&pool, g1).await.unwrap().0, "retained");
    assert_eq!(consumer_row(&pool, r.generation).await.unwrap().0, "active");
    let until: bool = sqlx::query_scalar(
        "SELECT retained_until > now() + interval '23 hours' FROM outbox_cursors WHERE consumer = $1",
    )
    .bind(format!("search@{g1}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(until, "24 h rollback window");
    // Past the window the GC removes the old directory and row.
    exec(
        &pool,
        &format!("UPDATE outbox_cursors SET retained_until = now() - interval '1 minute' WHERE consumer = 'search@{g1}'"),
    )
    .await;
    let (_, gens) = app.index.gc(&pool).await.unwrap();
    assert_eq!(gens, 1);
    assert!(!dir.join(format!("gen-{g1}")).exists());
    assert!(consumer_row(&pool, g1).await.is_none());
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed health check aborts the switch: the old generation keeps
/// serving and the new one's lease retires.
#[tokio::test]
async fn a_failed_rebuild_leaves_the_old_generation_serving() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "bilby").await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let g1 = app.index.generation().unwrap();
    faults::arm("search.before_snapshot", Action::Fail);
    // The first rebuild round already serves; the fault hits the new baseline.
    assert!(app.index.rebuild(&pool).await.is_err());
    assert_eq!(app.index.generation(), Some(g1));
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT state FROM outbox_cursors WHERE consumer LIKE 'search@%' AND consumer <> $1",
    )
    .bind(format!("search@{g1}"))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(states, vec!["retired".to_string()]);
    assert_eq!(found(&app, &w, "bilby").await.len(), 1);
}

/// 03 §4.9: the GC keeps every row a live consumer (a bootstrap lease
/// included) may still need, and keeps everything when there is none.
#[tokio::test]
async fn outbox_gc_respects_leases() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    for _ in 0..3 {
        w.issue(w.auth_p, "wallaby").await;
    }
    // The first round builds the generation from a snapshot; its cursor is
    // that snapshot's H. Only once H is past every row is each row below
    // the cursor (applied by the baseline), which is what "applied rows go"
    // below needs: an older transaction elsewhere on the cluster would
    // leave the newest rows to be applied one by one, the last one at the
    // cursor itself, where the GC (xid < cursor) keeps it (G21).
    let newest: String = sqlx::query_scalar("SELECT max(xid)::text FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    common::wait_horizon_past(&pool, newest.parse().unwrap()).await;
    app.index.sync_all(&pool).await.unwrap();
    let old = "UPDATE outbox SET created_at = now() - interval '2 hours'";
    exec(&pool, old).await;
    let total = count(&pool, "SELECT count(*) FROM outbox").await;
    // A bootstrap lease from before these rows holds them.
    exec(
        &pool,
        "INSERT INTO outbox_cursors (consumer, last_xid, last_id, state) VALUES ('search@00000000-0000-0000-0000-000000000001', '0', 0, 'bootstrapping')",
    )
    .await;
    let (n, _) = app.index.gc(&pool).await.unwrap();
    assert_eq!(n, 0, "the lease holds every row");
    // Retired: no longer holds; rows below the active cursor go, newer stay.
    exec(
        &pool,
        "UPDATE outbox_cursors SET state = 'retired' WHERE consumer = 'search@00000000-0000-0000-0000-000000000001'",
    )
    .await;
    let fresh = w.issue(w.auth_p, "wallaby fresh").await;
    let _ = fresh;
    exec(&pool, old).await;
    let (n, _) = app.index.gc(&pool).await.unwrap();
    assert!(
        n >= total as u64,
        "rows the active generation applied are gone"
    );
    assert!(
        count(&pool, "SELECT count(*) FROM outbox").await >= 1,
        "the unapplied row stays"
    );
    // Recent rows stay even below every cursor (one hour of slack).
    app.index.sync_all(&pool).await.unwrap();
    w.issue(w.auth_p, "wallaby recent").await;
    app.index.sync_all(&pool).await.unwrap();
    let before = count(&pool, "SELECT count(*) FROM outbox").await;
    app.index.gc(&pool).await.unwrap();
    assert!(count(&pool, "SELECT count(*) FROM outbox").await >= before - 1);
    // No consumer at all: nothing goes.
    exec(&pool, "DELETE FROM outbox_cursors").await;
    exec(&pool, old).await;
    let (n, _) = app.index.gc(&pool).await.unwrap();
    assert_eq!(n, 0);
}

/// While the lease is registered and the snapshot is about to be taken,
/// the GC cannot remove rows the new generation will replay.
#[tokio::test]
async fn gc_during_bootstrap_keeps_replay_rows() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    w.issue(w.auth_p, "possum").await;
    app.index.sync_all(&pool).await.unwrap();
    let active_xid: String =
        sqlx::query_scalar("SELECT last_xid::text FROM outbox_cursors WHERE state = 'active'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let gate = Arc::new(Gate::default());
    faults::arm("search.before_snapshot", Action::Block(gate.clone()));
    let a2 = app.clone();
    let p2 = pool.clone();
    let job = tokio::spawn(async move { a2.index.rebuild(&p2).await });
    gate.reached.notified().await;
    // The serving generation moves on; old rows age.
    for _ in 0..3 {
        w.issue(w.auth_p, "possum later").await;
    }
    app.index.sync_all(&pool).await.unwrap();
    exec(
        &pool,
        "UPDATE outbox SET created_at = now() - interval '2 hours'",
    )
    .await;
    let lease_xid: String = sqlx::query_scalar(
        "SELECT last_xid::text FROM outbox_cursors WHERE state = 'bootstrapping'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // The lease starts at the serving generation's cursor at that time,
    // not later (everything after it must still be replayable).
    assert!(
        lease_xid.parse::<u64>().unwrap() <= active_xid.parse::<u64>().unwrap(),
        "lease {lease_xid} must not be ahead of the serving cursor {active_xid}"
    );
    app.index.gc(&pool).await.unwrap();
    let below: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE xid >= $1::text::xid8")
        .bind(&lease_xid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(below >= 3, "rows from the lease on survive");
    gate.release.notify_one();
    common::join("the rebuild job", job).await.unwrap();
    assert_eq!(found(&app, &w, "possum").await.len(), 4);
}

/// §17.15 fast fields: version, overdue and the built-in date filters run
/// in the index too; with free text and a selective version the right
/// issues come back even when thousands of others match the text.
#[tokio::test]
async fn fast_fields_narrow_candidates() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let v = id(
        &pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, created_by) VALUES ('{}', 'V9', 'v9', '{}') RETURNING id",
            w.auth_p, w.alice
        ),
    )
    .await;
    // 8,100 issues that all match the text and are not in the version.
    exec(
        &pool,
        &format!(
            "WITH n AS (UPDATE projects SET issue_counter = issue_counter + 8100 WHERE id = '{p}' RETURNING issue_counter),
                  ins AS (INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id)
                          SELECT '{p}', '{t}', '{s}', 'dingo filler ' || g, '', '{a}' FROM generate_series(1, 8100) g
                          RETURNING id)
             INSERT INTO issue_keys (key, issue_id, project_id, number)
             SELECT 'AUTH-' || (n.issue_counter - 8100 + row_number() OVER ()), ins.id, '{p}', n.issue_counter - 8100 + row_number() OVER ()
             FROM ins, n",
            p = w.auth_p,
            t = w.bug,
            s = w.new,
            a = w.bob
        ),
    )
    .await;
    let mine = w.issue(w.auth_p, "dingo target").await;
    let late = w.issue(w.auth_p, "dingo late").await;
    // Boundary (R10 org date): due yesterday is overdue, due today is not.
    let edge = w.issue(w.auth_p, "dingo edge").await;
    let today = "(SELECT (now() AT TIME ZONE timezone)::date FROM org_settings)";
    exec(
        &pool,
        &format!("UPDATE issues SET fixed_version_id = '{v}' WHERE id = '{mine}'"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE issues SET due_date = {today} - 1, start_date = {today} - 10 WHERE id = '{late}';
             UPDATE issues SET due_date = {today}, start_date = {today} - 10 WHERE id = '{edge}'"
        ),
    )
    .await;
    let today_s: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {today}::text")))
        .fetch_one(&pool)
        .await
        .unwrap();
    // A fresh index takes the snapshot path (fast at this size).
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let r = service::search(
        &app,
        w.alice_ctx,
        SearchParams {
            q: Some("dingo".into()),
            version: Some(v.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!r.truncated && r.complete);
    assert_eq!(
        r.items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(),
        vec![key_of(&pool, mine).await]
    );
    let r = service::search(
        &app,
        w.alice_ctx,
        SearchParams {
            q: Some("dingo".into()),
            overdue: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!r.truncated);
    assert_eq!(
        r.items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(),
        vec![key_of(&pool, late).await]
    );
    for (filter, want) in [
        (
            serde_json::json!({"field": "due_date", "op": "lt", "value": today_s}),
            vec![late],
        ),
        (
            serde_json::json!({"field": "due_date", "op": "ge", "value": today_s}),
            vec![edge],
        ),
        (
            serde_json::json!({"field": "fixed_version_id", "op": "eq", "value": v.to_string()}),
            vec![mine],
        ),
        (
            serde_json::json!({"and": [{"field": "start_date", "op": "not_null"},
                                       {"field": "due_date", "op": "is_null"}]}),
            vec![],
        ),
    ] {
        let r = service::search(
            &app,
            w.alice_ctx,
            SearchParams {
                q: Some("dingo".into()),
                filter: Some(filter.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!r.truncated, "{filter}");
        let mut got: Vec<String> = r.items.into_iter().map(|i| i.key).collect();
        got.sort();
        let mut exp = Vec::new();
        for i in want {
            exp.push(key_of(&pool, i).await);
        }
        exp.sort();
        assert_eq!(got, exp, "{filter}");
    }
    // ne keeps issues without the value (the index side must not drop them).
    let r = service::search(
        &app,
        w.alice_ctx,
        SearchParams {
            q: Some("target".into()),
            filter: Some(serde_json::json!({"field": "fixed_version_id", "op": "ne", "value": Uuid::now_v7().to_string()})),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r.items.len(), 1);
}

/// 03 §3.4 step 4 / §7 step 9: equal `search_rev` → `current`; a changed
/// text the index has not seen → `rechecked`; same items both ways.
#[tokio::test]
async fn search_rev_marks_current_and_rechecked() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let i = w.issue(w.auth_p, "platypus alpha").await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let run = || async {
        service::search(
            &app,
            w.alice_ctx,
            SearchParams {
                q: Some("platypus".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let r = run().await;
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].evidence[0].evidence_status, "current");
    // The text changes; the index has not seen it yet (no sync).
    exec(
        &pool,
        &format!("UPDATE issues SET subject = 'platypus beta' WHERE id = '{i}'"),
    )
    .await;
    let r2 = run().await;
    assert_eq!(r2.items.len(), 1);
    assert_eq!(r2.items[0].evidence[0].evidence_status, "rechecked");
    assert!(r2.items[0].evidence[0].text.contains("beta"));
    // The text no longer matches: dropped, whatever the index says.
    exec(
        &pool,
        &format!("UPDATE issues SET subject = 'kiwi' WHERE id = '{i}'"),
    )
    .await;
    assert!(run().await.items.is_empty());
    app.index.sync_all(&pool).await.unwrap();
    assert!(run().await.items.is_empty());
}

/// P2.4: `/metrics` needs the bearer token and shows the search metrics;
/// a transaction holding an xid in *another* database raises the long
/// transaction alert (03 §4.7: the snapshot horizon is instance-wide).
#[tokio::test]
async fn metrics_are_served_and_long_xacts_alert() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let _ = found(&app, &w, "anything").await;
    let r = oxledger::api::metrics_routes(app.clone(), "s3cret".into());
    let get = |auth: Option<&str>| {
        let mut b = axum::http::Request::builder().uri("/metrics");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        r.clone()
            .oneshot(b.body(axum::body::Body::empty()).unwrap())
    };
    assert_eq!(get(None).await.unwrap().status(), 401);
    assert_eq!(get(Some("Bearer wrong")).await.unwrap().status(), 401);
    let resp = get(Some("Bearer s3cret")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    for name in [
        "search_index_lag_seconds_bucket",
        "search_consistency_outcome_total",
        "search_sync_rounds_total",
        "search_outbox_pending",
        "search_frontier ",
        "pg_oldest_xid_holder_age_seconds",
    ] {
        assert!(body.contains(name), "{name} missing:\n{body}");
    }

    // Another database on the same instance, holding an assigned xid.
    // Same server and credentials as the test pool, whichever URL it came from.
    let other = (*pool.connect_options()).clone().database("postgres");
    let opool = sqlx::PgPool::connect_with(other).await.unwrap();
    let mut held = opool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    sqlx::query("SELECT pg_current_xact_id()")
        .execute(&mut *held)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let alerts = oxledger::metrics::check_long_xacts(
        &pool,
        Duration::from_secs(1),
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    let hit = alerts
        .iter()
        .find(|(_, h)| h.pid == pid)
        .expect("the cross-database xid holder raises an alert");
    assert!(matches!(hit.0, oxledger::metrics::Level::Warning));
    assert_eq!(hit.1.datname.as_deref(), Some("postgres"));
    held.rollback().await.unwrap();
    let alerts = oxledger::metrics::check_long_xacts(
        &pool,
        Duration::from_secs(1),
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    assert!(alerts.iter().all(|(_, h)| h.pid != pid));
    opool.close().await;
}

/// 03 §4.9: the GC bound is strict. Events of the transaction the cursor
/// sits in (same xid, later id) are not applied yet and must stay.
#[tokio::test]
async fn gc_keeps_the_cursor_transaction() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let ids: Vec<(String, i64)> = sqlx::query_as(
        "INSERT INTO outbox (topic, entity_id) VALUES ('issue', uuidv7()), ('issue', uuidv7())
         RETURNING xid::text, id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (x, first) = (ids[0].0.clone(), ids.iter().map(|r| r.1).min().unwrap());
    sqlx::query(
        "UPDATE outbox_cursors SET last_xid = $1::text::xid8, last_id = $2 WHERE state = 'active'",
    )
    .bind(&x)
    .bind(first)
    .execute(&pool)
    .await
    .unwrap();
    exec(
        &pool,
        "UPDATE outbox SET created_at = now() - interval '2 hours'",
    )
    .await;
    app.index.gc(&pool).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE xid = $1::text::xid8")
        .bind(&x)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 2, "events of the cursor's transaction survive");
}

/// 03 §4.2 restart table: a payload of another schema, analyzer or query
/// semantics version is not served; the first round builds a new one.
#[tokio::test]
async fn a_payload_of_another_version_is_rebuilt() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "quokka").await;
    let dir = tmp_dir();
    let g1 = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_all(&pool).await.unwrap();
        idx.generation().unwrap()
    };
    let meta = dir.join(format!("gen-{g1}")).join("meta.json");
    let orig = std::fs::read_to_string(&meta).unwrap();
    for key in [
        "schema_version",
        "analyzer_version",
        "query_semantics_version",
    ] {
        let (from, to) = (format!("{key}\\\":"), format!("{key}\\\":9"));
        assert!(orig.contains(&from), "{key} in the payload: {orig}");
        std::fs::write(&meta, orig.replacen(&from, &to, 1)).unwrap();
        std::fs::write(dir.join("CURRENT"), format!("gen-{g1}\n")).unwrap();
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_round(&pool).await.unwrap();
        assert_ne!(idx.generation(), Some(g1), "{key} changed: rebuilt");
        let app = app_with(&w, idx);
        assert_eq!(found(&app, &w, "quokka").await.len(), 1);
        drop(app);
        std::fs::write(&meta, &orig).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 03 §4.8 step 6: a sample that differs without an outbox event to
/// explain it aborts the switch; the old generation keeps serving.
#[tokio::test]
async fn an_unexplained_mismatch_aborts_the_switch() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.auth_p, "platypus").await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let g1 = app.index.generation().unwrap();
    let gate = Arc::new(Gate::default());
    faults::arm("search.before_switch", Action::Block(gate.clone()));
    let a2 = app.clone();
    let p2 = pool.clone();
    let job = tokio::spawn(async move { a2.index.rebuild(&p2).await });
    gate.reached.notified().await;
    // A change that bypasses the outbox (what a bug or a manual fix does).
    let mut tx = pool.begin().await.unwrap();
    for sql in [
        "ALTER TABLE issues DISABLE TRIGGER USER".to_string(),
        format!("UPDATE issues SET subject = 'platypus changed' WHERE id = '{a}'"),
        "ALTER TABLE issues ENABLE TRIGGER USER".to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    gate.release.notify_one();
    assert!(
        common::join("the rebuild job", job).await.is_err(),
        "the health check aborts"
    );
    assert_eq!(app.index.generation(), Some(g1));
    assert_eq!(found(&app, &w, "platypus").await.len(), 1);
}

/// P2-D11: with the background loop running and no success for 60 s, text
/// search is `search_unavailable`; a recent success serves again.
#[tokio::test]
async fn a_stalled_sync_makes_text_search_unavailable() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "bandicoot").await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let m = &app.index.metrics;
    m.loop_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    m.set_last_success_ago(Duration::from_secs(61));
    let p = SearchParams {
        q: Some("bandicoot".into()),
        ..Default::default()
    };
    let e = service::search(&app, w.alice_ctx, p.clone())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::SearchUnavailable);
    m.set_last_success_ago(Duration::from_secs(1));
    let r = service::search(&app, w.alice_ctx, p).await.unwrap();
    assert_eq!(r.items.len(), 1);
}

/// Verification D1: if PostgreSQL refuses to mark the new generation
/// active, nothing has switched and the old one keeps serving.
#[tokio::test]
async fn a_failed_switch_keeps_the_old_generation() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "cassowary").await;
    let dir = tmp_dir();
    let app = app_with(&w, SearchIndex::open(Some(&dir)).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    let g1 = app.index.generation().unwrap();
    exec(
        &pool,
        "CREATE FUNCTION p2_refuse() RETURNS trigger LANGUAGE plpgsql AS $f$
         BEGIN IF OLD.state = 'bootstrapping' AND NEW.state = 'active' THEN RAISE 'refused'; END IF;
         RETURN NEW; END $f$",
    )
    .await;
    exec(
        &pool,
        "CREATE TRIGGER p2_refuse BEFORE UPDATE ON outbox_cursors FOR EACH ROW EXECUTE FUNCTION p2_refuse()",
    )
    .await;
    let r = app.index.rebuild(&pool).await;
    exec(&pool, "DROP TRIGGER p2_refuse ON outbox_cursors").await;
    exec(&pool, "DROP FUNCTION p2_refuse()").await;
    assert!(r.is_err());
    assert_eq!(app.index.generation(), Some(g1));
    assert_eq!(
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim(),
        format!("gen-{g1}")
    );
    assert!(dir.join(format!("gen-{g1}")).exists());
    let (state, _, _) = consumer_row(&pool, g1).await.unwrap();
    assert_eq!(state, "active");
    w.issue(w.auth_p, "cassowary two").await;
    app.index.sync_all(&pool).await.unwrap();
    assert_eq!(found(&app, &w, "cassowary").await.len(), 2);
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Verification D2: a transient error while activating the generation on
/// disk retries it; it is not thrown away for a full rebuild.
#[tokio::test]
async fn a_failed_activation_retries_the_same_generation() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "wallaby").await;
    let dir = tmp_dir();
    let g1 = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.sync_all(&pool).await.unwrap();
        idx.generation().unwrap()
    };
    let idx = SearchIndex::open(Some(&dir)).unwrap();
    exec(
        &pool,
        "CREATE FUNCTION p2_refuse() RETURNS trigger LANGUAGE plpgsql AS $f$
         BEGIN RAISE 'refused'; END $f$",
    )
    .await;
    exec(
        &pool,
        "CREATE TRIGGER p2_refuse BEFORE INSERT OR UPDATE ON outbox_cursors FOR EACH ROW EXECUTE FUNCTION p2_refuse()",
    )
    .await;
    let r = idx.sync_round(&pool).await;
    exec(&pool, "DROP TRIGGER p2_refuse ON outbox_cursors").await;
    exec(&pool, "DROP FUNCTION p2_refuse()").await;
    assert!(r.is_err());
    idx.sync_round(&pool).await.unwrap();
    assert_eq!(idx.generation(), Some(g1));
    drop(idx);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Verification D3: an issue without a current key (an import, damage) is
/// not indexed, and it does not stop the baseline.
#[tokio::test]
async fn a_keyless_issue_does_not_block_the_baseline() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.issue(w.auth_p, "numbat keyed").await;
    exec(
        &pool,
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id)
         SELECT project_id, tracker_id, status_id, 'numbat keyless', '', author_id FROM issues LIMIT 1",
    )
    .await;
    let app = app_with(&w, SearchIndex::open(None).unwrap());
    app.index.sync_all(&pool).await.unwrap();
    assert_eq!(found(&app, &w, "numbat").await.len(), 1);
}
