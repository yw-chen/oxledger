// SPDX-License-Identifier: GPL-2.0-or-later
//! P2.5–P2.6 (design 03 §5.4–5.8, §7 steps 10–11): strategy selection for
//! filter-only searches (probe, S0/S2, shared deadline, `query_timeout`) and
//! `resolve(kind: "custom_field")` with `definition_version`. Needs
//! PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use ox_core::filter::FieldType;
use oxledger::custom_fields::{NewField, create_field};
use oxledger::error::{ApiError, ErrorCode};
use oxledger::filter::Shape;
use oxledger::service::{self, BUDGET_MS, LAST_SHAPE, S2_DELAY_MS, SearchParams};
use serde_json::json;
use std::sync::atomic::Ordering;
use world::*;

const N: i64 = 12_000;

struct Fx {
    w: World,
    /// Every issue has 5 / 7; only 50 have 1.
    a: String,
    b: String,
    c: String,
}

async fn field(w: &World, name: &str) -> i32 {
    create_field(
        &w.pool,
        NewField {
            name,
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap()
}

async fn fixture() -> Fx {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    exec(
        &pool,
        &format!(
            "WITH n AS (UPDATE projects SET issue_counter = issue_counter + {N} WHERE id = '{p}' RETURNING issue_counter),
                  ins AS (INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id, updated_at)
                          SELECT '{p}', '{t}', '{s}', 'bulk ' || g, '', '{a}', now() - g * interval '1 second'
                          FROM generate_series(1, {N}) g RETURNING id)
             INSERT INTO issue_keys (key, issue_id, project_id, number)
             SELECT 'AUTH-' || (n.issue_counter - {N} + row_number() OVER ()), ins.id, '{p}', n.issue_counter - {N} + row_number() OVER ()
             FROM ins, n",
            p = w.auth_p,
            t = w.bug,
            s = w.new,
            a = w.bob
        ),
    )
    .await;
    let (fa, fb, fc) = (
        field(&w, "Alpha").await,
        field(&w, "Beta").await,
        field(&w, "Gamma").await,
    );
    exec(
        &pool,
        &format!(
            "INSERT INTO issue_cf (issue_id, field_id, v_num) SELECT id, {fa}, 5 FROM issues;
             INSERT INTO issue_cf (issue_id, field_id, v_num) SELECT id, {fb}, 7 FROM issues;
             INSERT INTO issue_cf (issue_id, field_id, v_num) SELECT id, {fc}, 1 FROM issues ORDER BY id LIMIT 50;
             ANALYZE issues; ANALYZE issue_keys; ANALYZE issue_cf;"
        ),
    )
    .await;
    Fx {
        w,
        a: "Alpha".into(),
        b: "Beta".into(),
        c: "Gamma".into(),
    }
}

async fn filter_search(
    fx: &Fx,
    f: serde_json::Value,
    cursor: Option<String>,
) -> anyhow::Result<service::SearchResult> {
    service::search(
        &fx.w.app,
        fx.w.alice_ctx,
        SearchParams {
            filter: Some(f),
            limit: Some(100),
            cursor,
            fields: Some(vec!["key".into()]),
            ..Default::default()
        },
    )
    .await
}

fn last_shape() -> (Shape, bool) {
    LAST_SHAPE
        .lock()
        .unwrap()
        .take()
        .expect("a filter search ran")
}

/// §5.8: probes pick S2 only when every condition is above T; S0 and S2
/// page through exactly the same rows; S2's plan walks the updated_at index
/// with per-row SubPlans, never a hashed one.
#[tokio::test]
async fn strategy_selection_and_paging() {
    let fx = fixture().await;
    let both = json!({"and": [{"field": fx.a, "op": "eq", "value": 5}, {"field": fx.b, "op": "eq", "value": 7}]});
    let rare = json!({"and": [{"field": fx.a, "op": "eq", "value": 5}, {"field": fx.c, "op": "eq", "value": 1}]});
    let r = filter_search(&fx, rare.clone(), None).await.unwrap();
    assert_eq!(last_shape(), (Shape::S0, false), "a condition ≤ T keeps S0");
    assert_eq!(r.items.len(), 50);
    // Out of scope: or / ne / one condition.
    for f in [
        json!({"or": [{"field": fx.a, "op": "eq", "value": 5}, {"field": fx.b, "op": "eq", "value": 7}]}),
        json!({"and": [{"field": fx.a, "op": "ne", "value": 4}, {"field": fx.b, "op": "eq", "value": 7}]}),
        json!({"field": fx.a, "op": "eq", "value": 5}),
    ] {
        filter_search(&fx, f, None).await.unwrap();
        assert_eq!(last_shape().0, Shape::S0);
    }
    // All common: S2. Page through 3 pages and compare with plain SQL order.
    let mut got: Vec<String> = Vec::new();
    let mut cursor = None;
    for _ in 0..3 {
        let r = filter_search(&fx, both.clone(), cursor).await.unwrap();
        assert_eq!(last_shape(), (Shape::S2, false));
        got.extend(r.items.into_iter().map(|i| i.key));
        cursor = r.next_cursor;
        assert!(cursor.is_some());
    }
    let want: Vec<String> = sqlx::query_scalar(
        "SELECT k.key FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         ORDER BY i.updated_at DESC, i.id DESC LIMIT 300",
    )
    .fetch_all(&fx.w.pool)
    .await
    .unwrap();
    assert_eq!(got, want, "S2 pages: no gaps, no repeats");
    // Plan structure (§5.8): backward/ordered index walk, correlated SubPlans.
    let ids: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM field_defs WHERE name IN ('Alpha','Beta') ORDER BY name",
    )
    .fetch_all(&fx.w.pool)
    .await
    .unwrap();
    let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "EXPLAIN SELECT i.id FROM issues i
         WHERE EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {} AND cf.v_num = 5 OFFSET 0)
           AND EXISTS (SELECT 1 FROM issue_cf cf WHERE cf.issue_id = i.id AND cf.field_id = {} AND cf.v_num = 7 OFFSET 0)
         ORDER BY i.updated_at DESC, i.id DESC LIMIT 101",
        ids[0], ids[1]
    )))
    .fetch_all(&fx.w.pool)
    .await
    .unwrap();
    let plan = plan.join("\n");
    assert!(plan.contains("issues_updated_idx"), "{plan}");
    assert!(plan.contains("SubPlan"), "{plan}");
    assert!(!plan.to_lowercase().contains("hashed subplan"), "{plan}");
}

