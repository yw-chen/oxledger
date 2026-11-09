// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.2: saved views and boards. T-M1-06…16 (views), T-M1-17…20
//! (board drags are ordinary commands, K4), plus built-in views, agents'
//! `view_list` / `view_run` (R-VD9, B-4), project tabs of kind `view`,
//! `board` and `triage`, and a run cursor with the longest sort values over
//! HTTP (POST, so every cursor the server issues comes back).
//! Needs PostgreSQL 18; see `common::test_pool`. The count hooks are
//! process-wide, so run with `--test-threads=1`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use ox_core::filter::{FieldType, Value as FValue};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::commands;
use oxledger::custom_fields::{self, NewField};
use oxledger::describe;
use oxledger::error::{ErrorCode, classify};
use oxledger::project_tabs::{self, TabSettings, TabsPut};
use oxledger::views::{
    self, COUNT_BUDGET_MS, COUNT_DELAY_MS, NewView, PreviewInput, RunInput, RunOverride, ViewCopy,
    ViewDetail, ViewListParams, ViewPatch, ViewRunParams,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

/// (code, details.reason or details.kind, details.path)
fn err(e: anyhow::Error) -> (ErrorCode, String, String) {
    let a = classify(&e).unwrap_or_else(|| panic!("not an API error: {e:#}"));
    let d = a.details.clone().unwrap_or(Value::Null);
    let reason = d["reason"]
        .as_str()
        .or_else(|| d["kind"].as_str())
        .unwrap_or("")
        .to_string();
    (a.code, reason, d["path"].as_str().unwrap_or("").to_string())
}

fn details(e: &anyhow::Error) -> Value {
    classify(e).unwrap().details.clone().unwrap_or(Value::Null)
}

async fn create(
    w: &World,
    who: AuthContext,
    scope: &str,
    project: Option<&str>,
    name: &str,
    def: Value,
) -> anyhow::Result<ViewDetail> {
    let kind = def["kind"].as_str().unwrap_or("list").to_string();
    views::create(
        &w.app,
        who,
        Uuid::now_v7(),
        &NewView {
            scope: scope.into(),
            project_id: project.map(String::from),
            kind,
            name: name.into(),
            def,
        },
    )
    .await
}

async fn run(w: &World, who: AuthContext, id: &str, input: RunInput) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(
        views::run(&w.app, who, id, &input).await?,
    )?)
}

async fn run0(w: &World, who: AuthContext, id: &str) -> Value {
    run(w, who, id, RunInput::default()).await.unwrap()
}

fn item_keys(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect()
}

fn lane<'a>(r: &'a Value, id: &str) -> &'a Value {
    r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .unwrap_or_else(|| panic!("no lane {id} in {r}"))
}

async fn key_of(pool: &PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn set(pool: &PgPool, i: Uuid, assignments: &str) {
    exec(
        pool,
        &format!("UPDATE issues SET {assignments} WHERE id = '{i}'"),
    )
    .await;
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// A user with `role` on `project`, signed in.
async fn user(w: &World, login: &str, project: Uuid, role: Uuid) -> (Uuid, AuthContext) {
    let id = auth::create_user(&w.pool, login, login, PW, false)
        .await
        .unwrap();
    member(&w.pool, project, id, role).await;
    (id, sign_in(&w.pool, &w.policy, login).await)
}

/// A role seeing only public issues and its own (Redmine "default").
async fn viewer_role(w: &World) -> Uuid {
    let r = id(
        &w.pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(&w.pool, r, &["view_issues"]).await;
    r
}

async fn list_field(w: &World, name: &str, visible: bool, roles: &[Uuid]) -> (i32, Uuid, Uuid) {
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
            roles,
        },
    )
    .await
    .unwrap();
    custom_fields::reconcile_indexes(&w.pool).await.unwrap();
    let opt = |label: &'static str| {
        let pool = w.pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM field_options WHERE field_id = $1 AND label = $2",
            )
            .bind(f)
            .bind(label)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    (f, opt("High").await, opt("Low").await)
}

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "drag", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn tabs(w: &World, who: AuthContext, project: &str) -> TabSettings {
    project_tabs::get(&w.app, who, project).await.unwrap()
}

async fn put_tabs(
    w: &World,
    who: AuthContext,
    project: &str,
    s: &TabSettings,
    list: Vec<Value>,
) -> anyhow::Result<TabSettings> {
    let input: TabsPut = serde_json::from_value(json!({
        "lock_version": s.lock_version, "tabs": list, "default_tab": s.default_tab,
    }))
    .unwrap();
    project_tabs::put(&w.app, who, ORIGIN, Uuid::now_v7(), project, &input).await
}

fn as_input(s: &TabSettings) -> Vec<Value> {
    s.tabs
        .iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect()
}

/// Inserts `n` issues into `project` in one statement (status New, author bob).
async fn bulk(w: &World, project: Uuid, n: i32) {
    exec(
        &w.pool,
        &format!(
            "WITH base AS (
                 UPDATE projects SET issue_counter = issue_counter + {n} WHERE id = '{project}'
                 RETURNING issue_counter - {n} AS b, key),
             ins AS (
                 INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id)
                 SELECT '{project}', '{bug}', '{new}', 'bulk ' || g, '', '{bob}'
                 FROM generate_series(1, {n}) g RETURNING id),
             numbered AS (
                 SELECT ins.id, (base.b + row_number() OVER (ORDER BY ins.id))::int AS num, base.key
                 FROM ins, base)
             INSERT INTO issue_keys (key, issue_id, project_id, number)
             SELECT key || '-' || num, id, '{project}', num FROM numbered",
            bug = w.bug,
            new = w.new,
            bob = w.bob,
        ),
    )
    .await;
}

/// Puts the count hooks back however the test ends.
struct CountReset;

impl Drop for CountReset {
    fn drop(&mut self) {
        COUNT_BUDGET_MS.store(0, Ordering::SeqCst);
        COUNT_DELAY_MS.store(0, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- T-M1-06

#[tokio::test]
async fn t_m1_06_project_views_have_no_restricted_fields_and_one_project() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let a = w.alice_ctx;
    let (secret, high, _) = list_field(&w, "Customer", false, &[w.dev]).await;
    let (open_f, _, _) = list_field(&w, "Area", true, &[]).await;

    let cond_secret = json!({"kind": "list", "query": {"where":
        {"field": format!("cf:{secret}"), "op": "eq", "value": high.to_string()}}});
    let e = create(&w, a, "project", Some("WEB"), "s", cond_secret.clone())
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            ErrorCode::InvalidView,
            "restricted_field".into(),
            "def.query.where".into()
        )
    );
    let e = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "s",
        json!({"kind": "list", "columns": ["key", format!("cf:{secret}")]}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (
            ErrorCode::InvalidView,
            "restricted_field".into(),
            "def.columns[1]".into()
        )
    );
    let e = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "s",
        json!({"kind": "list", "sort": [{"field": format!("cf:{secret}"), "dir": "asc"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).1, "restricted_field");
    // A personal view may use it (only its owner sees it).
    create(&w, a, "personal", None, "mine", cond_secret)
        .await
        .unwrap();
    // A visible field is fine in a project view.
    create(
        &w,
        a,
        "project",
        Some("WEB"),
        "area",
        json!({"kind": "list", "columns": ["key", format!("cf:{open_f}")]}),
    )
    .await
    .unwrap();

    // One project only.
    let e = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "both",
        json!({"kind": "list", "query": {"where":
            {"field": "project", "op": "in", "value": ["AUTH", "WEB"]}}}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (
            ErrorCode::InvalidView,
            "project_scope".into(),
            "def.query.where".into()
        )
    );
    let e = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "other",
        json!({"kind": "list", "query": {"where":
            {"field": "project", "op": "eq", "value": "AUTH"}}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).1, "project_scope", "a bound personal view too");
    // The server names the project; an equal condition is not repeated.
    let v = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "web",
        json!({"kind": "list", "query": {"where":
            {"field": "status_category", "op": "in", "value": ["todo"]}}}),
    )
    .await
    .unwrap();
    assert_eq!(
        v.def["query"]["where"]["and"][0],
        json!({"field": "project", "op": "eq", "value": w.web.to_string()})
    );
    let v = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "web2",
        json!({"kind": "list", "query": {"where":
            {"field": "project", "op": "eq", "value": "WEB"}}}),
    )
    .await
    .unwrap();
    assert_eq!(
        v.def["query"]["where"],
        json!({"field": "project", "op": "eq", "value": w.web.to_string()})
    );
    let v = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "plain",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    assert_eq!(
        v.def["query"]["where"],
        json!({"field": "project", "op": "eq", "value": w.web.to_string()})
    );
    assert_eq!(
        v.def["columns"],
        json!([
            "key",
            "subject",
            "status",
            "assignee",
            "priority",
            "updated_at"
        ])
    );
    // Unknown keys and later stages' keys are unknown (05 §8).
    for (def, path) in [
        (
            json!({"kind": "list", "columns": ["spent_hours"]}),
            "def.columns[0]",
        ),
        (
            json!({"kind": "list", "totals": ["spent_hours"]}),
            "def.totals[0]",
        ),
        (
            json!({"kind": "list", "group_by": "triage"}),
            "def.group_by",
        ),
        (json!({"kind": "list", "colour": "red"}), "def.colour"),
        (
            json!({"kind": "board", "lanes": {"by": "status_category"}, "group_by": "status"}),
            "def.group_by",
        ),
    ] {
        let e = create(&w, a, "personal", None, "x", def).await.unwrap_err();
        assert_eq!(
            err(e),
            (ErrorCode::InvalidView, "unknown_field".into(), path.into())
        );
    }
    // A bad where keeps its invalid_filter, under def.query.where.
    let e = create(
        &w,
        a,
        "personal",
        None,
        "x",
        json!({"kind": "list", "query": {"where": {"field": "nope", "op": "eq", "value": 1}}}),
    )
    .await
    .unwrap_err();
    let (c, _, p) = err(e);
    assert_eq!(c, ErrorCode::InvalidFilter);
    assert!(p.starts_with("def.query.where"), "{p}");
}

