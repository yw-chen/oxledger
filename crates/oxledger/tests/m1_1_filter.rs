// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.1: Filter AST v2 (`where`), `sort`, `/filter-fields`.
//! T-M1-01 (legacy parameters = `where`), T-M1-02 (unknown and invisible
//! look the same), T-M1-03 (`ever_in` after D12, through the filter),
//! T-M1-04 (relative dates, week start, the org-time-zone day boundary),
//! T-M1-05 (limits), plus sort paging, `me` and `filter_fields`.
//! Needs PostgreSQL 18; see `common::test_pool`. Uses the process-wide
//! `TEST_NOW`, so run with `--test-threads=1`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use chrono::{DateTime, NaiveDate, Utc};
use common::test_pool;
use ox_core::filter::{FieldType, Value as FValue};
use oxledger::auth::AuthContext;
use oxledger::commands;
use oxledger::custom_fields::{self, NewField};
use oxledger::describe::{self, DescribeParams, DescribeScope};
use oxledger::error::{ErrorCode, classify};
use oxledger::filter::v2::{self, SortDir, SortInput, TEST_NOW};
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::service::{self, SearchParams, SearchResult};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

fn keys(r: &SearchResult) -> Vec<String> {
    r.items.iter().map(|i| i.key.clone()).collect()
}

fn wh(v: Value) -> SearchParams {
    SearchParams {
        where_: Some(v),
        limit: Some(100),
        ..Default::default()
    }
}

fn cond(field: &str, op: &str, value: Value) -> Value {
    json!({"field": field, "op": op, "value": value})
}

async fn run(w: &World, who: AuthContext, p: SearchParams) -> anyhow::Result<SearchResult> {
    service::search(&w.app, who, p).await
}

async fn ks(w: &World, who: AuthContext, p: SearchParams) -> Vec<String> {
    keys(&run(w, who, p).await.unwrap())
}

/// Every page of `p` at `limit` items each, concatenated.
async fn paged(w: &World, who: AuthContext, p: SearchParams, limit: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = None;
    for _ in 0..100 {
        let r = run(
            w,
            who,
            SearchParams {
                limit: Some(limit),
                cursor: cursor.take(),
                ..p.clone()
            },
        )
        .await
        .unwrap();
        assert!(r.items.len() <= limit as usize);
        out.extend(keys(&r));
        match r.next_cursor {
            Some(c) => cursor = Some(c),
            None => return out,
        }
    }
    panic!("too many pages");
}

/// (code, details.reason, details.path, message)
fn err(e: anyhow::Error) -> (ErrorCode, String, String, String) {
    let a = classify(&e).unwrap_or_else(|| panic!("not an API error: {e:#}"));
    let d = a.details.clone().unwrap_or(Value::Null);
    (
        a.code,
        d["reason"].as_str().unwrap_or("").to_string(),
        d["path"].as_str().unwrap_or("").to_string(),
        a.message.clone(),
    )
}

async fn where_err(w: &World, who: AuthContext, v: Value) -> (ErrorCode, String, String, String) {
    err(run(w, who, wh(v)).await.expect_err("an error"))
}

/// Two errors that must look the same: equal apart from the input they
/// repeat (`a_in` in the first message, `b_in` in the second).
fn same_shape(
    a: (ErrorCode, String, String, String),
    b: (ErrorCode, String, String, String),
    a_in: &str,
    b_in: &str,
) {
    assert_eq!((a.0, &a.1, &a.2), (b.0, &b.1, &b.2), "{a:?} vs {b:?}");
    assert_eq!(a.3.replace(a_in, "<x>"), b.3.replace(b_in, "<x>"));
}

async fn key_of(pool: &PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn key_list(pool: &PgPool, ids: &[Uuid]) -> Vec<String> {
    let mut out = Vec::new();
    for i in ids {
        out.push(key_of(pool, *i).await);
    }
    out
}

async fn set(pool: &PgPool, i: Uuid, assignments: &str) {
    exec(
        pool,
        &format!("UPDATE issues SET {assignments} WHERE id = '{i}'"),
    )
    .await;
}

async fn version(pool: &PgPool, project: Uuid, name: &str, date: NaiveDate, by: Uuid) -> Uuid {
    id(
        pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, effective_date, created_by)
             VALUES ('{project}', '{name}', lower('{name}'), '{date}', '{by}') RETURNING id"
        ),
    )
    .await
}

async fn org_today(pool: &PgPool) -> NaiveDate {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn day(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

/// Puts `TEST_NOW` and `week_start` back however the test ends.
struct ClockReset(PgPool);

impl Drop for ClockReset {
    fn drop(&mut self) {
        *TEST_NOW.lock().unwrap() = None;
        let pool = self.0.clone();
        // Best effort: the next test's World::build does not reset week_start.
        let _ = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let _ = sqlx::query("UPDATE org_settings SET week_start = 1")
                        .execute(&pool)
                        .await;
                })
        })
        .join();
    }
}

fn set_now(at: &str) {
    *TEST_NOW.lock().unwrap() = Some(at.parse::<DateTime<Utc>>().unwrap());
}