/// §5.8 deadline: a timed-out S2 switches once to S0 within the same budget;
/// out of budget is `query_timeout` (no partial page); the transaction is
/// still usable after the cancel (savepoint).
#[tokio::test]
async fn deadline_switch_and_timeout() {
    let fx = fixture().await;
    let both = json!({"and": [{"field": fx.a, "op": "eq", "value": 5}, {"field": fx.b, "op": "eq", "value": 7}]});
    S2_DELAY_MS.store(10_000, Ordering::SeqCst);
    let t = std::time::Instant::now();
    let r = filter_search(&fx, both.clone(), None).await;
    S2_DELAY_MS.store(0, Ordering::SeqCst);
    let r = r.unwrap();
    assert_eq!(last_shape(), (Shape::S0, true), "switched after S2 ran out");
    assert_eq!(r.items.len(), 100);
    assert!(
        t.elapsed() < std::time::Duration::from_millis(3500),
        "{:?}",
        t.elapsed()
    );
    // No budget at all: query_timeout with the documented details.
    BUDGET_MS.store(1, Ordering::SeqCst);
    let e = filter_search(&fx, both, None).await;
    BUDGET_MS.store(0, Ordering::SeqCst);
    let e = e.unwrap_err();
    let a = e.downcast_ref::<ApiError>().expect("api error");
    assert_eq!(a.code, ErrorCode::QueryTimeout);
    assert!(!a.code.retryable());
    assert_eq!(a.details.as_ref().unwrap()["deadline_ms"], 1);
    // Nothing keeps running server-side.
    let running: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()
           AND state = 'active' AND query LIKE '%issue_cf%' AND pid <> pg_backend_pid()",
    )
    .fetch_one(&fx.w.pool)
    .await
    .unwrap();
    assert_eq!(running, 0);
}