// ---------------------------------------------------------------- T-M1-07

#[tokio::test]
async fn t_m1_07_me_stays_dynamic() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let ia = w.issue(w.web, "for alice").await;
    let ib = w.issue(w.web, "for bob").await;
    w.issue(w.web, "nobody's").await;
    set(&pool, ia, &format!("assigned_to_id = '{}'", w.alice)).await;
    set(&pool, ib, &format!("assigned_to_id = '{}'", w.bob)).await;
    let v = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "指派給我",
        json!({"kind": "list", "query": {"where":
            {"field": "assignee", "op": "eq", "value": {"me": true}}}}),
    )
    .await
    .unwrap();
    assert_eq!(
        v.def["query"]["where"]["and"][1]["value"],
        json!({"me": true})
    );
    assert_eq!(v.scope, "project");
    assert!(v.editable);
    let r = run0(&w, bob, &v.id).await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, ib).await]);
    let r = run0(&w, a, &v.id).await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, ia).await]);
    assert_eq!(r["view"]["name"], "指派給我");
    assert!(r["meta"]["today"].as_str().is_some());
    // values: one per column but key, plus overdue.
    let vals = &r["items"][0]["values"];
    assert_eq!(vals["subject"], "for alice");
    assert_eq!(vals["assignee"]["id"], json!(w.alice));
    assert_eq!(vals["status"]["category"], "todo");
    assert_eq!(vals["overdue"], false);
    assert!(vals.get("key").is_none());
    // Both may list it.
    let l = views::list(&w.app, bob, Some("WEB"), None, None, None)
        .await
        .unwrap();
    assert!(l.items.iter().any(|x| x.id == v.id));
}

// ---------------------------------------------------------------- T-M1-08

#[tokio::test]
async fn t_m1_08_someone_elses_personal_view_is_not_found() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let v = create(&w, a, "personal", None, "mine", json!({"kind": "list"}))
        .await
        .unwrap();
    let nf = |e: anyhow::Error| assert_eq!(err(e).0, ErrorCode::NotFound);
    nf(views::get(&w.app, bob, &v.id).await.unwrap_err());
    nf(run(&w, bob, &v.id, RunInput::default()).await.unwrap_err());
    nf(views::update(
        &w.app,
        bob,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: Some("x".into()),
            def: None,
        },
    )
    .await
    .unwrap_err());
    nf(views::delete(&w.app, bob, Uuid::now_v7(), &v.id, 0)
        .await
        .unwrap_err());
    nf(
        views::copy(&w.app, bob, Uuid::now_v7(), &v.id, &ViewCopy::default())
            .await
            .unwrap_err(),
    );
    nf(views::events(&w.app, bob, &v.id, None).await.unwrap_err());
    // Same answer as an id that never existed.
    nf(views::get(&w.app, bob, &Uuid::now_v7().to_string())
        .await
        .unwrap_err());
    let l = views::list(&w.app, bob, None, Some("personal"), None, None)
        .await
        .unwrap();
    assert!(l.items.is_empty());
    // An agent never sees it either.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let e = views::agent_run(
        &w.app,
        agent,
        &ViewRunParams {
            view: v.id.clone(),
            limit: None,
            cursor: None,
            lane: None,
            fields: None,
        },
    )
    .await
    .unwrap_err();
    nf(e);
    // The owner can.
    assert_eq!(views::get(&w.app, a, &v.id).await.unwrap().name, "mine");
}

// ---------------------------------------------------------------- T-M1-09

#[tokio::test]
async fn t_m1_09_project_views_need_manage_public_views_in_an_active_project() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let e = create(&w, a, "project", Some("WEB"), "p", json!({"kind": "list"}))
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            ErrorCode::ForbiddenOp,
            "manage_public_views".into(),
            "".into()
        )
    );
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let v = create(&w, a, "project", Some("WEB"), "p", json!({"kind": "list"}))
        .await
        .unwrap();
    // A reader without the permission cannot change it.
    let viewer = viewer_role(&w).await;
    let (_, dave) = user(&w, "dave", w.web, viewer).await;
    assert!(!views::get(&w.app, dave, &v.id).await.unwrap().editable);
    let e = views::update(
        &w.app,
        dave,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: Some("x".into()),
            def: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ForbiddenOp);
    let e = views::delete(&w.app, dave, Uuid::now_v7(), &v.id, 0)
        .await
        .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ForbiddenOp);
    // Closed project: even with the permission.
    exec(
        &pool,
        "UPDATE projects SET status = 'closed' WHERE key = 'WEB'",
    )
    .await;
    let e = create(&w, a, "project", Some("WEB"), "q", json!({"kind": "list"}))
        .await
        .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ForbiddenOp);
    let e = views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: Some("x".into()),
            def: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ForbiddenOp);
    // Built-ins never change.
    let e = views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        "builtin:assigned_to_me",
        &ViewPatch {
            lock_version: 0,
            name: Some("x".into()),
            def: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (ErrorCode::ForbiddenOp, "builtin_view".into(), "".into())
    );
}

// ---------------------------------------------------------------- T-M1-10

#[tokio::test]
async fn t_m1_10_a_field_turned_restricted_stops_the_run() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (f, high, _) = list_field(&w, "Area", true, &[]).await;
    let i = w.issue(w.web, "high").await;
    custom_fields::set_value(&pool, i, f, &FValue::Ref(high))
        .await
        .unwrap();
    let cond = create(
        &w,
        a,
        "personal",
        None,
        "by area",
        json!({"kind": "list", "query": {"where":
            {"field": format!("cf:{f}"), "op": "eq", "value": "High"}}}),
    )
    .await
    .unwrap();
    assert_eq!(
        cond.def["query"]["where"]["value"],
        json!({"list": high}),
        "label stored as id"
    );
    let col = create(
        &w,
        a,
        "personal",
        None,
        "with column",
        json!({"kind": "list", "columns": ["key", format!("cf:{f}")]}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &cond.id).await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, i).await]);
    let r = run0(&w, a, &col.id).await;
    assert_eq!(r["items"][0]["values"][format!("cf:{f}")]["label"], "High");

    // Restricted, alice's role not among its readers.
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = false WHERE id = {f}"),
    )
    .await;
    let e = run(&w, a, &cond.id, RunInput::default()).await.unwrap_err();
    let (c, reason, path) = err(e);
    assert_eq!(
        (c, reason.as_str()),
        (ErrorCode::InvalidFilter, "field_unavailable")
    );
    assert!(path.starts_with("def.query.where"), "{path}");
    // A column on it is left out, the run goes on.
    let r = run0(&w, a, &col.id).await;
    assert_eq!(r["columns"], json!([{"key": "key", "label": "單號"}]));
    assert!(r["items"][0]["values"].get(format!("cf:{f}")).is_none());
}

// ---------------------------------------------------------------- T-M1-11

