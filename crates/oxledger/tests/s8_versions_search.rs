// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S8 gate, milestone side: T-V17 (`overdue`), T-V18 and T-V18b
//! (`version:`), the Filter AST date / version fields (§17.15), T-V5's
//! deleted-version half and T-V16 (history names). Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ApiError, ErrorCode};
use oxledger::issue_view::{self, GetParams};
use oxledger::query_syntax;
use oxledger::service::{self, SearchParams, SearchResult};
use oxledger::versions::{self, NewVersion, VersionPatch, VersionSet};
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

async fn version(w: &World, project: &str, name: &str, date: Option<String>) -> (Uuid, i32) {
    let r = versions::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        project,
        &NewVersion {
            name: name.into(),
            effective_date: date,
            description: None,
            status: None,
        },
    )
    .await
    .unwrap();
    (r.version.id, r.version.lock_version)
}

async fn today(pool: &sqlx::PgPool) -> chrono::NaiveDate {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn day(t: chrono::NaiveDate, d: i64) -> String {
    (t + chrono::Duration::days(d)).to_string()
}

async fn lv(pool: &sqlx::PgPool, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn set(
    w: &World,
    who: oxledger::auth::AuthContext,
    i: Uuid,
    s: Value,
) -> anyhow::Result<CommandResult> {
    let lv = lv(&w.pool, i).await;
    let d = json!({"command_schema_version": 1,
        "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": s}],
        "preconditions": [{"target": tgt(i), "lock_version": lv}]});
    commands::submit(&w.app, who, Uuid::now_v7(), &d).await
}

async fn search(w: &World, p: SearchParams) -> anyhow::Result<SearchResult> {
    service::search(&w.app, w.alice_ctx, p).await
}

fn keys(r: &SearchResult) -> Vec<String> {
    let mut k: Vec<String> = r.items.iter().map(|i| i.key.clone()).collect();
    k.sort();
    k
}