/// §7 step 11: resolve(kind: custom_field) and definition_version.
#[tokio::test]
async fn resolve_custom_field_and_definition_version() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let sev = create_field(
        &pool,
        NewField {
            name: "Severity",
            ty: FieldType::List,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &["High", "Low"],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let est = create_field(
        &pool,
        NewField {
            name: "Estimate",
            ty: FieldType::Float,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    create_field(
        &pool,
        NewField {
            name: "Secret score",
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[],
        },
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!("UPDATE field_defs SET unit = 'hour' WHERE id = {est}"),
    )
    .await;
    let resolve = |text: &'static str| {
        let w = &w;
        async move {
            service::resolve(&w.app, w.alice_ctx, text, Some("custom_field"))
                .await
                .unwrap()
        }
    };
    let r = resolve("severity").await;
    assert_eq!(r.custom_fields.len(), 1);
    let f = &r.custom_fields[0];
    assert_eq!(
        (f.id, f.ftype.as_str(), f.definition_version),
        (sev, "list", 1)
    );
    assert_eq!(
        f.options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        vec!["High", "Low"]
    );
    assert!(f.operators.contains(&"in") && !f.operators.contains(&"lt"));
    let e = &resolve("Estimate").await.custom_fields[0];
    assert_eq!(e.unit.as_deref(), Some("hour"));
    assert_eq!(e.definition_version, 2, "unit change bumped it");
    assert!(e.operators.contains(&"between"));
    // A field alice may not read is not there, not even as a valid value.
    let r = resolve("secret").await;
    assert!(r.custom_fields.is_empty());
    assert!(!r.valid_values.iter().any(|v| v.contains("Secret")));
    assert!(r.valid_values.contains(&"Severity".to_string()));

    // Options change → version moves; a pinned old version fails.
    exec(
        &pool,
        &format!(
            "INSERT INTO field_options (field_id, label, position) VALUES ({sev}, 'Critical', 3)"
        ),
    )
    .await;
    exec(
        &pool,
        &format!("UPDATE field_options SET position = 9 WHERE field_id = {sev} AND label = 'Low'"),
    )
    .await;
    let v = resolve("Severity").await.custom_fields[0].definition_version;
    assert_eq!(v, 2, "insert bumps, a position-only change does not");
    let search = |dv: i32| {
        let w = &w;
        async move {
            service::search(
                &w.app,
                w.alice_ctx,
                SearchParams {
                    filter: Some(json!({"field": "Severity", "op": "eq", "value": "High", "definition_version": dv})),
                    ..Default::default()
                },
            )
            .await
        }
    };
    let e = search(1).await.unwrap_err();
    let a = e.downcast_ref::<ApiError>().unwrap();
    assert_eq!(a.code, ErrorCode::FieldDefinitionChanged);
    let d = a.details.as_ref().unwrap();
    assert_eq!(
        (d["field"].as_str(), d["current_version"].as_i64()),
        (Some("Severity"), Some(2))
    );
    assert_eq!(d["suggested_tool"], "resolve");
    assert!(search(2).await.is_ok());
    // Hidden field with a guessed version: the same "no such field" as any unknown name.
    let e = service::search(
        &w.app,
        w.alice_ctx,
        SearchParams {
            filter: Some(
                json!({"field": "Secret score", "op": "eq", "value": 1, "definition_version": 99}),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        e.downcast_ref::<ApiError>().unwrap().code,
        ErrorCode::InvalidFilter
    );
}