async fn list_field(w: &World, name: &str, visible: bool) -> (i32, Uuid) {
    let f = custom_fields::create_field(
        &w.pool,
        NewField {
            name,
            ty: FieldType::List,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &["High", "Low"],
            visible,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let high: Uuid =
        sqlx::query_scalar("SELECT id FROM field_options WHERE field_id = $1 AND label = 'High'")
            .bind(f)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    (f, high)
}

// ---------------------------------------------------------------- T-M1-01

#[tokio::test]
async fn t_m1_01_legacy_parameters_equal_where() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let g = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'Ops') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO group_members VALUES ('{g}', '{}')", w.alice),
    )
    .await;
    let (sev, high) = list_field(&w, "Severity", true).await;
    custom_fields::reconcile_indexes(&pool).await.unwrap();
    let today = org_today(&pool).await;
    let v_next = version(&pool, w.web, "v1", today + chrono::Days::new(10), w.alice).await;
    let v_later = version(&pool, w.web, "v2", today + chrono::Days::new(40), w.alice).await;

    let rows = [
        (w.web, Some(w.alice), w.new, Some(v_next), true),
        (w.web, Some(g), w.doing, None, false),
        (w.web, Some(w.bob), w.new, Some(v_next), true),
        (w.web, None, w.doing, Some(v_later), false),
        (w.web, Some(w.alice), w.done, None, true),
        (w.auth_p, Some(w.alice), w.new, None, false),
        (w.web, Some(w.carol), w.done, Some(v_next), false),
        (w.web, None, w.new, None, false),
    ];
    let mut ids = Vec::new();
    for (n, (project, assignee, status, ver, high_sev)) in rows.into_iter().enumerate() {
        let i = w.issue(project, "legacy").await;
        let mut s = format!("status_id = '{status}', updated_at = now() - interval '{n} minutes'");
        if let Some(x) = assignee {
            s.push_str(&format!(", assigned_to_id = '{x}'"));
        }
        if let Some(v) = ver {
            s.push_str(&format!(", fixed_version_id = '{v}'"));
        }
        set(&pool, i, &s).await;
        if high_sev {
            custom_fields::set_value(&pool, i, sev, &FValue::Ref(high))
                .await
                .unwrap();
        }
        ids.push(i);
    }
    // Equal updated_at: the id breaks the tie, the same way on both sides.
    exec(
        &pool,
        &format!(
            "UPDATE issues SET updated_at = (SELECT updated_at FROM issues WHERE id = '{}')
             WHERE id = '{}'",
            ids[2], ids[3]
        ),
    )
    .await;
    // Overdue: an open issue and a done one, both due yesterday.
    for i in [ids[0], ids[4]] {
        set(&pool, i, "due_date = current_date - 3").await;
    }
    let k = key_list(&pool, &ids).await;

    let legacy = |f: fn(&mut SearchParams)| {
        let mut p = SearchParams {
            limit: Some(100),
            ..Default::default()
        };
        f(&mut p);
        p
    };
    let pairs: Vec<(SearchParams, Value, Vec<usize>)> = vec![
        (
            legacy(|p| p.status_category = Some("open".into())),
            cond("status_category", "eq", json!("open")),
            vec![0, 1, 2, 3, 5, 7],
        ),
        (
            legacy(|p| p.assignee = Some("me".into())),
            cond("assignee", "eq", json!({"me": true})),
            // The group's issue too.
            vec![0, 1, 4, 5],
        ),
        (
            legacy(|p| p.assignee = Some("none".into())),
            json!({"field": "assignee", "op": "is_null"}),
            vec![3, 7],
        ),
        (
            legacy(|p| {
                p.project = Some("WEB".into());
                p.version = Some("next".into());
            }),
            json!({"and": [cond("project", "eq", json!("WEB")),
                           cond("fixed_version", "eq", json!({"next": true}))]}),
            vec![0, 2, 6],
        ),
        (
            legacy(|p| p.status = Some("Doing".into())),
            cond("status", "eq", json!("Doing")),
            vec![1, 3],
        ),
        (
            legacy(|p| p.tracker = Some("Bug".into())),
            cond("tracker", "eq", json!("Bug")),
            (0..8).collect(),
        ),
        (
            legacy(|p| p.overdue = Some(true)),
            cond("overdue", "eq", json!(true)),
            vec![0],
        ),
        (
            legacy(|p| {
                p.filter = Some(json!({"field": "Severity", "op": "in", "value": ["High"]}));
            }),
            cond(&format!("cf:{sev}"), "in", json!([{"list": high}])),
            vec![0, 2, 4],
        ),
    ];
    for (n, (old, new, want)) in pairs.into_iter().enumerate() {
        let got_old = ks(&w, a, old.clone()).await;
        let got_new = ks(&w, a, wh(new.clone())).await;
        assert_eq!(got_old, got_new, "pair {n}: {new}");
        let mut want: Vec<String> = want.iter().map(|i| k[*i].clone()).collect();
        let mut got = got_new.clone();
        want.sort();
        got.sort();
        assert_eq!(got, want, "pair {n}: {new}");
        // The same pages, one or three at a time.
        for limit in [1, 3] {
            assert_eq!(paged(&w, a, old.clone(), limit).await, got_old, "pair {n}");
            assert_eq!(
                paged(&w, a, wh(new.clone()), limit).await,
                got_new,
                "pair {n}"
            );
        }
    }
    // Plain cf values (v1 style) are taken too.
    assert_eq!(
        ks(
            &w,
            a,
            wh(cond(&format!("cf:{sev}"), "eq", json!(high.to_string())))
        )
        .await
        .len(),
        3
    );
    // where ANDs with the legacy parameters.
    let both = SearchParams {
        project: Some("WEB".into()),
        ..wh(cond("assignee", "eq", json!({"me": true})))
    };
    let mut got = ks(&w, a, both).await;
    got.sort();
    let mut want = vec![k[0].clone(), k[1].clone(), k[4].clone()];
    want.sort();
    assert_eq!(got, want);
    // meta.today is there without overdue.
    assert_eq!(
        run(&w, a, wh(cond("is_private", "eq", json!(false))))
            .await
            .unwrap()
            .meta
            .today,
        today.to_string()
    );
    // overdue=false: the complement.
    let not_overdue = ks(&w, a, wh(cond("overdue", "eq", json!(false)))).await;
    assert_eq!(not_overdue.len(), 7);
    assert!(!not_overdue.contains(&k[0]));
    // A cursor is bound to its where.
    let r = run(
        &w,
        a,
        SearchParams {
            limit: Some(1),
            ..wh(cond("status_category", "eq", json!("open")))
        },
    )
    .await
    .unwrap();
    let e = run(
        &w,
        a,
        SearchParams {
            limit: Some(1),
            cursor: r.next_cursor,
            ..wh(cond("status_category", "eq", json!("done")))
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidCursor);
}

// ---------------------------------------------------------------- T-M1-02