#[tokio::test]
async fn t_m1_11_concurrent_patches() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let v = create(&w, a, "personal", None, "v", json!({"kind": "list"}))
        .await
        .unwrap();
    assert_eq!(v.lock_version, Some(0));
    let p1 = views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: Some("first".into()),
            def: None,
        },
    )
    .await
    .unwrap();
    assert_eq!((p1.name.as_str(), p1.lock_version), ("first", Some(1)));
    let e = views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: None,
            def: Some(json!({"kind": "list", "columns": ["key"]})),
        },
    )
    .await
    .unwrap_err();
    let d = details(&e);
    assert_eq!(err(e).0, ErrorCode::Conflict);
    assert_eq!(d["current_lock_version"], 1);
    assert_eq!(d["current"]["name"], "first");
    assert_eq!(d["current"]["lock_version"], 1);
    // The same Idempotency-Key again: the first result, no second change.
    let op_id = Uuid::now_v7();
    let patch = ViewPatch {
        lock_version: 1,
        name: Some("second".into()),
        def: None,
    };
    let x = views::update(&w.app, a, op_id, &v.id, &patch)
        .await
        .unwrap();
    let y = views::update(&w.app, a, op_id, &v.id, &patch)
        .await
        .unwrap();
    assert_eq!((x.lock_version, y.lock_version), (Some(2), Some(2)));
    // Name taken; renaming to its own name in another case is fine.
    create(&w, a, "personal", None, "Other", json!({"kind": "list"}))
        .await
        .unwrap();
    let e = views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 2,
            name: Some("other".into()),
            def: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ViewNameTaken);
    let e = create(&w, a, "personal", None, " OTHER ", json!({"kind": "list"}))
        .await
        .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ViewNameTaken);
    // History: created, updated ×2, newest first.
    let ev = views::events(&w.app, a, &v.id, None).await.unwrap();
    let kinds: Vec<&str> = ev.items.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, vec!["updated", "updated", "created"]);
    assert_eq!(ev.items[0].payload["after"]["name"], "second");
    assert_eq!(ev.items[0].payload["before"]["name"], "first");
}

// ---------------------------------------------------------------- T-M1-12 and tabs

#[tokio::test]
async fn t_m1_12_a_view_a_tab_shows_is_in_use() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_public_views", "manage_project"]).await;
    let a = w.alice_ctx;
    let s = tabs(&w, a, "WEB").await;
    // Board and Triage (hidden) are built in.
    let ids: Vec<(&str, bool)> = s.tabs.iter().map(|t| (t.id.as_str(), t.hidden)).collect();
    assert_eq!(
        ids,
        vec![
            ("timeline", false),
            ("milestones", false),
            ("issues", false),
            ("dev", false),
            ("plan", false),
            ("board", false),
            ("triage", true)
        ]
    );
    let list = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "Open",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    let board = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "Flow",
        json!({"kind": "board", "lanes": {"by": "status_category"}}),
    )
    .await
    .unwrap();
    let personal = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "Mine",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    let auth_view = create(
        &w,
        a,
        "project",
        Some("AUTH"),
        "Auth",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();

    // R-V1: a live project view of this project.
    for bad in [&personal.id, &auth_view.id] {
        let mut t = as_input(&s);
        t.push(json!({"kind": "view", "title": "x", "view_id": bad}));
        let e = put_tabs(&w, a, "WEB", &s, t).await.unwrap_err();
        assert_eq!(err(e).1, "tab_view_invalid");
    }
    let mut t = as_input(&s);
    t.push(json!({"kind": "view", "title": "x"}));
    assert_eq!(
        err(put_tabs(&w, a, "WEB", &s, t).await.unwrap_err()).1,
        "tab_view_invalid"
    );
    // The built-in board takes a board view only; other built-ins none.
    let mut t = as_input(&s);
    t[5]["view_id"] = json!(list.id);
    assert_eq!(
        err(put_tabs(&w, a, "WEB", &s, t).await.unwrap_err()).1,
        "tab_view_invalid"
    );
    let mut t = as_input(&s);
    t[2]["view_id"] = json!(list.id);
    assert_eq!(
        err(put_tabs(&w, a, "WEB", &s, t).await.unwrap_err()).1,
        "not_allowed"
    );
    // Triage can be turned on; the board shows a board view; a view tab.
    let mut t = as_input(&s);
    t[5]["view_id"] = json!(board.id);
    t[6]["hidden"] = json!(false);
    t.push(json!({"kind": "view", "title": "未完成", "view_id": list.id}));
    let s2 = put_tabs(&w, a, "WEB", &s, t).await.unwrap();
    assert!(!s2.tabs[6].hidden);
    assert_eq!(
        s2.tabs[5].view_id.map(|x| x.to_string()),
        Some(board.id.clone())
    );
    let vt = s2.tabs.last().unwrap();
    assert!(vt.id.starts_with("v_"), "{}", vt.id);
    assert_eq!(vt.kind, "view");

    // R-V2: both are in use.
    for (v, title) in [(&list, "未完成"), (&board, "看板")] {
        let e = views::delete(&w.app, a, Uuid::now_v7(), &v.id, 0)
            .await
            .unwrap_err();
        let d = details(&e);
        assert_eq!(err(e).0, ErrorCode::ViewInUse);
        assert_eq!(d["tabs"], json!([title]));
    }
    // Remove the tab, then delete.
    let mut t = as_input(&s2);
    t.pop();
    t[5].as_object_mut().unwrap().remove("view_id");
    let s3 = put_tabs(&w, a, "WEB", &s2, t).await.unwrap();
    assert!(s3.tabs[5].view_id.is_none());
    let d = views::delete(&w.app, a, Uuid::now_v7(), &list.id, 0)
        .await
        .unwrap();
    assert_eq!(d.lock_version, 1);
    assert_eq!(
        err(views::get(&w.app, a, &list.id).await.unwrap_err()).0,
        ErrorCode::NotFound
    );
    views::delete(&w.app, a, Uuid::now_v7(), &board.id, 0)
        .await
        .unwrap();
    // A deleted view cannot go into a tab.
    let mut t = as_input(&s3);
    t.push(json!({"kind": "view", "title": "gone", "view_id": list.id}));
    assert_eq!(
        err(put_tabs(&w, a, "WEB", &s3, t).await.unwrap_err()).1,
        "tab_view_invalid"
    );
    // Its history is still readable by... nobody: the view is gone.
    // Personal views never block deletion (no tab can show them).
    views::delete(&w.app, a, Uuid::now_v7(), &personal.id, 0)
        .await
        .unwrap();
}

// ---------------------------------------------------------------- T-M1-13