async fn key_of(pool: &sqlx::PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn details(e: &anyhow::Error) -> Value {
    e.downcast_ref::<ApiError>()
        .and_then(|a| a.details.clone())
        .unwrap_or(Value::Null)
}

async fn setup(pool: &sqlx::PgPool) -> World {
    let w = World::build(pool).await;
    grant(pool, w.dev, &["manage_versions"]).await;
    w
}

#[tokio::test]
async fn version_filters() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    // An invisible project with its own v2.0 (T-V18b).
    let ops = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OPS','Ops') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO enabled_modules VALUES ('{ops}', 'issue_tracking')"),
    )
    .await;
    let t = today(&pool).await;
    let (a20, _) = version(&w, "AUTH", "v2.0", Some(day(t, 10))).await;
    let (a21, _) = version(&w, "AUTH", "v2.1", Some(day(t, 20))).await;
    let (w20, _) = version(&w, "WEB", "v2.0", Some(day(t, 5))).await;
    let o20 = id(
        &pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, created_by, last_event_seq)
             VALUES ('{ops}', 'v2.0', 'v2.0', '{}', 0) RETURNING id",
            w.alice
        ),
    )
    .await;
    let a1 = w.issue(w.auth_p, "auth in v2.0").await;
    let a2 = w.issue(w.auth_p, "auth in v2.1").await;
    let a3 = w.issue(w.auth_p, "auth unscheduled").await;
    let w1 = w.issue(w.web, "web in v2.0").await;
    set(
        &w,
        w.alice_ctx,
        a1,
        json!({"fixed_version_id": a20, "due_date": day(t, 3)}),
    )
    .await
    .unwrap();
    set(&w, w.alice_ctx, a2, json!({"fixed_version_id": a21}))
        .await
        .unwrap();
    set(&w, w.alice_ctx, w1, json!({"fixed_version_id": w20}))
        .await
        .unwrap();
    let o1 = oxledger::test_support::create_issue(
        &pool,
        oxledger::test_support::NewIssue {
            project_id: ops,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "ops in v2.0",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap()
    .0;
    sqlx::query("UPDATE issues SET fixed_version_id = $1 WHERE id = $2")
        .bind(o20)
        .bind(o1)
        .execute(&pool)
        .await
        .unwrap();
    let (k1, k2, k3, kw) = (
        key_of(&pool, a1).await,
        key_of(&pool, a2).await,
        key_of(&pool, a3).await,
        key_of(&pool, w1).await,
    );
    let p = |project: Option<&str>, version: &str| SearchParams {
        project: project.map(String::from),
        version: Some(version.into()),
        ..Default::default()
    };

    // T-V18: a name in another case matches; unknown → invalid_filter.
    assert_eq!(
        keys(&search(&w, p(Some("AUTH"), "V2.0")).await.unwrap()),
        vec![k1.clone()]
    );
    let e = search(&w, p(Some("AUTH"), "不存在")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidFilter);
    assert_eq!(details(&e)["path"], "version");
    // next.
    assert_eq!(
        keys(&search(&w, p(Some("AUTH"), "next")).await.unwrap()),
        vec![k1.clone()]
    );
    assert_eq!(
        keys(&search(&w, p(Some("WEB"), "NEXT")).await.unwrap()),
        vec![kw.clone()]
    );

    // T-V18b: by name or next without one project → project_scope_required.
    for v in ["v2.0", "next"] {
        let e = search(&w, p(None, v)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidFilter, "{v}");
        assert_eq!(
            details(&e),
            json!({"path": "version", "reason": "project_scope_required"})
        );
    }
    // Ids work across projects and never show an invisible project's issues.
    assert_eq!(
        keys(&search(&w, p(None, &w20.to_string())).await.unwrap()),
        vec![kw.clone()]
    );
    assert!(
        search(&w, p(None, &o20.to_string()))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // A name in AUTH means AUTH's v2.0 only.
    let r = search(
        &w,
        SearchParams {
            q: None,
            ..p(Some("AUTH"), "v2.0")
        },
    )
    .await
    .unwrap();
    assert!(!keys(&r).contains(&kw));

    // The text syntax (B2) carries version: too.
    let q = query_syntax::parse("project:AUTH version:\"V2.1\"").unwrap();
    assert_eq!(q.normalized(), "project:AUTH version:V2.1");
    assert_eq!(
        keys(&search(&w, q.to_params()).await.unwrap()),
        vec![k2.clone()]
    );

    // Filter AST fields (§17.15).
    let f = |filter: Value| SearchParams {
        project: Some("AUTH".into()),
        filter: Some(filter),
        ..Default::default()
    };
    let r = search(&w, f(json!({"field": "fixed_version_id", "op": "is_null"})))
        .await
        .unwrap();
    assert_eq!(keys(&r), vec![k3.clone()]);
    let r = search(
        &w,
        f(json!({"field": "fixed_version_id", "op": "in", "value": [a20, a21]})),
    )
    .await
    .unwrap();
    assert_eq!(keys(&r), {
        let mut v = vec![k1.clone(), k2.clone()];
        v.sort();
        v
    });
    let r = search(
        &w,
        f(json!({"field": "fixed_version_id", "op": "ne", "value": a20})),
    )
    .await
    .unwrap();
    assert_eq!(
        keys(&r),
        {
            let mut v = vec![k2.clone(), k3.clone()];
            v.sort();
            v
        },
        "ne keeps issues without a version (custom-field semantics)"
    );
    let r = search(
        &w,
        f(json!({"field": "due_date", "op": "between", "value": [day(t, 1), day(t, 3)]})),
    )
    .await
    .unwrap();
    assert_eq!(keys(&r), vec![k1.clone()]);
    let r = search(&w, f(json!({"field": "start_date", "op": "is_null"})))
        .await
        .unwrap();
    assert_eq!(r.items.len(), 3);
    for bad in [
        json!({"field": "fixed_version_id", "op": "lt", "value": a20}),
        json!({"field": "fixed_version_id", "op": "eq", "value": "v2.0"}),
        json!({"field": "due_date", "op": "eq", "value": "tomorrow"}),
    ] {
        let e = search(&w, f(bad.clone())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidFilter, "{bad}");
    }

    // T-V18 ambiguity: an overdue open version makes next ambiguous.
    version(&w, "AUTH", "v1.9", Some(day(t, -2))).await;
    let e = search(&w, p(Some("AUTH"), "next")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AmbiguousReference);
    assert_eq!(details(&e)["reason"], "overdue_open_exists");
    assert_eq!(details(&e)["candidates"].as_array().unwrap().len(), 2);
}

/// T-V17: `overdue` follows the date without any write or reindex.
#[tokio::test]
async fn overdue_moves_with_the_day() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    // UTC−12 → UTC+14: 26 hours apart, so "today" moves by one or two days.
    exec(&pool, "UPDATE org_settings SET timezone = 'Etc/GMT+12'").await;
    let t = today(&pool).await;
    let due = w.issue(w.auth_p, "due today").await;
    let late = w.issue(w.auth_p, "late but done").await;
    let later = w.issue(w.auth_p, "due later").await;
    set(&w, w.alice_ctx, due, json!({"due_date": t.to_string()}))
        .await
        .unwrap();
    set(&w, w.alice_ctx, later, json!({"due_date": day(t, 5)}))
        .await
        .unwrap();
    set(&w, w.alice_ctx, late, json!({"due_date": day(t, -3)}))
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET status_id = $1 WHERE id = $2")
        .bind(w.done)
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    w.app.index.sync_all(&pool).await.unwrap();
    let kd = key_of(&pool, due).await;
    let overdue = |q: Option<&str>| SearchParams {
        q: q.map(String::from),
        project: Some("AUTH".into()),
        overdue: Some(true),
        ..Default::default()
    };
    for q in [None, Some("due")] {
        let r = search(&w, overdue(q)).await.unwrap();
        assert!(r.items.is_empty(), "due today is not overdue ({q:?})");
        assert_eq!(r.meta.today, t.to_string());
    }
    let outbox = count(&pool, "SELECT count(*) FROM outbox").await;
    let events = count(&pool, "SELECT count(*) FROM issue_events").await;
    exec(&pool, "UPDATE org_settings SET timezone = 'Etc/GMT-14'").await;
    let t2 = today(&pool).await;
    assert!(t2 > t);
    for q in [None, Some("due")] {
        let r = search(&w, overdue(q)).await.unwrap();
        assert_eq!(keys(&r), vec![kd.clone()], "{q:?}");
        assert_eq!(r.meta.today, t2.to_string());
    }
    assert_eq!(
        count(&pool, "SELECT count(*) FROM outbox").await,
        outbox,
        "no reindex"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_events").await,
        events
    );
    // Without overdue meta.today is still there (design 05 §3.2: always).
    let r = search(
        &w,
        SearchParams {
            project: Some("AUTH".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(r.meta.today == t2.to_string() && r.items.len() == 3);
    exec(&pool, "UPDATE org_settings SET timezone = 'Asia/Taipei'").await;
}

async fn history(w: &World, who: oxledger::auth::AuthContext, key: &str) -> Vec<Value> {
    issue_view::get(
        &w.app,
        who,
        GetParams {
            key: key.into(),
            fields: Some(vec!["fixed_version".into()]),
            citations: None,
            include: Some(vec!["recent_events".into()]),
            note_limit: None,
            event_limit: Some(20),
        },
    )
    .await
    .unwrap()
    .recent_events
    .unwrap()
    .items
    .into_iter()
    .filter_map(|e| e.detail)
    .collect()
}

/// T-V5 (the invisible-reference half) and T-V16.
#[tokio::test]
async fn deleted_versions_keep_their_issues_and_names() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    // alice sees only public issues and her own; bob's private one is not hers.
    exec(
        &pool,
        "UPDATE roles SET issues_visibility = 'default' WHERE name = 'Dev'",
    )
    .await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let t = today(&pool).await;
    let (v3, v3_lv) = version(&w, "AUTH", "v3", Some(day(t, 9))).await;
    let (v4, v4_lv) = version(&w, "AUTH", "v4", Some(day(t, 9))).await;
    let p1 = w.issue(w.auth_p, "bob's private one").await;
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{p1}'"),
    )
    .await;
    set(&w, bob, p1, json!({"fixed_version_id": v3}))
        .await
        .unwrap();
    let kp = key_of(&pool, p1).await;

    // The only reference is invisible to alice: deleting looks exactly like
    // deleting an unused version.
    let d3 = versions::delete(&w.app, w.alice_ctx, Uuid::now_v7(), v3, v3_lv)
        .await
        .unwrap();
    let d4 = versions::delete(&w.app, w.alice_ctx, Uuid::now_v7(), v4, v4_lv)
        .await
        .unwrap();
    let shape = |r: &versions::VersionWriteResult| {
        let mut v = serde_json::to_value(r).unwrap();
        for k in ["execution_id", "search"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let ver = v["version"].as_object_mut().unwrap();
        for k in ["id", "name"] {
            ver.remove(k);
        }
        v
    };
    assert_eq!(shape(&d3), shape(&d4), "T-V5: same response");

    // bob still sees the deleted version on his issue.
    let got = issue_view::get(
        &w.app,
        bob,
        GetParams {
            key: kp.clone(),
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
    assert_eq!((fv.id, fv.name.as_str(), fv.deleted), (v3, "v3", true));
    // T-V16: history names it as deleted.
    let h = serde_json::to_string(&history(&w, bob, &kp).await).unwrap();
    assert!(h.contains("v3（已刪除）"), "{h}");
    assert!(h.contains(r#""deleted":true"#), "{h}");
    // Setting it again is no_effect; other fields still change; another
    // issue cannot take it; it can be cleared.
    let r = set(&w, bob, p1, json!({"fixed_version_id": v3}))
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "no_effect");
    set(&w, bob, p1, json!({"subject": "still editable"}))
        .await
        .unwrap();
    let other = w.issue(w.auth_p, "another").await;
    let e = set(&w, w.alice_ctx, other, json!({"fixed_version_id": v3}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let r = set(&w, bob, p1, json!({"fixed_version_id": null}))
        .await
        .unwrap();
    assert_eq!(r.items[0].status, "applied");
    // A new v3 takes the name; resolving it finds only the new one.
    let (v3b, _) = version(&w, "AUTH", "v3", None).await;
    assert_ne!(v3b, v3);
    set(&w, w.alice_ctx, other, json!({"fixed_version_id": v3b}))
        .await
        .unwrap();
    let r = search(
        &w,
        SearchParams {
            project: Some("AUTH".into()),
            version: Some("v3".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(keys(&r), vec![key_of(&pool, other).await]);

    // T-V16: a rename shows the current name in old history.
    let (v5, v5_lv) = version(&w, "AUTH", "v5", None).await;
    let pub1 = w.issue(w.auth_p, "public one").await;
    set(&w, w.alice_ctx, pub1, json!({"fixed_version_id": v5}))
        .await
        .unwrap();
    versions::update(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        v5,
        &VersionPatch {
            lock_version: v5_lv,
            set: VersionSet {
                name: Some("v5-final".into()),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let h =
        serde_json::to_string(&history(&w, w.alice_ctx, &key_of(&pool, pub1).await).await).unwrap();
    assert!(h.contains("v5-final") && !h.contains("\"v5\""), "{h}");
}