#[tokio::test]
async fn t_m1_02_unknown_and_invisible_look_alike() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // HID: nobody is a member.
    let hid = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('HID','Hidden') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO enabled_modules VALUES ('{hid}', 'issue_tracking')"),
    )
    .await;
    let hv = version(&pool, hid, "secret", day("2030-01-01"), w.bob).await;
    let (restricted, _) = list_field(&w, "Secret", false).await;
    let (open_f, _) = list_field(&w, "Open", true).await;
    w.issue(w.web, "x").await;
    let unknown = Uuid::now_v7();

    // Fields: unreadable vs nonexistent.
    let rf = format!("cf:{restricted}");
    same_shape(
        where_err(&w, a, cond(&rf, "is_null", Value::Null)).await,
        where_err(&w, a, cond("cf:999999", "is_null", Value::Null)).await,
        &rf,
        "cf:999999",
    );
    same_shape(
        where_err(&w, a, cond("cf:Secret", "is_null", Value::Null)).await,
        where_err(&w, a, cond("cf:Nope", "is_null", Value::Null)).await,
        "Secret",
        "Nope",
    );
    let e = where_err(&w, a, cond("triage", "eq", json!(true))).await;
    assert_eq!(
        (e.0, e.1.as_str(), e.2.as_str()),
        (ErrorCode::InvalidFilter, "unknown_field", "where.field")
    );
    let e = where_err(&w, a, cond("spent_hours", "gt", json!("1"))).await;
    assert_eq!(e.1, "unknown_field", "not in M1.1");
    // The readable one works.
    run(
        &w,
        a,
        wh(json!({"field": format!("cf:{open_f}"), "op": "is_null"})),
    )
    .await
    .unwrap();

    // Projects: by key and by id.
    same_shape(
        where_err(&w, a, cond("project", "eq", json!("HID"))).await,
        where_err(&w, a, cond("project", "eq", json!("NOPE"))).await,
        "HID",
        "NOPE",
    );
    let (hs, us) = (hid.to_string(), unknown.to_string());
    let e = where_err(&w, a, cond("project", "in", json!([hs]))).await;
    assert_eq!(
        (e.0, e.1.as_str()),
        (ErrorCode::InvalidFilter, "unknown_value")
    );
    same_shape(
        e,
        where_err(&w, a, cond("project", "in", json!([us]))).await,
        &hs,
        &us,
    );
    // Versions of an invisible project.
    let hvs = hv.to_string();
    same_shape(
        where_err(&w, a, cond("fixed_version", "eq", json!(hvs))).await,
        where_err(&w, a, cond("fixed_version", "eq", json!(us))).await,
        &hvs,
        &us,
    );
    // Sorting by an unreadable field looks like an unknown one.
    let sort_err = |f: String| {
        let w = &w;
        async move {
            err(run(
                w,
                a,
                SearchParams {
                    sort: Some(vec![SortInput {
                        field: f,
                        dir: SortDir::Asc,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err())
        }
    };
    same_shape(
        sort_err(rf.clone()).await,
        sort_err("cf:999999".into()).await,
        &rf,
        "cf:999999",
    );
    assert_eq!(
        sort_err("subject".into()).await.0,
        ErrorCode::InvalidArgument
    );

    // Ops and shapes.
    let e = where_err(&w, a, cond("is_private", "in", json!([true]))).await;
    assert_eq!((e.1.as_str(), e.2.as_str()), ("unsupported_op", "where.op"));
    let e = where_err(&w, a, cond("status", "lt", json!("New"))).await;
    assert_eq!(e.1, "unsupported_op");
    let e = where_err(
        &w,
        a,
        json!({"field": "status", "op": "eq", "value": "New", "x": 1}),
    )
    .await;
    assert_eq!(e.1, "invalid_shape");
    let e = where_err(&w, a, json!([1])).await;
    assert_eq!(e.1, "invalid_shape");
    let e = where_err(&w, a, cond("due_date", "eq", json!("2026-13-01"))).await;
    assert_eq!(e.1, "invalid_value");
    let e = where_err(&w, a, cond("fixed_version", "eq", json!({"next": true}))).await;
    assert_eq!(e.1, "project_scope_required");
    let e = where_err(&w, a, cond("status", "eq", json!({"me": true}))).await;
    assert_eq!(e.1, "invalid_value");

    // An agent has no `me`.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let e = where_err(&w, agent, cond("assignee", "eq", json!({"me": true}))).await;
    assert_eq!(
        (e.0, e.1.as_str(), e.2.as_str()),
        (ErrorCode::InvalidFilter, "me_unavailable", "where.value")
    );
    let e = where_err(
        &w,
        agent,
        json!({"or": [cond("author", "in", json!(["me"]))]}),
    )
    .await;
    assert_eq!(e.1, "me_unavailable");
    // The rest works for it.
    assert_eq!(
        ks(&w, agent, wh(cond("project", "eq", json!("WEB"))))
            .await
            .len(),
        1
    );
}

// ---------------------------------------------------------------- T-M1-03

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "w", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn human(w: &World, d: &Value) -> commands::CommandResult {
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), d)
        .await
        .unwrap()
}

async fn transition(w: &World, i: Uuid, to: Uuid) {
    human(
        w,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to}),
            ],
            vec![(i, lv(w, i).await)],
        ),
    )
    .await;
}

#[tokio::test]
async fn t_m1_03_ever_in_after_redaction() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_transitions VALUES ('{}','{}','{}','{}','always')",
            w.bug, w.dev, w.doing, w.done
        ),
    )
    .await;
    let create = |subject: &'static str| {
        let w = &w;
        async move {
            human(
                w,
                &doc(
                    vec![
                        json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                                "tracker_id": w.bug,
                                "fields": {"subject": subject, "assigned_to_id": w.bob}}),
                    ],
                    vec![],
                ),
            )
            .await
            .effects[0]
                .issue_id
        }
    };
    // i: New → Doing → Done, bob → carol. j: stays New with bob.
    let i = create("moves").await;
    let j = create("stays").await;
    human(
        &w,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i),
                        "set": {"assigned_to_id": w.carol}}),
            ],
            vec![(i, lv(&w, i).await)],
        ),
    )
    .await;
    transition(&w, i, w.doing).await;
    transition(&w, i, w.done).await;
    let (ki, kj) = (key_of(&pool, i).await, key_of(&pool, j).await);
    let ever = |field: &'static str, set: Vec<Uuid>| {
        let w = &w;
        async move {
            let mut k = ks(w, a, wh(cond(field, "ever_in", json!(set)))).await;
            k.sort();
            k
        }
    };
    let both = {
        let mut v = vec![ki.clone(), kj.clone()];
        v.sort();
        v
    };
    assert_eq!(ever("status", vec![w.new]).await, both);
    assert_eq!(ever("status", vec![w.doing]).await, vec![ki.clone()]);
    assert_eq!(
        ever("status", vec![w.done]).await,
        vec![ki.clone()],
        "current value"
    );
    assert_eq!(ever("assignee", vec![w.bob]).await, both);
    assert_eq!(ever("assignee", vec![w.carol]).await, vec![ki.clone()]);
    // ever_in is only for status, assignee and fixed_version.
    let e = where_err(&w, a, cond("priority", "ever_in", json!([w.new]))).await;
    assert_eq!(e.1, "unsupported_op");
    // Names work as with in.
    assert_eq!(
        ks(&w, a, wh(cond("status", "ever_in", json!(["Doing"])))).await,
        vec![ki.clone()]
    );

    grant(&pool, w.dev, &["manage_project"]).await;
    let event = |kind: &'static str, old: Option<Uuid>| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM issue_events WHERE issue_id = $1 AND kind = $2
                   AND ($3::text IS NULL OR payload->>'old' = $3::text) ORDER BY seq LIMIT 1",
            )
            .bind(i)
            .bind(kind)
            .bind(old.map(|u| u.to_string()))
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let redact = |event: Uuid| {
        let (w, key) = (&w, ki.clone());
        async move {
            redaction::redact(
                &w.app,
                w.alice_ctx,
                &w.policy,
                Uuid::now_v7(),
                &Target::IssueEvent { key, event },
                &RedactInput {
                    reason_code: "confidential".into(),
                    reason: "wrong status history".into(),
                },
            )
            .await
            .unwrap()
        }
    };
    let a_to_b = event("status_changed", Some(w.new)).await;
    let created = event("created", None).await;
    assert!(redact(a_to_b).await.changed);
    // The created event still says i started as New.
    assert_eq!(ever("status", vec![w.new]).await, both);
    redact(created).await;
    assert_eq!(
        ever("status", vec![w.new]).await,
        vec![kj.clone()],
        "redacted: no hit"
    );
    // Doing is still known from Doing → Done.
    assert_eq!(ever("status", vec![w.doing]).await, vec![ki.clone()]);
    // bob is still known from bob → carol.
    assert_eq!(ever("assignee", vec![w.bob]).await, both);
    // The same with q (index path).
    w.app.index.sync_all(&pool).await.unwrap();
    let r = run(
        &w,
        a,
        SearchParams {
            q: Some("moves".into()),
            ..wh(cond("status", "ever_in", json!([w.new])))
        },
    )
    .await
    .unwrap();
    assert!(keys(&r).is_empty(), "{:?}", keys(&r));
    let r = run(
        &w,
        a,
        SearchParams {
            q: Some("moves".into()),
            ..wh(cond("status", "ever_in", json!([w.doing])))
        },
    )
    .await
    .unwrap();
    assert_eq!(keys(&r), [ki]);
}