#[tokio::test]
async fn t_m1_13_invisible_issues_change_nothing() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let viewer = viewer_role(&w).await;
    let (_, dave) = user(&w, "dave", w.web, viewer).await;
    let i1 = w.issue(w.web, "one").await;
    let hidden = w.issue(w.web, "private").await;
    let i2 = w.issue(w.web, "two").await;
    set(&pool, hidden, "is_private = true").await;
    set(&pool, i2, &format!("status_id = '{}'", w.doing)).await;
    let list = create(
        &w,
        dave,
        "personal",
        Some("WEB"),
        "grouped",
        json!({"kind": "list", "group_by": "status_category",
               "sort": [{"field": "created_at", "dir": "asc"}], "totals": ["estimated_hours"]}),
    )
    .await;
    // status_category is not a group_by key; status is.
    assert_eq!(err(list.unwrap_err()).1, "unknown_field");
    let list = create(
        &w,
        dave,
        "personal",
        Some("WEB"),
        "grouped",
        json!({"kind": "list", "group_by": "status",
               "sort": [{"field": "created_at", "dir": "asc"}], "totals": ["estimated_hours"]}),
    )
    .await
    .unwrap();
    let board = create(
        &w,
        dave,
        "personal",
        Some("WEB"),
        "board",
        json!({"kind": "board", "lanes": {"by": "status_category"},
               "sort": [{"field": "created_at", "dir": "asc"}]}),
    )
    .await
    .unwrap();
    set(&pool, i1, "estimated_hours = 1.5").await;
    set(&pool, hidden, "estimated_hours = 100").await;
    set(&pool, i2, "estimated_hours = 2.25").await;

    /// Everything a page shows, without cursors (they are sealed anew).
    async fn shape(w: &World, who: AuthContext, list: &str, board: &str) -> Value {
        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let r = run(
                w,
                who,
                list,
                RunInput {
                    limit: Some(1),
                    cursor: cursor.take(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            pages.push(
                json!({"keys": item_keys(&r["items"]), "groups": r["groups"],
                              "totals": r["totals"]}),
            );
            match r["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        let b = run(
            w,
            who,
            board,
            RunInput {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let lanes: Vec<Value> = b["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                json!({"id": l["id"], "count": l["count"], "keys": item_keys(&l["items"]),
                       "more": l["next_cursor"].is_string()})
            })
            .collect();
        json!({"pages": pages, "lanes": lanes})
    }
    let with = shape(&w, dave, &list.id, &board.id).await;
    assert_eq!(with["pages"].as_array().unwrap().len(), 2);
    assert_eq!(
        with["pages"][0]["groups"],
        json!([{"value": w.new, "label": "New", "count": {"value": 1, "capped": false}}])
    );
    assert_eq!(
        with["pages"][0]["totals"],
        json!({"estimated_hours": {"value": "3.75", "complete": true}})
    );
    assert_eq!(
        with["lanes"][0]["count"],
        json!({"value": 1, "capped": false})
    );
    // Without the private issue at all.
    set(&pool, hidden, "archived_at = now()").await;
    let without = shape(&w, dave, &list.id, &board.id).await;
    assert_eq!(with, without);
    // alice (sees it) counts it.
    set(&pool, hidden, "archived_at = NULL").await;
    let r = run0(&w, w.alice_ctx, "builtin:project_board:WEB").await;
    assert_eq!(lane(&r, "todo")["count"]["value"], 2);
}

// ---------------------------------------------------------------- T-M1-14

#[tokio::test]
async fn t_m1_14_counts_cap_and_time_out() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let _reset = CountReset;
    let a = w.alice_ctx;
    bulk(&w, w.web, 1000).await;
    let r = run(
        &w,
        a,
        "builtin:project_board:WEB",
        RunInput {
            limit: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let todo = lane(&r, "todo");
    assert_eq!(todo["count"], json!({"value": 999, "capped": true}));
    assert_eq!(todo["items"].as_array().unwrap().len(), 5);
    assert_eq!(
        lane(&r, "doing")["count"],
        json!({"value": 0, "capped": false})
    );
    // WIP: exceeded is computed from the count.
    let v = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "wip",
        json!({"kind": "board", "lanes": {"by": "status_category", "items": [
            {"title": "Todo", "values": ["todo"], "wip_max": 10},
            {"title": "Doing", "values": ["doing"], "wip_max": 3}]}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &v.id).await;
    assert_eq!(lane(&r, "l1")["wip"], json!({"max": 10, "exceeded": true}));
    assert_eq!(lane(&r, "l2")["wip"], json!({"max": 3, "exceeded": false}));

    // Out of time: counts null, items still there.
    COUNT_DELAY_MS.store(300, Ordering::SeqCst);
    COUNT_BUDGET_MS.store(100, Ordering::SeqCst);
    let r = run(
        &w,
        a,
        "builtin:project_board:WEB",
        RunInput {
            limit: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let todo = lane(&r, "todo");
    assert!(todo["count"].is_null());
    assert_eq!(todo["items"].as_array().unwrap().len(), 5);
    let r = run0(&w, a, &v.id).await;
    assert_eq!(lane(&r, "l1")["wip"], json!({"max": 10, "exceeded": null}));
    let g = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "grouped",
        json!({"kind": "list", "group_by": "status", "totals": ["estimated_hours"]}),
    )
    .await
    .unwrap();
    let r = run(
        &w,
        a,
        &g.id,
        RunInput {
            limit: Some(3),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 3);
    assert!(r["groups"][0]["count"].is_null());
    assert_eq!(
        r["totals"]["estimated_hours"],
        json!({"value": null, "complete": false})
    );
    // The run's transaction survived the cancels: the next run works.
    COUNT_DELAY_MS.store(0, Ordering::SeqCst);
    COUNT_BUDGET_MS.store(0, Ordering::SeqCst);
    let r = run0(&w, a, &g.id).await;
    assert_eq!(
        r["groups"][0]["count"],
        json!({"value": 999, "capped": true})
    );
}

#[tokio::test]
async fn m1_2_free_text_has_no_counts() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let hit = w.issue(w.web, "alpha widget").await;
    w.issue(w.web, "beta gadget").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let g = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "grouped",
        json!({"kind": "list", "group_by": "status", "totals": ["estimated_hours"]}),
    )
    .await
    .unwrap();
    let q = || RunInput {
        override_: Some(RunOverride {
            q: Some("widget".into()),
            where_and: None,
        }),
        ..Default::default()
    };
    let r = run(&w, a, &g.id, q()).await.unwrap();
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, hit).await]);
    assert!(r["groups"][0]["count"].is_null());
    assert_eq!(
        r["totals"]["estimated_hours"],
        json!({"value": null, "complete": false})
    );
    assert_eq!(r["coverage"]["truncated"], false);
    let r = run(&w, a, "builtin:project_board:WEB", q()).await.unwrap();
    assert!(lane(&r, "todo")["count"].is_null());
    assert_eq!(
        item_keys(&lane(&r, "todo")["items"]),
        vec![key_of(&pool, hit).await]
    );
    // Without it, counted.
    let r = run0(&w, a, &g.id).await;
    assert_eq!(r["groups"][0]["count"]["value"], 2);
}

// ---------------------------------------------------------------- T-M1-15

#[tokio::test]
async fn t_m1_15_a_changed_view_refuses_old_cursors() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    for _ in 0..3 {
        w.issue(w.web, "x").await;
    }
    let def = json!({"kind": "list", "sort": [{"field": "created_at", "dir": "asc"}]});
    let v = create(&w, a, "personal", None, "v", def.clone())
        .await
        .unwrap();
    let other = create(&w, a, "personal", None, "other", def).await.unwrap();
    let r = run(
        &w,
        a,
        &v.id,
        RunInput {
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let c = r["next_cursor"].as_str().unwrap().to_string();
    // It works as long as nothing changed.
    let r2 = run(
        &w,
        a,
        &v.id,
        RunInput {
            limit: Some(1),
            cursor: Some(c.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_ne!(item_keys(&r2["items"]), item_keys(&r["items"]));
    // Not in another view.
    let e = run(
        &w,
        a,
        &other.id,
        RunInput {
            limit: Some(1),
            cursor: Some(c.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidCursor);
    // Not with another narrowing.
    let e = run(
        &w,
        a,
        &v.id,
        RunInput {
            limit: Some(1),
            cursor: Some(c.clone()),
            override_: Some(RunOverride {
                q: None,
                where_and: Some(json!({"field": "project", "op": "eq", "value": "WEB"})),
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidCursor);
    views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &v.id,
        &ViewPatch {
            lock_version: 0,
            name: None,
            def: Some(json!({"kind": "list", "columns": ["key", "subject"],
                             "sort": [{"field": "created_at", "dir": "asc"}]})),
        },
    )
    .await
    .unwrap();
    let e = run(
        &w,
        a,
        &v.id,
        RunInput {
            limit: Some(1),
            cursor: Some(c),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidCursor);
    // A board cursor goes with its lane.
    let e = run(
        &w,
        a,
        "builtin:project_board:WEB",
        RunInput {
            cursor: Some("x".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidArgument);
}

// ---------------------------------------------------------------- T-M1-16

#[tokio::test]
async fn t_m1_16_unmapped_statuses_are_counted_not_shown() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let i_new = w.issue(w.web, "new").await;
    let i_doing = w.issue(w.web, "doing").await;
    let i_done = w.issue(w.web, "done").await;
    set(&pool, i_doing, &format!("status_id = '{}'", w.doing)).await;
    set(&pool, i_done, &format!("status_id = '{}'", w.done)).await;
    let e = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "b",
        json!({"kind": "board", "lanes": {"by": "status"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).1, "lanes_required");
    let e = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "b",
        json!({"kind": "board", "lanes": {"by": "status", "items": [
            {"title": "A", "values": ["New"]}, {"title": "B", "values": ["New", "Doing"]}]}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).1, "duplicate_value");
    let v = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "b",
        json!({"kind": "board", "lanes": {"by": "status", "items": [
            {"id": "todo", "title": "待辦", "values": ["New"]},
            {"id": "wip", "title": "進行", "values": ["Doing"]}]}}),
    )
    .await
    .unwrap();
    assert_eq!(
        v.def["lanes"]["items"][0]["values"],
        json!([w.new.to_string()]),
        "names stored as ids"
    );
    let r = run0(&w, a, &v.id).await;
    // One status in no lane; its one issue (Done, within the done window)
    // is counted as not shown.
    assert_eq!(
        r["unmapped"],
        json!({"status_count": 1, "issues": {"value": 1, "capped": false}, "lanes_truncated": false})
    );
    assert_eq!(
        item_keys(&lane(&r, "todo")["items"]),
        vec![key_of(&pool, i_new).await]
    );
    assert_eq!(
        item_keys(&lane(&r, "wip")["items"]),
        vec![key_of(&pool, i_doing).await]
    );
    let all: Vec<String> = r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|l| item_keys(&l["items"]))
        .collect();
    assert!(!all.contains(&key_of(&pool, i_done).await));
    // Cards: card fields but key, plus overdue.
    let card = &lane(&r, "todo")["items"][0]["card"];
    assert_eq!(card["subject"], "new");
    assert!(card.get("overdue").is_some());
    assert!(card.get("due_date").is_some());
}

// ---------------------------------------------------------------- T-M1-17…20

#[tokio::test]
async fn t_m1_17_to_20_drags_are_ordinary_commands() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let i = w.issue(w.web, "drag me").await;
    let key = key_of(&pool, i).await;
    let v = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "flow",
        json!({"kind": "board", "lanes": {"by": "status", "items": [
            {"id": "todo", "title": "待辦", "values": ["New"]},
            {"id": "active", "title": "進行與完成", "values": ["Doing", "Done"]}]}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &v.id).await;
    let target: Vec<Uuid> = lane(&r, "active")["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().parse().unwrap())
        .collect();
    // T-M1-17: the lane's statuses ∩ the available transitions: exactly one.
    let acts = describe::issue_actions(&w.app, a, &key).await.unwrap();
    let to: Vec<Uuid> = acts
        .transitions
        .iter()
        .map(|t| t.to_status_id)
        .filter(|s| target.contains(s))
        .collect();
    assert_eq!(to, vec![w.doing]);
    let lv0 = lv(&w, i).await;
    // T-M1-18: someone changed it meanwhile: conflict, nothing happens.
    set(
        &pool,
        i,
        "subject = 'changed', lock_version = lock_version + 1",
    )
    .await;
    let stale = doc(
        vec![json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to[0]})],
        vec![(i, lv0)],
    );
    let e = commands::submit(&w.app, a, Uuid::now_v7(), &stale)
        .await
        .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::Conflict);
    assert_eq!(lv(&w, i).await, lv0 + 1);
    // T-M1-17 / 20: one transition, sent twice with the same key.
    let lv1 = lv(&w, i).await;
    let d = doc(
        vec![json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to[0]})],
        vec![(i, lv1)],
    );
    let key_id = Uuid::now_v7();
    let first = commands::submit(&w.app, a, key_id, &d).await.unwrap();
    let again = commands::submit(&w.app, a, key_id, &d).await.unwrap();
    assert_eq!(first.execution_id, again.execution_id);
    assert_eq!(lv(&w, i).await, lv1 + 1);
    let moves = count(
        &pool,
        &format!(
            "SELECT count(*) FROM issue_events WHERE issue_id = '{i}' AND kind = 'status_changed'"
        ),
    )
    .await;
    assert_eq!(moves, 1);
    let r = run0(&w, a, &v.id).await;
    assert_eq!(item_keys(&lane(&r, "active")["items"]), vec![key.clone()]);

    // T-M1-19: lanes by assignee; dropping on 未指派 clears it.
    set(&pool, i, &format!("assigned_to_id = '{}'", w.alice)).await;
    let by = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "people",
        json!({"kind": "board", "lanes": {"by": "assignee"}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &by.id).await;
    let ids: Vec<&str> = r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![w.alice.to_string().as_str(), "none"]);
    assert_eq!(lane(&r, "none")["title"], "未指派");
    assert_eq!(lane(&r, "none")["values"], json!([null]));
    let clear = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(i),
                    "set": {"assigned_to_id": null}}),
        ],
        vec![(i, lv(&w, i).await)],
    );
    commands::submit(&w.app, a, Uuid::now_v7(), &clear)
        .await
        .unwrap();
    let r = run0(&w, a, &by.id).await;
    assert_eq!(item_keys(&lane(&r, "none")["items"]), vec![key.clone()]);
    // Read-only by workflow in the issue's status (Doing).
    set(&pool, i, &format!("assigned_to_id = '{}'", w.alice)).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_field_rules VALUES ('{}','{}','{}','assigned_to_id','readonly')",
            w.bug, w.dev, w.doing
        ),
    )
    .await;
    let clear = doc(
        vec![
            json!({"op_id": op(), "kind": "update_fields", "target": tgt(i),
                    "set": {"assigned_to_id": null}}),
        ],
        vec![(i, lv(&w, i).await)],
    );
    let e = commands::submit(&w.app, a, Uuid::now_v7(), &clear)
        .await
        .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ReadonlyField);
}