// ---------------------------------------------------------------- T-M1-04

#[tokio::test]
async fn t_m1_04_relative_dates_and_the_day_boundary() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let _reset = ClockReset(pool.clone());
    let a = w.alice_ctx;
    exec(
        &pool,
        "UPDATE org_settings SET timezone = 'Asia/Taipei', week_start = 1",
    )
    .await;
    // 2026-11-14 is a Saturday.
    let mut by_due = Vec::new();
    for d in [
        "2026-11-07",
        "2026-11-08",
        "2026-11-09",
        "2026-11-14",
        "2026-11-15",
        "2026-11-16",
        "2026-11-22",
    ] {
        let i = w.issue(w.web, "dated").await;
        set(
            &pool,
            i,
            &format!("due_date = '{d}', created_at = '2026-01-01T00:00:00Z'"),
        )
        .await;
        by_due.push((d, key_of(&pool, i).await));
    }
    let due = |ds: &[&str]| {
        let mut v: Vec<String> = by_due
            .iter()
            .filter(|(d, _)| ds.contains(d))
            .map(|(_, k)| k.clone())
            .collect();
        v.sort();
        v
    };
    let q = |v: Value| {
        let w = &w;
        async move {
            let r = run(w, a, wh(v)).await.unwrap();
            let mut k = keys(&r);
            k.sort();
            (k, r.meta.today)
        }
    };
    let within = |range: &str| cond("due_date", "within", json!({"range": range}));

    // 23:59 in Taipei on Saturday the 14th.
    set_now("2026-11-14T15:59:00Z");
    let (k, today) = q(within("this_week")).await;
    assert_eq!(today, "2026-11-14");
    assert_eq!(k, due(&["2026-11-09", "2026-11-14", "2026-11-15"]));
    assert_eq!(
        q(within("last_week")).await.0,
        due(&["2026-11-07", "2026-11-08"])
    );
    assert_eq!(
        q(within("next_week")).await.0,
        due(&["2026-11-16", "2026-11-22"])
    );
    exec(&pool, "UPDATE org_settings SET week_start = 7").await;
    assert_eq!(
        q(within("this_week")).await.0,
        due(&["2026-11-08", "2026-11-09", "2026-11-14"])
    );
    // 00:00 on Sunday the 15th: a new week when it starts on Sunday.
    set_now("2026-11-14T16:00:00Z");
    let (k, today) = q(within("this_week")).await;
    assert_eq!(today, "2026-11-15");
    assert_eq!(k, due(&["2026-11-15", "2026-11-16"]));
    exec(&pool, "UPDATE org_settings SET week_start = 1").await;
    assert_eq!(
        q(within("this_week")).await.0,
        due(&["2026-11-09", "2026-11-14", "2026-11-15"])
    );
    // Relative dates and n-day ranges (Redmine: n + 1 days).
    assert_eq!(
        q(cond(
            "due_date",
            "eq",
            json!({"rel": "today", "offset": -1})
        ))
        .await
        .0,
        due(&["2026-11-14"])
    );
    assert_eq!(
        q(cond("due_date", "lt", json!({"rel": "today"}))).await.0,
        due(&["2026-11-07", "2026-11-08", "2026-11-09", "2026-11-14"])
    );
    assert_eq!(
        q(cond(
            "due_date",
            "within",
            json!({"range": "last_n_days", "n": 1})
        ))
        .await
        .0,
        due(&["2026-11-14", "2026-11-15"])
    );
    assert_eq!(
        q(cond(
            "due_date",
            "within",
            json!({"range": "next_n_days", "n": 7})
        ))
        .await
        .0,
        due(&["2026-11-15", "2026-11-16", "2026-11-22"])
    );
    assert_eq!(
        q(cond("due_date", "within", json!({"range": "this_month"})))
            .await
            .0,
        due(&[
            "2026-11-07",
            "2026-11-08",
            "2026-11-09",
            "2026-11-14",
            "2026-11-15",
            "2026-11-16",
            "2026-11-22"
        ])
    );
    assert_eq!(
        q(cond("due_date", "between", json!([null, "2026-11-08"])))
            .await
            .0,
        due(&["2026-11-07", "2026-11-08"])
    );
    // Overdue moves with the clock.
    assert_eq!(
        q(cond("overdue", "eq", json!(true))).await.0,
        due(&["2026-11-07", "2026-11-08", "2026-11-09", "2026-11-14"])
    );
    let legacy = run(
        &w,
        a,
        SearchParams {
            overdue: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(legacy.meta.today, "2026-11-15");
    assert_eq!(legacy.items.len(), 4);
    // Bad ranges.
    for bad in [
        json!({"range": "last_n_days"}),
        json!({"range": "this_week", "n": 2}),
        json!({"range": "last_n_days", "n": 367}),
        json!({"range": "fortnight"}),
        json!("this_week"),
    ] {
        let e = where_err(&w, a, cond("due_date", "within", bad.clone())).await;
        assert_eq!(e.1, "invalid_value", "{bad}");
    }
    let e = where_err(
        &w,
        a,
        cond("due_date", "eq", json!({"rel": "today", "offset": 4000})),
    )
    .await;
    assert_eq!(e.1, "invalid_value");

    // created_at by the org's date: 15:30Z is the 14th in Taipei, 16:30Z the 15th.
    let late = w.issue(w.web, "late").await;
    let early = w.issue(w.web, "early").await;
    set(
        &pool,
        late,
        "created_at = '2026-11-14T15:30:00Z', updated_at = '2026-11-14T15:30:00Z'",
    )
    .await;
    set(
        &pool,
        early,
        "created_at = '2026-11-14T16:30:00Z', updated_at = '2026-11-14T16:30:00Z'",
    )
    .await;
    let (kl, ke) = (key_of(&pool, late).await, key_of(&pool, early).await);
    assert_eq!(
        q(cond(
            "created_at",
            "between",
            json!(["2026-11-14", "2026-11-14"])
        ))
        .await
        .0,
        vec![kl.clone()]
    );
    assert_eq!(
        q(cond("created_at", "ge", json!("2026-11-15"))).await.0,
        vec![ke.clone()]
    );
    assert_eq!(
        q(cond("created_at", "gt", json!("2026-11-14"))).await.0,
        vec![ke.clone()]
    );
    assert_eq!(
        q(cond(
            "created_at",
            "within",
            json!({"range": "last_n_days", "n": 1})
        ))
        .await
        .0,
        {
            let mut v = vec![kl.clone(), ke.clone()];
            v.sort();
            v
        }
    );
    assert_eq!(
        q(cond(
            "created_at",
            "lt",
            json!({"rel": "today", "offset": -1})
        ))
        .await
        .0
        .len(),
        7,
        "the dated ones, created in January"
    );
    // A different org zone moves the boundary.
    exec(&pool, "UPDATE org_settings SET timezone = 'UTC'").await;
    let r = q(cond(
        "created_at",
        "between",
        json!(["2026-11-14", "2026-11-14"]),
    ))
    .await;
    assert_eq!(r.1, "2026-11-14");
    let mut both = vec![kl, ke];
    both.sort();
    assert_eq!(r.0, both);
    exec(&pool, "UPDATE org_settings SET timezone = 'Asia/Taipei'").await;
}

// ---------------------------------------------------------------- T-M1-05

#[tokio::test]
async fn t_m1_05_limits() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    w.issue(w.web, "x").await;
    let c = cond("is_private", "eq", json!(false));
    let too = |e: (ErrorCode, String, String, String)| {
        assert_eq!(
            (e.0, e.1.as_str()),
            (ErrorCode::InvalidFilter, "too_complex"),
            "{e:?}"
        );
    };
    // Depth: 3 levels pass, 4 do not.
    let d3 = json!({"and": [{"or": [{"and": [c.clone()]}]}]});
    assert_eq!(ks(&w, a, wh(d3.clone())).await.len(), 1);
    too(where_err(&w, a, json!({"or": [d3]})).await);
    // Conditions: 30 pass, 31 do not.
    let n = |k: usize| json!({"and": vec![c.clone(); k]});
    assert_eq!(ks(&w, a, wh(n(30))).await.len(), 1);
    too(where_err(&w, a, n(31)).await);
    // Across branches too.
    too(where_err(&w, a, json!({"or": [n(16), n(15)]})).await);
    // Values: 100 pass, 101 do not.
    let vals = |k: usize| cond("status", "in", json!(vec!["New"; k]));
    assert_eq!(ks(&w, a, wh(vals(100))).await.len(), 1);
    let e = where_err(&w, a, vals(101)).await;
    assert_eq!(e.2, "where.value");
    too(e);
    too(where_err(&w, a, cond("status", "not_in", json!(vec!["New"; 101]))).await);
    too(where_err(&w, a, cond("status", "ever_in", json!(vec!["New"; 101]))).await);
    // Size: over 16 KiB.
    let big = cond("cf:1", "eq", json!("x".repeat(17 * 1024)));
    too(where_err(&w, a, big).await);
    // Sort: at most 3 keys.
    let s = |f: &str| SortInput {
        field: f.into(),
        dir: SortDir::Desc,
    };
    let e = run(
        &w,
        a,
        SearchParams {
            sort: Some(vec![s("due_date"), s("priority"), s("status"), s("key")]),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidArgument);
    run(
        &w,
        a,
        SearchParams {
            sort: Some(vec![s("due_date"), s("priority"), s("status")]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------- sort

#[tokio::test]
async fn m1_1_sort_and_paging() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let pts = custom_fields::create_field(
        &pool,
        NewField {
            name: "Points",
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
    .unwrap();
    custom_fields::reconcile_indexes(&pool).await.unwrap();
    // 12 issues (keys WEB-1 … WEB-12), dues with ties and gaps.
    let dues = [
        Some("2026-12-03"),
        None,
        Some("2026-12-01"),
        Some("2026-12-03"),
        None,
        Some("2026-12-02"),
        Some("2026-12-01"),
        None,
        Some("2026-12-05"),
        Some("2026-12-03"),
        Some("2026-12-04"),
        None,
    ];
    let mut rows: Vec<(Option<&str>, Option<i64>, Uuid, String)> = Vec::new();
    for (n, d) in dues.iter().enumerate() {
        let i = w.issue(w.web, "alpha").await;
        let mut s = format!("updated_at = now() - interval '{} minutes'", (n * 7) % 12);
        if let Some(d) = d {
            s.push_str(&format!(", due_date = '{d}'"));
        }
        set(&pool, i, &s).await;
        let p = (n % 4 != 3).then_some(((n * 5) % 7) as i64);
        if let Some(p) = p {
            custom_fields::set_value(&pool, i, pts, &FValue::Num(p as f64))
                .await
                .unwrap();
        }
        rows.push((*d, p, i, key_of(&pool, i).await));
    }
    let sort = |f: &str, dir: SortDir| SearchParams {
        sort: Some(vec![SortInput {
            field: f.into(),
            dir,
        }]),
        limit: Some(100),
        ..Default::default()
    };
    // Expected: the value, nulls last either way, then the id in the key's direction.
    fn expect<T: Ord + Clone>(
        rows: &[(Option<&str>, Option<i64>, Uuid, String)],
        val: impl Fn(&(Option<&str>, Option<i64>, Uuid, String)) -> Option<T>,
        desc: bool,
    ) -> Vec<String> {
        let mut r: Vec<_> = rows.to_vec();
        r.sort_by(|x, y| {
            let (a, b) = (val(x), val(y));
            let o = match (&a, &b) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => std::cmp::Ordering::Greater,
                (_, None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => {
                    if desc {
                        b.cmp(a)
                    } else {
                        a.cmp(b)
                    }
                }
            };
            o.then_with(|| if desc { y.2.cmp(&x.2) } else { x.2.cmp(&y.2) })
        });
        r.into_iter().map(|x| x.3).collect()
    }
    for desc in [false, true] {
        let dir = if desc { SortDir::Desc } else { SortDir::Asc };
        let want = expect(&rows, |r| r.0.map(String::from), desc);
        assert_eq!(ks(&w, a, sort("due_date", dir)).await, want, "due {dir:?}");
        for limit in [1, 2, 5] {
            assert_eq!(paged(&w, a, sort("due_date", dir), limit).await, want);
        }
        let want = expect(&rows, |r| r.1, desc);
        let f = format!("cf:{pts}");
        assert_eq!(ks(&w, a, sort(&f, dir)).await, want, "points {dir:?}");
        assert_eq!(paged(&w, a, sort(&f, dir), 3).await, want);
        // key: by number, not text (WEB-2 before WEB-10).
        let mut want: Vec<String> = rows.iter().map(|r| r.3.clone()).collect();
        want.sort_by_key(|k| k[4..].parse::<u32>().unwrap());
        if desc {
            want.reverse();
        }
        assert_eq!(paged(&w, a, sort("key", dir), 4).await, want, "key {dir:?}");
    }
    // Two keys: due asc, then points desc.
    let two = SearchParams {
        sort: Some(vec![
            SortInput {
                field: "due_date".into(),
                dir: SortDir::Asc,
            },
            SortInput {
                field: format!("cf:{pts}"),
                dir: SortDir::Desc,
            },
        ]),
        ..Default::default()
    };
    let mut want: Vec<_> = rows.clone();
    want.sort_by(|x, y| {
        let nl = |a: Option<String>, b: Option<String>, desc: bool| match (a, b) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => {
                if desc {
                    b.cmp(&a)
                } else {
                    a.cmp(&b)
                }
            }
        };
        nl(x.0.map(String::from), y.0.map(String::from), false)
            .then_with(|| match (x.1, y.1) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => std::cmp::Ordering::Greater,
                (_, None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => b.cmp(&a),
            })
            .then_with(|| y.2.cmp(&x.2))
    });
    let want: Vec<String> = want.into_iter().map(|x| x.3).collect();
    assert_eq!(paged(&w, a, two.clone(), 2).await, want);

    // Explicit updated_at desc is the default order.
    let default = paged(&w, a, SearchParams::default(), 5).await;
    assert_eq!(
        paged(&w, a, sort("updated_at", SortDir::Desc), 5).await,
        default
    );
    assert_eq!(default.len(), 12);
    // A cursor is bound to its sort.
    let r = run(
        &w,
        a,
        SearchParams {
            limit: Some(2),
            ..sort("due_date", SortDir::Asc)
        },
    )
    .await
    .unwrap();
    let e = run(
        &w,
        a,
        SearchParams {
            limit: Some(2),
            cursor: r.next_cursor,
            ..sort("due_date", SortDir::Desc)
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidCursor);

    // With q: the same order, and rank is still the relevance position.
    w.app.index.sync_all(&pool).await.unwrap();
    let with_q = |p: SearchParams| SearchParams {
        q: Some("alpha".into()),
        ..p
    };
    let want = expect(&rows, |r| r.0.map(String::from), false);
    let r = run(&w, a, with_q(sort("due_date", SortDir::Asc)))
        .await
        .unwrap();
    assert_eq!(keys(&r), want);
    let mut ranks: Vec<u32> = r.items.iter().map(|i| i.rank.unwrap()).collect();
    ranks.sort();
    assert_eq!(ranks, (1..=12).collect::<Vec<u32>>());
    assert_eq!(
        paged(&w, a, with_q(sort("due_date", SortDir::Asc)), 5).await,
        want
    );
    assert_eq!(
        paged(
            &w,
            a,
            with_q(SearchParams {
                where_: Some(json!({"field": "due_date", "op": "not_null"})),
                ..sort("due_date", SortDir::Desc)
            }),
            3
        )
        .await,
        expect(&rows, |r| r.0.map(String::from), true)[..8]
    );
}

// ---------------------------------------------------------------- me, author, filter_fields

#[tokio::test]
async fn m1_1_author_me_and_on_behalf() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let mine = w.issue(w.web, "mine").await;
    set(&pool, mine, &format!("author_id = '{}'", w.alice)).await;
    // Written by the agent for alice: author is not alice.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let agent_principal = agent.principal_id().unwrap();
    let by_agent = w.issue(w.web, "by agent").await;
    set(
        &pool,
        by_agent,
        &format!(
            "author_id = '{agent_principal}', created_on_behalf_of_id = '{}'",
            w.alice
        ),
    )
    .await;
    let other = w.issue(w.web, "bob's").await;
    let (km, ka, ko) = (
        key_of(&pool, mine).await,
        key_of(&pool, by_agent).await,
        key_of(&pool, other).await,
    );
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(
        sorted(ks(&w, a, wh(cond("author", "eq", json!({"me": true})))).await),
        sorted(vec![km.clone(), ka.clone()])
    );
    assert_eq!(
        ks(&w, a, wh(cond("author", "ne", json!("me")))).await,
        vec![ko.clone()]
    );
    assert_eq!(
        ks(&w, a, wh(cond("author", "not_in", json!([{"me": true}])))).await,
        vec![ko.clone()]
    );
    assert_eq!(
        ks(&w, a, wh(cond("author", "eq", json!("bob")))).await,
        vec![ko.clone()]
    );
    assert_eq!(
        sorted(
            ks(
                &w,
                a,
                wh(cond("author", "in", json!([{"me": true}, "bob"])))
            )
            .await
        ),
        sorted(vec![km, ka, ko])
    );
}

#[tokio::test]
async fn m1_1_filter_fields() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let hid = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('HID','Hidden') RETURNING id",
    )
    .await;
    let _ = hid;
    let (restricted, _) = list_field(&w, "Secret", false).await;
    let (open_f, high) = list_field(&w, "Severity", true).await;
    // A field only for AUTH.
    let auth_only = custom_fields::create_field(
        &pool,
        NewField {
            name: "AuthOnly",
            ty: FieldType::Date,
            description: "",
            is_required: false,
            is_for_all: false,
            trackers: &[w.bug],
            projects: &[w.auth_p],
            options: &[],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();

    let all = v2::filter_fields(&w.app, a, None).await.unwrap();
    let names: Vec<&str> = all.fields.iter().map(|f| f.field.as_str()).collect();
    for b in [
        "project",
        "tracker",
        "status",
        "status_category",
        "priority",
        "assignee",
        "author",
        "fixed_version",
        "is_private",
        "start_date",
        "due_date",
        "created_at",
        "updated_at",
        "estimated_hours",
        "overdue",
    ] {
        assert!(names.contains(&b), "{b} missing");
    }
    assert!(!names.contains(&"triage"));
    assert!(!names.contains(&format!("cf:{restricted}").as_str()));
    assert!(names.contains(&format!("cf:{open_f}").as_str()));
    assert!(names.contains(&format!("cf:{auth_only}").as_str()));
    assert_eq!(all.today, org_today(&pool).await.to_string());
    assert_eq!(all.week_start, 1);
    fn f<'a>(r: &'a v2::FilterFields, name: &str) -> &'a v2::FilterFieldInfo {
        r.fields
            .iter()
            .find(|x| x.field == name)
            .unwrap_or_else(|| panic!("{name}"))
    }
    let project = f(&all, "project");
    let keys: Vec<_> = project
        .options
        .as_ref()
        .unwrap()
        .iter()
        .map(|o| o.key.clone().unwrap())
        .collect();
    assert_eq!(keys, ["AUTH", "WEB"], "HID is not listed");
    let assignee = f(&all, "assignee");
    assert_eq!(
        assignee.options_source.as_deref(),
        Some("POST /api/v1/resolve")
    );
    assert_eq!(assignee.dynamic, ["me"]);
    assert!(f(&all, "status").ops.contains(&"ever_in"));
    let sev = f(&all, &format!("cf:{open_f}"));
    assert_eq!(sev.restricted, Some(false));
    assert!(
        sev.options
            .as_ref()
            .unwrap()
            .iter()
            .any(|o| o.id == high.to_string() && o.label == "High")
    );
    assert!(f(&all, &format!("cf:{auth_only}")).ops.contains(&"within"));
    assert!(all.sortable.contains(&format!("cf:{open_f}")));
    assert!(all.sortable.contains(&"key".to_string()));

    // Within WEB: AUTH's field is gone, sources point at WEB.
    let web = v2::filter_fields(&w.app, a, Some("WEB")).await.unwrap();
    let names: Vec<&str> = web.fields.iter().map(|f| f.field.as_str()).collect();
    assert!(!names.contains(&format!("cf:{auth_only}").as_str()));
    assert!(names.contains(&format!("cf:{open_f}").as_str()));
    assert_eq!(
        f(&web, "assignee").options_source.as_deref(),
        Some("GET /api/v1/projects/WEB/assignees")
    );
    assert_eq!(
        f(&web, "fixed_version").options_source.as_deref(),
        Some("GET /api/v1/projects/WEB/versions")
    );
    let trackers = f(&web, "tracker").options.as_ref().unwrap();
    assert_eq!(trackers.len(), 1);
    assert_eq!(trackers[0].label, "Bug");

    // HID and an unknown project look the same.
    let e1 = err(v2::filter_fields(&w.app, a, Some("HID")).await.unwrap_err());
    let e2 = err(v2::filter_fields(&w.app, a, Some("NOPE"))
        .await
        .unwrap_err());
    assert_eq!(e1.0, ErrorCode::NotFound);
    same_shape(e1, e2, "HID", "NOPE");

    // describe scope filter_fields (an agent).
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let r = describe::describe(
        &w.app,
        agent,
        DescribeParams {
            scope: DescribeScope::FilterFields,
            project: Some("WEB".into()),
            tracker: None,
            issue_key: None,
        },
    )
    .await
    .unwrap();
    let ff = r.filter_fields.unwrap();
    assert!(ff.fields.iter().any(|x| x.field == format!("cf:{open_f}")));
    let e = describe::describe(
        &w.app,
        agent,
        DescribeParams {
            scope: DescribeScope::FilterFields,
            project: None,
            tracker: Some("Bug".into()),
            issue_key: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidArgument);
}

// ---------------------------------------------------------------- review fixes

/// Repeated values cost one lookup and ids one batch; distinct names are
/// capped (`MAX_NAME_LOOKUPS`).
#[tokio::test]
async fn m1_1_value_lookups_are_bounded() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    w.issue(w.web, "x").await;
    exec(
        &pool,
        "INSERT INTO issue_statuses (name, category, position) VALUES ('Reviewing','doing',4), ('X','todo',5)",
    )
    .await;
    let many =
        |v: Value, n: usize| json!({"and": vec![cond("status", "in", json!(vec![v; 100])); n]});
    // 30 × 100 copies of a name (about 13 KB).
    let t = std::time::Instant::now();
    assert_eq!(ks(&w, a, wh(many(json!("X"), 30))).await.len(), 0);
    let names = t.elapsed();
    // 3 × 100 copies of an id (16 KiB holds about 400 ids).
    let t = std::time::Instant::now();
    assert_eq!(
        ks(&w, a, wh(many(json!(w.new.to_string()), 3))).await.len(),
        1
    );
    let ids = t.elapsed();
    assert!(
        names < std::time::Duration::from_secs(1) && ids < std::time::Duration::from_secs(1),
        "names {names:?}, ids {ids:?}"
    );
    // 100 distinct ids in each of 3 conditions: one query per kind.
    let mut principals = vec![w.alice, w.bob, w.carol];
    for n in 0..97 {
        principals.push(
            id(
                &pool,
                &format!(
                    "INSERT INTO principals (kind, name) VALUES ('group', 'g{n}') RETURNING id"
                ),
            )
            .await,
        );
    }
    let pv: Vec<String> = principals.iter().map(Uuid::to_string).collect();
    let t = std::time::Instant::now();
    run(
        &w,
        a,
        wh(json!({"or": vec![cond("assignee", "in", json!(pv)); 3]})),
    )
    .await
    .unwrap();
    assert!(
        t.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        t.elapsed()
    );

    // Distinct names: case variants of "Reviewing" all resolve.
    let variants: Vec<String> = (0..512u32)
        .map(|m| {
            "reviewing"
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if m & (1 << i) != 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect()
        })
        .collect();
    assert_eq!(
        ks(&w, a, wh(cond("status", "eq", json!(variants[5]))))
            .await
            .len(),
        0,
        "a case variant resolves (no issue is Reviewing)"
    );
    let split = |k: usize| json!({"or": variants[..k].chunks(50).map(|c| cond("status", "in", json!(c))).collect::<Vec<_>>()});
    run(&w, a, wh(split(100))).await.unwrap();
    let t = std::time::Instant::now();
    let e = where_err(&w, a, split(101)).await;
    assert_eq!(
        (e.0, e.1.as_str()),
        (ErrorCode::InvalidFilter, "too_complex"),
        "{e:?}"
    );
    assert_eq!(e.2, "where.or[2].value[0]");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn m1_1_dates_out_of_range_and_unknown_version_names() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    w.issue(w.web, "x").await;
    for bad in ["-5000-01-01", "0000-12-31", "10000-01-01", "+10000-01-01"] {
        for field in ["due_date", "created_at"] {
            let e = where_err(&w, a, cond(field, "ge", json!(bad))).await;
            assert_eq!(
                (e.0, e.1.as_str(), e.2.as_str()),
                (ErrorCode::InvalidFilter, "invalid_value", "where.value"),
                "{field} {bad}"
            );
        }
    }
    assert_eq!(
        ks(
            &w,
            a,
            wh(json!({"and": [
                cond("due_date", "between", json!(["0001-01-01", "9999-12-31"])),
                cond("created_at", "between", json!(["0001-01-01", "9999-12-31"])),
            ]}))
        )
        .await
        .len(),
        0
    );
    assert_eq!(
        ks(
            &w,
            a,
            wh(cond(
                "created_at",
                "between",
                json!(["0001-01-01", "9999-12-31"])
            ))
        )
        .await
        .len(),
        1
    );
    // A version name that does not exist: unknown_value like an unknown id.
    version(&pool, w.web, "v1", day("2030-01-01"), w.alice).await;
    let named = |v: Value| json!({"and": [cond("project", "eq", json!("WEB")), cond("fixed_version", "in", v)]});
    let e = where_err(&w, a, named(json!(["nope"]))).await;
    assert_eq!(
        (e.0, e.1.as_str(), e.2.as_str()),
        (
            ErrorCode::InvalidFilter,
            "unknown_value",
            "where.and[1].value[0]"
        )
    );
    same_shape(
        e,
        where_err(&w, a, named(json!(["nada"]))).await,
        "nope",
        "nada",
    );
    run(&w, a, wh(named(json!(["v1", "V1"])))).await.unwrap();
}

/// Every cursor the server issues opens again: three string sort keys of
/// 2000 bytes that JSON escapes 6× (control characters) or 2× (quotes).
#[tokio::test]
async fn m1_1_cursor_with_the_longest_sort_values() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let mut fs = Vec::new();
    for name in ["S1", "S2", "S3"] {
        fs.push(
            custom_fields::create_field(
                &pool,
                NewField {
                    name,
                    ty: FieldType::String,
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
            .unwrap(),
        );
    }
    custom_fields::reconcile_indexes(&pool).await.unwrap();
    let mut want = Vec::new();
    for fill in ['\u{1}', '"'] {
        for tail in ['a', 'b'] {
            let i = w.issue(w.web, "long").await;
            let v: String = std::iter::repeat_n(fill, 1999).chain([tail]).collect();
            assert_eq!(v.len(), 2000);
            for f in &fs {
                custom_fields::set_value(&pool, i, *f, &FValue::Text(v.clone()))
                    .await
                    .unwrap();
            }
            want.push((v, key_of(&pool, i).await));
        }
    }
    assert_eq!(want.len(), 4);
    for keys_n in 1..=3 {
        let p = SearchParams {
            sort: Some(
                fs[..keys_n]
                    .iter()
                    .map(|f| SortInput {
                        field: format!("cf:{f}"),
                        dir: SortDir::Asc,
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        // PostgreSQL's collation orders them; one page holds all four.
        let whole = ks(
            &w,
            a,
            SearchParams {
                limit: Some(100),
                ..p.clone()
            },
        )
        .await;
        let mut all = whole.clone();
        all.sort();
        let mut keys: Vec<String> = want.iter().map(|x| x.1.clone()).collect();
        keys.sort();
        assert_eq!(all, keys);
        assert_eq!(paged(&w, a, p, 1).await, whole, "{keys_n} keys");
    }
}

/// A custom field is filtered and sorted only where the caller can read it
/// and where it applies: a value left in a project the field no longer
/// applies to, or one in a project where the caller's role may not read the
/// field, neither matches nor orders anything (it counts as missing).
#[tokio::test]
async fn m1_1_custom_field_gates_in_where_and_sort() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // Lead: alice holds it in WEB only.
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    grant(&pool, lead, &["view_issues"]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{lead}' FROM members
             WHERE project_id = '{}' AND principal_id = '{}'",
            w.web, w.alice
        ),
    )
    .await;
    let new_string = |name: &'static str, visible: bool, roles: Vec<Uuid>| {
        let pool = pool.clone();
        let bug = w.bug;
        async move {
            custom_fields::create_field(
                &pool,
                NewField {
                    name,
                    ty: FieldType::String,
                    description: "",
                    is_required: false,
                    is_for_all: true,
                    trackers: &[bug],
                    projects: &[],
                    options: &[],
                    visible,
                    roles: &roles,
                },
            )
            .await
            .unwrap()
        }
    };
    // Scoped: applies to every project now, WEB only later (AUTH keeps its values).
    let scoped = new_string("Scoped", true, vec![]).await;
    // Leads: readable only through Lead.
    let leads = new_string("Leads", false, vec![lead]).await;
    custom_fields::reconcile_indexes(&pool).await.unwrap();
    let mk = |project: Uuid, v: Option<&'static str>| {
        let w = &w;
        async move {
            let i = w.issue(project, "gated").await;
            if let Some(v) = v {
                for f in [scoped, leads] {
                    custom_fields::set_value(&w.pool, i, f, &FValue::Text(v.into()))
                        .await
                        .unwrap();
                }
            }
            (i, key_of(&w.pool, i).await)
        }
    };
    let (_, wb) = mk(w.web, Some("b")).await;
    let (_, wc) = mk(w.web, Some("c")).await;
    let (_, wn) = mk(w.web, None).await;
    let (_, aa) = mk(w.auth_p, Some("a")).await;
    let (_, ab) = mk(w.auth_p, Some("b")).await;
    exec(
        &pool,
        &format!(
            "UPDATE field_defs SET is_for_all = false WHERE id = {scoped};
             INSERT INTO field_projects VALUES ({scoped}, '{}')",
            w.web
        ),
    )
    .await;
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_cf WHERE field_id = {scoped}")
        )
        .await,
        4,
        "AUTH's values are still stored"
    );
    w.app.index.sync_all(&pool).await.unwrap();
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    for f in [scoped, leads] {
        let field = format!("cf:{f}");
        for q in [None, Some("gated")] {
            let s = |v: Value| SearchParams {
                q: q.map(String::from),
                ..wh(v)
            };
            assert_eq!(
                ks(&w, a, s(cond(&field, "eq", json!("b")))).await,
                vec![wb.clone()],
                "{field} q={q:?}: AUTH's b is not there"
            );
            assert_eq!(
                sorted(ks(&w, a, s(cond(&field, "in", json!(["a", "b", "c"])))).await),
                sorted(vec![wb.clone(), wc.clone()]),
                "{field} q={q:?}"
            );
            assert_eq!(
                ks(&w, a, s(cond(&field, "eq", json!("a")))).await,
                Vec::<String>::new(),
                "{field} q={q:?}: AUTH's a is not there"
            );
            assert_eq!(
                sorted(ks(&w, a, s(json!({"field": field, "op": "not_null"}))).await),
                sorted(vec![wb.clone(), wc.clone()]),
                "{field} q={q:?}"
            );
        }
        // Sort: AUTH's values count as missing (last, by id).
        let by = |dir: SortDir| SearchParams {
            sort: Some(vec![SortInput {
                field: field.clone(),
                dir,
            }]),
            ..Default::default()
        };
        let missing = vec![wn.clone(), aa.clone(), ab.clone()];
        let mut want = vec![wb.clone(), wc.clone()];
        want.extend(missing.clone());
        assert_eq!(paged(&w, a, by(SortDir::Asc), 2).await, want, "{field} asc");
        let mut want = vec![wc.clone(), wb.clone()];
        want.extend(missing.into_iter().rev());
        assert_eq!(
            paged(&w, a, by(SortDir::Desc), 2).await,
            want,
            "{field} desc"
        );
    }
    // bob holds Dev only: Leads is unknown to him, Scoped still filters in WEB.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = where_err(&w, bob, cond(&format!("cf:{leads}"), "eq", json!("b"))).await;
    assert_eq!(e.1, "unknown_field");
    assert_eq!(
        ks(&w, bob, wh(cond(&format!("cf:{scoped}"), "eq", json!("b")))).await,
        vec![wb]
    );
}