// ---------------------------------------------------------------- built-in views

#[tokio::test]
async fn m1_2_builtin_views() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let mine = w.issue(w.web, "assigned to alice").await;
    let wrote = w.issue(w.auth_p, "alice wrote here").await;
    let closed = w.issue(w.web, "closed, assigned").await;
    let other = w.issue(w.web, "not alice's").await;
    set(&pool, mine, &format!("assigned_to_id = '{}'", w.alice)).await;
    set(
        &pool,
        closed,
        &format!("assigned_to_id = '{}', status_id = '{}'", w.alice, w.done),
    )
    .await;
    // alice writes a note on `wrote` (an issue event of hers).
    let note = json!({"command_schema_version": 1, "title": "n",
        "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(wrote), "body": "hi"}],
        "preconditions": [{"target": tgt(wrote), "lock_version": lv(&w, wrote).await}]});
    commands::submit(&w.app, a, Uuid::now_v7(), &note)
        .await
        .unwrap();

    let l = views::list(&w.app, a, None, Some("builtin"), None, None)
        .await
        .unwrap();
    let ids: Vec<&str> = l.items.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "builtin:assigned_to_me",
            "builtin:reported_by_me",
            "builtin:involving_me"
        ]
    );
    let l = views::list(&w.app, a, Some("WEB"), Some("builtin"), None, None)
        .await
        .unwrap();
    let ids: Vec<String> = l.items.iter().map(|x| x.id.clone()).collect();
    assert_eq!(
        ids,
        vec![
            format!("builtin:project_issues:{}", w.web),
            format!("builtin:project_board:{}", w.web)
        ]
    );
    let r = run0(&w, a, "builtin:assigned_to_me").await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, mine).await]);
    let r = run0(&w, a, "builtin:involving_me").await;
    let mut got = item_keys(&r["items"]);
    got.sort();
    let mut want = vec![
        key_of(&pool, mine).await,
        key_of(&pool, wrote).await,
        key_of(&pool, closed).await,
    ];
    want.sort();
    assert_eq!(got, want);
    let r = run0(&w, a, "builtin:reported_by_me").await;
    assert!(item_keys(&r["items"]).is_empty(), "bob wrote them all");
    let r = run0(&w, a, &format!("builtin:project_issues:{}", w.web)).await;
    let mut got = item_keys(&r["items"]);
    got.sort();
    let mut want = vec![key_of(&pool, mine).await, key_of(&pool, other).await];
    want.sort();
    assert_eq!(got, want, "open issues of WEB");
    // The key works as well as the id.
    let r = run0(&w, a, "builtin:project_board:WEB").await;
    assert_eq!(r["view"]["name"], "看板");
    assert_eq!(lane(&r, "done")["count"]["value"], 1);
    // Unknown, or a project that is not visible: not_found.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    for id in ["builtin:nope", "builtin:project_board:NOPE"] {
        assert_eq!(
            err(views::get(&w.app, a, id).await.unwrap_err()).0,
            ErrorCode::NotFound
        );
    }
    assert_eq!(
        err(views::get(&w.app, carol, "builtin:project_board:AUTH")
            .await
            .unwrap_err())
        .0,
        ErrorCode::NotFound
    );

    // Copy: built-ins are copied as personal views; involving_me is not.
    let c = views::copy(
        &w.app,
        a,
        Uuid::now_v7(),
        "builtin:assigned_to_me",
        &ViewCopy::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        (c.scope.as_str(), c.name.as_str()),
        ("personal", "指派給我（副本）")
    );
    let r = run0(&w, a, &c.id).await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, mine).await]);
    let e = views::copy(
        &w.app,
        a,
        Uuid::now_v7(),
        "builtin:involving_me",
        &ViewCopy::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (ErrorCode::ForbiddenOp, "not_copyable".into(), "".into())
    );
    // A project built-in copied into another project gets that project.
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let c = views::copy(
        &w.app,
        a,
        Uuid::now_v7(),
        "builtin:project_board:WEB",
        &ViewCopy {
            scope: Some("project".into()),
            project_id: Some("AUTH".into()),
            name: Some("Auth board".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(c.kind, "board");
    assert_eq!(
        c.def["query"]["where"],
        json!({"field": "project", "op": "eq", "value": w.auth_p.to_string()})
    );
    let ev = views::events(&w.app, a, &c.id, None).await.unwrap();
    assert_eq!(
        ev.items[0].payload["copied_from"],
        format!("builtin:project_board:{}", w.web)
    );
}

// ---------------------------------------------------------------- lists, boards, preview

#[tokio::test]
async fn m1_2_groups_totals_lanes_and_preview() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (f, high, low) = list_field(&w, "Area", true, &[]).await;
    let i1 = w.issue(w.web, "a").await;
    let i2 = w.issue(w.web, "b").await;
    let i3 = w.issue(w.web, "c").await;
    custom_fields::set_value(&pool, i1, f, &FValue::Ref(high))
        .await
        .unwrap();
    custom_fields::set_value(&pool, i2, f, &FValue::Ref(low))
        .await
        .unwrap();
    set(&pool, i1, &format!("assigned_to_id = '{}'", w.bob)).await;
    set(&pool, i3, "estimated_hours = 0.5, due_date = '2001-01-01'").await;

    // Group by a list custom field: High, Low, 未設定 (by option position).
    let g = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "by area",
        json!({"kind": "list", "group_by": format!("cf:{f}"),
               "columns": ["key", "subject", "due_date", format!("cf:{f}")],
               "sort": [{"field": "created_at", "dir": "asc"}], "totals": ["estimated_hours"]}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &g.id).await;
    assert_eq!(
        item_keys(&r["items"]),
        vec![
            key_of(&pool, i1).await,
            key_of(&pool, i2).await,
            key_of(&pool, i3).await
        ]
    );
    let groups: Vec<(Value, Value, Value)> = r["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["value"].clone(),
                g["label"].clone(),
                g["count"]["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (json!(high), json!("High"), json!(1)),
            (json!(low), json!("Low"), json!(1)),
            (Value::Null, json!("未設定"), json!(1)),
        ]
    );
    assert_eq!(r["items"][0]["group"], json!(high));
    assert_eq!(r["items"][2]["values"]["overdue"], true);
    assert_eq!(r["totals"]["estimated_hours"]["value"], "0.50");
    let cols: Vec<&str> = r["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    assert_eq!(cols[3], "Area");
    // Group paging keeps the group order (limit 1 each).
    let mut keys = Vec::new();
    let mut cursor = None;
    loop {
        let r = run(
            &w,
            a,
            &g.id,
            RunInput {
                limit: Some(1),
                cursor: cursor.take(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        keys.extend(item_keys(&r["items"]));
        match r["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(keys.len(), 3);
    // override.where_and narrows (not saved).
    let r = run(
        &w,
        a,
        &g.id,
        RunInput {
            override_: Some(RunOverride {
                q: None,
                where_and: Some(json!({"field": "assignee", "op": "is_null"})),
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        views::get(&w.app, a, &g.id).await.unwrap().lock_version,
        Some(0)
    );

    // A board by the list field, auto lanes (options + 未設定), swimlanes.
    let b = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "area board",
        json!({"kind": "board", "lanes": {"by": format!("cf:{f}")},
               "swimlanes": {"by": "assignee"}, "card_fields": ["subject", "assignee"]}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &b.id).await;
    let lanes: Vec<(String, i64)> = r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["title"].as_str().unwrap().to_string(),
                l["count"]["value"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        lanes,
        vec![("High".into(), 1), ("Low".into(), 1), ("未設定".into(), 1)]
    );
    let sw: Vec<&str> = r["swimlanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["label"].as_str().unwrap())
        .collect();
    assert_eq!(sw, vec!["bob", "未指派"]);
    assert_eq!(
        lane(&r, &high.to_string())["items"][0]["swimlane"],
        json!(w.bob)
    );
    // Only one swimlane.
    let r = run(
        &w,
        a,
        &b.id,
        RunInput {
            swimlane: Some("none".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(lane(&r, &high.to_string())["count"]["value"], 0);
    assert_eq!(lane(&r, &low.to_string())["count"]["value"], 1);
    // One lane, paged.
    let r = run(
        &w,
        a,
        &b.id,
        RunInput {
            lane: Some(low.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r["lanes"].as_array().unwrap().len(), 1);
    // Lanes by fixed_version need a project (or their items).
    let e = create(
        &w,
        a,
        "personal",
        None,
        "versions",
        json!({"kind": "board", "lanes": {"by": "fixed_version"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).1, "lanes_required");
    // done_window_days: a done issue updated long ago leaves the board.
    set(
        &pool,
        i3,
        &format!(
            "status_id = '{}', updated_at = now() - interval '30 days'",
            w.done
        ),
    )
    .await;
    let r = run0(&w, a, "builtin:project_board:WEB").await;
    assert_eq!(lane(&r, "done")["count"]["value"], 0);

    // Preview: the same checks as saving, nothing saved.
    let before = views::list(&w.app, a, None, Some("personal"), None, None)
        .await
        .unwrap()
        .items
        .len();
    let p = views::preview(
        &w.app,
        a,
        &PreviewInput {
            def: json!({"kind": "list", "query": {"where":
                {"field": "status_category", "op": "in", "value": ["todo"]}}}),
            scope: None,
            project_id: Some("WEB".into()),
            cursor: None,
            limit: None,
            lane: None,
            swimlane: None,
            override_: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(p.view.id, "preview");
    let after = views::list(&w.app, a, None, Some("personal"), None, None)
        .await
        .unwrap()
        .items
        .len();
    assert_eq!(before, after);
    let e = views::preview(
        &w.app,
        a,
        &PreviewInput {
            def: json!({"columns": ["key"]}),
            scope: None,
            project_id: None,
            cursor: None,
            limit: None,
            lane: None,
            swimlane: None,
            override_: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::InvalidView);
}

// ---------------------------------------------------------------- agents

#[tokio::test]
async fn m1_2_agents_list_and_run_views() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let a = w.alice_ctx;
    let i = w.issue(w.web, "web issue").await;
    w.issue(w.auth_p, "auth issue").await;
    let shared = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "Shared",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    let with_me = create(
        &w,
        a,
        "project",
        Some("WEB"),
        "Mine",
        json!({"kind": "list", "query": {"where":
            {"field": "assignee", "op": "eq", "value": {"me": true}}}}),
    )
    .await
    .unwrap();
    let auth_shared = create(
        &w,
        a,
        "project",
        Some("AUTH"),
        "AuthShared",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    let personal = create(&w, a, "personal", None, "Private", json!({"kind": "list"}))
        .await
        .unwrap();
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;

    let l = views::agent_list(&w.app, agent, &ViewListParams { project: None })
        .await
        .unwrap();
    let ids: Vec<&str> = l.items.iter().map(|x| x.id.as_str()).collect();
    assert!(ids.contains(&shared.id.as_str()));
    assert!(ids.contains(&with_me.id.as_str()));
    assert!(!ids.contains(&auth_shared.id.as_str()), "not delegated");
    assert!(!ids.contains(&personal.id.as_str()), "B-4");
    assert!(!ids.iter().any(|x| x.contains("_me")), "{ids:?}");
    let pb = format!("builtin:project_board:{}", w.web);
    assert!(ids.contains(&pb.as_str()));
    assert!(l.items.iter().all(|x| !x.editable));

    let params = |view: &str| ViewRunParams {
        view: view.into(),
        limit: None,
        cursor: None,
        lane: None,
        fields: None,
    };
    let r = views::agent_run(&w.app, agent, &params(&shared.id))
        .await
        .unwrap();
    let keys: Vec<String> = r.items.unwrap().iter().map(|x| x.key.clone()).collect();
    assert_eq!(keys, vec![key_of(&pool, i).await]);
    let r =
        serde_json::to_value(views::agent_run(&w.app, agent, &params(&pb)).await.unwrap()).unwrap();
    assert_eq!(r["lanes"][0]["id"], "todo");
    assert_eq!(r["lanes"][0]["count"]["value"], 1);
    assert_eq!(r["lanes"][0]["items"][0]["key"], key_of(&pool, i).await);
    // R-VD9: me is not the delegator.
    let e = views::agent_run(&w.app, agent, &params(&with_me.id))
        .await
        .unwrap_err();
    assert_eq!(err(e).1, "me_unavailable");
    let e = views::agent_run(&w.app, agent, &params("builtin:assigned_to_me"))
        .await
        .unwrap_err();
    assert_eq!(err(e).1, "me_unavailable");
    for id in [auth_shared.id.as_str(), personal.id.as_str()] {
        let e = views::agent_run(&w.app, agent, &params(id))
            .await
            .unwrap_err();
        assert_eq!(err(e).0, ErrorCode::NotFound, "{id}");
    }
    // The REST endpoints are for people.
    let e = views::get(&w.app, agent, &shared.id).await.unwrap_err();
    assert_eq!(err(e).0, ErrorCode::HumanSessionRequired);
}

// ---------------------------------------------------------------- HTTP: the longest cursor

const ORIGIN_SPA: &str = "https://spa.test";

struct Browser {
    session: String,
    csrf: String,
}

async fn login(r: &Router, login: &str) -> Browser {
    let lc = r
        .clone()
        .oneshot(
            Request::get("/api/v1/auth/login-context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = lc
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{LOGIN_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={cookie}"))
                .header("x-csrf-token", token)
                .header(header::ORIGIN, ORIGIN_SPA)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

async fn call(
    r: &Router,
    b: &Browser,
    method: &str,
    path: &str,
    body: Value,
    key: Option<Uuid>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
        .header("x-csrf-token", &b.csrf)
        .header(header::ORIGIN, ORIGIN_SPA)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = key {
        req = req.header("idempotency-key", k.to_string());
    }
    let resp = r
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn m1_2_http_run_with_the_longest_cursor() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    );
    let b = login(&r, "alice").await;
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
    let mut keys = Vec::new();
    for tail in ['a', 'b', 'c'] {
        let i = w.issue(w.web, "long").await;
        let v: String = std::iter::repeat_n('\u{1}', 1999).chain([tail]).collect();
        for f in &fs {
            custom_fields::set_value(&pool, i, *f, &FValue::Text(v.clone()))
                .await
                .unwrap();
        }
        keys.push(key_of(&pool, i).await);
    }
    // Saved over HTTP, grouped (the group's lead columns ride along too).
    let (st, v) = call(
        &r,
        &b,
        "POST",
        "/views",
        json!({"scope": "personal", "project_id": "WEB", "kind": "list", "name": "long",
               "def": {"kind": "list", "group_by": "status",
                       "sort": fs.iter().map(|f| json!({"field": format!("cf:{f}"), "dir": "asc"}))
                                 .collect::<Vec<_>>()}}),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    // No Idempotency-Key: refused.
    let (st, _) = call(
        &r,
        &b,
        "POST",
        "/views",
        json!({"scope": "personal", "kind": "list", "name": "x", "def": {}}),
        None,
    )
    .await;
    assert_eq!(st.as_u16() / 100, 4);
    let mut got = Vec::new();
    let mut cursor: Option<String> = None;
    let mut longest = 0;
    loop {
        let (st, page) = call(
            &r,
            &b,
            "POST",
            &format!("/views/{id}/run"),
            json!({"limit": 1, "cursor": cursor}),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{page}");
        got.extend(item_keys(&page["data"]["items"]));
        match page["data"]["next_cursor"].as_str() {
            Some(c) => {
                longest = longest.max(c.len());
                cursor = Some(c.to_string());
            }
            None => break,
        }
        assert!(got.len() <= 3);
    }
    assert_eq!(got, keys);
    assert!(longest > 36_000, "cursor of {longest} characters");
    // Other routes answer.
    let (st, _) = call(&r, &b, "GET", &format!("/views/{id}"), Value::Null, None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, l) = call(&r, &b, "GET", "/views?scope=personal", Value::Null, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(l["data"]["items"][0]["id"], id);
    let (st, e) = call(
        &r,
        &b,
        "DELETE",
        &format!("/views/{id}"),
        Value::Null,
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st.as_u16(), 400, "lock_version required: {e}");
    let (st, _) = call(
        &r,
        &b,
        "DELETE",
        &format!("/views/{id}?lock_version=0"),
        Value::Null,
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------- verification fixes

/// A field of `ty` on Bug in WEB only (so AUTH issues are 不適用).
async fn web_only_field(w: &World, name: &str, ty: FieldType, options: &[&str]) -> i32 {
    let f = custom_fields::create_field(
        &w.pool,
        NewField {
            name,
            ty,
            description: "",
            is_required: false,
            is_for_all: false,
            trackers: &[w.bug],
            projects: &[w.web],
            options,
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    custom_fields::reconcile_indexes(&w.pool).await.unwrap();
    f
}

async fn option_id(pool: &PgPool, f: i32, label: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM field_options WHERE field_id = $1 AND label = $2")
        .bind(f)
        .bind(label)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn lane_counts(r: &Value) -> Vec<(String, Value)> {
    r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["title"].as_str().unwrap().to_string(),
                l["count"]["value"].clone(),
            )
        })
        .collect()
}

fn lane_ids(r: &Value) -> Vec<String> {
    r["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect()
}

const VIEW_ROW: &str = r#"INSERT INTO view_defs (scope, owner_id, kind, name, name_key, def, updated_by)
     VALUES ('personal', $1, 'list', $2, lower($2), '{"kind": "list"}', $1)"#;

fn db_code(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.to_string())
}

#[tokio::test]
async fn m1_2_not_applicable_fields_and_user_field_boards() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let area = web_only_field(&w, "Area", FieldType::List, &["High", "Low"]).await;
    let owner = web_only_field(&w, "Owner", FieldType::User, &[]).await;
    let high = option_id(&pool, area, "High").await;
    let low = option_id(&pool, area, "Low").await;
    let w1 = w.issue(w.web, "w1").await;
    let w2 = w.issue(w.web, "w2").await;
    let w3 = w.issue(w.web, "w3").await;
    let a1 = w.issue(w.auth_p, "a1").await;
    let a2 = w.issue(w.auth_p, "a2").await;
    custom_fields::set_value(&pool, w1, area, &FValue::Ref(high))
        .await
        .unwrap();
    custom_fields::set_value(&pool, w3, area, &FValue::Ref(low))
        .await
        .unwrap();
    custom_fields::set_value(&pool, w1, owner, &FValue::Ref(w.alice))
        .await
        .unwrap();

    // A list grouped by it (no project): 未設定 is "applies, empty",
    // 不適用 is "does not apply"; counts agree with what each group shows.
    let g = create(
        &w,
        a,
        "personal",
        None,
        "by area",
        json!({"kind": "list", "group_by": format!("cf:{area}"),
               "sort": [{"field": "created_at", "dir": "asc"}]}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &g.id).await;
    let mut want = Vec::new();
    for i in [w1, w3, w2, a1, a2] {
        want.push(key_of(&pool, i).await);
    }
    assert_eq!(item_keys(&r["items"]), want);
    let groups: Vec<(Value, Value, Value)> = r["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["value"].clone(),
                g["label"].clone(),
                g["count"]["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (json!(high), json!("High"), json!(1)),
            (json!(low), json!("Low"), json!(1)),
            (Value::Null, json!("未設定"), json!(1)),
            (json!("not_applicable"), json!("不適用"), json!(2)),
        ]
    );
    assert_eq!(r["items"][3]["group"], "not_applicable");

    // A board by the user field (auto lanes): no 500 (the gate's
    // parameters bind), and the AUTH issues are counted as not shown.
    let b = create(
        &w,
        a,
        "personal",
        None,
        "by owner",
        json!({"kind": "board", "lanes": {"by": format!("cf:{owner}")}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &b.id).await;
    assert_eq!(
        lane_counts(&r),
        vec![("alice".into(), json!(1)), ("未設定".into(), json!(2))]
    );
    assert_eq!(
        r["unmapped"],
        json!({"issues": {"value": 2, "capped": false}, "lanes_truncated": false})
    );
    // Over HTTP too.
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    );
    let br = login(&router, "alice").await;
    let (st, body) = call(
        &router,
        &br,
        "POST",
        &format!("/views/{}/run", b.id),
        json!({}),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // A board by the list field with Low made inactive: Low has no lane;
    // its issue and the AUTH ones are counted as not shown.
    exec(
        &pool,
        &format!("UPDATE field_options SET active = false WHERE id = '{low}'"),
    )
    .await;
    let lb = create(
        &w,
        a,
        "personal",
        None,
        "by area board",
        json!({"kind": "board", "lanes": {"by": format!("cf:{area}")}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &lb.id).await;
    assert_eq!(
        lane_counts(&r),
        vec![("High".into(), json!(1)), ("未設定".into(), json!(1))]
    );
    assert_eq!(
        r["unmapped"]["issues"],
        json!({"value": 3, "capped": false})
    );
}

#[tokio::test]
async fn m1_2_people_lanes_do_not_depend_on_counting() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let _reset = CountReset;
    let a = w.alice_ctx;
    let mine = w.issue(w.web, "alice's").await;
    set(&pool, mine, &format!("assigned_to_id = '{}'", w.alice)).await;
    // carol only has a done issue that left the done window: no lane.
    let old = w.issue(w.web, "old").await;
    set(
        &pool,
        old,
        &format!(
            "assigned_to_id = '{}', status_id = '{}', updated_at = now() - interval '31 days'",
            w.carol, w.done
        ),
    )
    .await;
    let b = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "people",
        json!({"kind": "board", "lanes": {"by": "assignee", "show_empty": true}}),
    )
    .await
    .unwrap();
    let r = run0(&w, a, &b.id).await;
    assert_eq!(lane_ids(&r), vec![w.alice.to_string(), "none".into()]);
    // Out of count time: the same lanes, counts null.
    COUNT_DELAY_MS.store(300, Ordering::SeqCst);
    COUNT_BUDGET_MS.store(100, Ordering::SeqCst);
    let r = run0(&w, a, &b.id).await;
    assert_eq!(lane_ids(&r), vec![w.alice.to_string(), "none".into()]);
    assert!(lane(&r, &w.alice.to_string())["count"].is_null());
    assert_eq!(
        item_keys(&lane(&r, &w.alice.to_string())["items"]),
        vec![key_of(&pool, mine).await]
    );
    assert!(r["unmapped"]["issues"].is_null());
    COUNT_DELAY_MS.store(0, Ordering::SeqCst);
    COUNT_BUDGET_MS.store(0, Ordering::SeqCst);

    // 31 more people: 29 lanes of people + 未指派 (30); the issues of the
    // three left out are counted as not shown.
    exec(
        &pool,
        "INSERT INTO principals (kind, login, name)
         SELECT 'user', 'u' || g, 'u' || lpad(g::text, 2, '0') FROM generate_series(1, 31) g",
    )
    .await;
    bulk(&w, w.web, 31).await;
    exec(
        &pool,
        "WITH u AS (SELECT id, row_number() OVER (ORDER BY name) n FROM principals WHERE login LIKE 'u%'),
              i AS (SELECT id, row_number() OVER (ORDER BY id) n FROM issues WHERE subject LIKE 'bulk %')
         UPDATE issues SET assigned_to_id = u.id FROM u, i WHERE issues.id = i.id AND u.n = i.n",
    )
    .await;
    let r = run0(&w, a, &b.id).await;
    let got = lane_ids(&r);
    assert_eq!(got.len(), 30, "{got:?}");
    assert_eq!(got.last().unwrap(), "none");
    // By name: alice, u01…u28; u29…u31 are left out.
    assert_eq!(got[0], w.alice.to_string());
    let u29: String = sqlx::query_scalar("SELECT id::text FROM principals WHERE name = 'u29'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!got.contains(&u29));
    assert_eq!(
        r["unmapped"],
        json!({"issues": {"value": 3, "capped": false}, "lanes_truncated": true})
    );
}

#[tokio::test]
async fn m1_2_concurrent_rename_is_name_taken() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let v = create(&w, a, "personal", None, "v", json!({"kind": "list"}))
        .await
        .unwrap();
    // Another transaction holds a new view named "Race", not yet committed:
    // the rename's own check does not see it; the unique index does.
    let mut t = pool.begin().await.unwrap();
    sqlx::query(VIEW_ROW)
        .bind(w.alice)
        .bind("Race")
        .execute(&mut *t)
        .await
        .unwrap();
    let app = w.app.clone();
    let id = v.id.clone();
    let rename = tokio::spawn(async move {
        views::update(
            &app,
            a,
            Uuid::now_v7(),
            &id,
            &ViewPatch {
                lock_version: 0,
                name: Some("race".into()),
                def: None,
            },
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    t.commit().await.unwrap();
    let e = rename.await.unwrap().unwrap_err();
    assert_eq!(err(e).0, ErrorCode::ViewNameTaken);
    assert_eq!(views::get(&w.app, a, &v.id).await.unwrap().name, "v");
}

#[tokio::test]
async fn m1_2_copy_replays_after_the_source_changes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let src = create(&w, a, "personal", None, "src", json!({"kind": "list"}))
        .await
        .unwrap();
    let key = Uuid::now_v7();
    let input = ViewCopy {
        scope: None,
        project_id: None,
        name: Some("copy".into()),
    };
    let first = views::copy(&w.app, a, key, &src.id, &input).await.unwrap();
    views::update(
        &w.app,
        a,
        Uuid::now_v7(),
        &src.id,
        &ViewPatch {
            lock_version: 0,
            name: Some("renamed".into()),
            def: None,
        },
    )
    .await
    .unwrap();
    let again = views::copy(&w.app, a, key, &src.id, &input).await.unwrap();
    assert_eq!(again.id, first.id);
    views::delete(&w.app, a, Uuid::now_v7(), &src.id, 1)
        .await
        .unwrap();
    let again = views::copy(&w.app, a, key, &src.id, &input).await.unwrap();
    assert_eq!(again.id, first.id);
    let n = count(
        &pool,
        "SELECT count(*) FROM view_defs WHERE name = 'copy' AND deleted_at IS NULL",
    )
    .await;
    assert_eq!(n, 1);
    // Another body under the same key is still refused.
    let e = views::copy(
        &w.app,
        a,
        key,
        &src.id,
        &ViewCopy {
            name: Some("other".into()),
            ..ViewCopy::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err(e).0, ErrorCode::IdempotencyKeyReused);
}

#[tokio::test]
async fn m1_2_a_personal_view_of_a_project_left_behind() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let v = create(
        &w,
        bob,
        "personal",
        Some("AUTH"),
        "auth only",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    assert_eq!(v.project.as_ref().unwrap().name, "Auth");
    exec(
        &pool,
        &format!(
            "DELETE FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            w.auth_p, w.bob
        ),
    )
    .await;
    let d = views::get(&w.app, bob, &v.id).await.unwrap();
    assert!(d.project.is_none());
    assert!(d.project_unavailable);
    let l = views::list(&w.app, bob, None, Some("personal"), None, None)
        .await
        .unwrap();
    let s = serde_json::to_value(&l).unwrap();
    assert_eq!(s["items"][0]["id"], json!(v.id));
    assert_eq!(s["items"][0]["project_unavailable"], true);
    assert!(s["items"][0].get("project").is_none());
    assert!(!s.to_string().contains("Auth"), "{s}");
    let e = run(&w, bob, &v.id, RunInput::default()).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            ErrorCode::InvalidView,
            "project_unavailable".into(),
            "project_id".into()
        )
    );
    // It can still be deleted.
    views::delete(&w.app, bob, Uuid::now_v7(), &v.id, 0)
        .await
        .unwrap();
}

#[tokio::test]
async fn m1_2_delete_checks_lock_version() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let v = create(&w, a, "personal", None, "v", json!({"kind": "list"}))
        .await
        .unwrap();
    let e = views::delete(&w.app, a, Uuid::now_v7(), &v.id, 5)
        .await
        .unwrap_err();
    let d = details(&e);
    assert_eq!(err(e).0, ErrorCode::Conflict);
    assert_eq!(d["current_lock_version"], 0);
    assert_eq!(views::get(&w.app, a, &v.id).await.unwrap().name, "v");
    views::delete(&w.app, a, Uuid::now_v7(), &v.id, 0)
        .await
        .unwrap();
}

#[tokio::test]
async fn m1_2_involving_me_counts_writes_of_the_last_30_days() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let recent = w.issue(w.web, "29 days").await;
    let stale = w.issue(w.web, "31 days").await;
    for i in [recent, stale] {
        let note = json!({"command_schema_version": 1, "title": "n",
            "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "hi"}],
            "preconditions": [{"target": tgt(i), "lock_version": lv(&w, i).await}]});
        commands::submit(&w.app, a, Uuid::now_v7(), &note)
            .await
            .unwrap();
    }
    for (i, days) in [(recent, 29), (stale, 31)] {
        exec(
            &pool,
            &format!(
                "UPDATE issue_events SET at = now() - interval '{days} days'
                 WHERE issue_id = '{i}' AND actor_id = '{}'",
                w.alice
            ),
        )
        .await;
    }
    let r = run0(&w, a, "builtin:involving_me").await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, recent).await]);
}

#[tokio::test]
async fn m1_2_bound_project_holds_at_run_time() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let web = w.issue(w.web, "web").await;
    w.issue(w.auth_p, "auth").await;
    let v = create(
        &w,
        a,
        "personal",
        Some("WEB"),
        "web",
        json!({"kind": "list"}),
    )
    .await
    .unwrap();
    // A saved definition without its project condition (as one saved at
    // the depth or condition limit is): the run still keeps to WEB.
    exec(
        &pool,
        &format!(
            "UPDATE view_defs SET def = def #- '{{query,where}}' WHERE id = '{}'",
            v.id
        ),
    )
    .await;
    let stored = views::get(&w.app, a, &v.id).await.unwrap();
    assert!(stored.def["query"].get("where").is_none(), "{}", stored.def);
    let r = run0(&w, a, &v.id).await;
    assert_eq!(item_keys(&r["items"]), vec![key_of(&pool, web).await]);
}

#[tokio::test]
async fn m1_2_agents_cannot_own_views() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    w.delegate(w.web).await.unwrap();
    let agent: Uuid = sqlx::query_scalar("SELECT id FROM principals WHERE kind = 'agent' LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let e = sqlx::query(VIEW_ROW)
        .bind(agent)
        .bind("x")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(db_code(&e).as_deref(), Some("23514"), "{e}");
    sqlx::query(VIEW_ROW)
        .bind(w.alice)
        .bind("x")
        .execute(&pool)
        .await
        .unwrap();
    let e = sqlx::query("UPDATE view_defs SET owner_id = $1 WHERE owner_id = $2")
        .bind(agent)
        .bind(w.alice)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(db_code(&e).as_deref(), Some("23514"), "{e}");
}
