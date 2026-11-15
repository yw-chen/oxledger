// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.6: project tabs (§18, U44): T-PT1…T-PT17. T-PT11 is split
//! in two layers (B1): HTTP answers a bearer with `unauthenticated` (§4.2),
//! the service answers `human_session_required`. T-PT17's hide action is
//! S6b.9; here the events are inserted directly to test the filter.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::error::ErrorCode;
use oxledger::project_tabs::{self, BUILTIN, Tab, TabSettings, TabsPut, TabsReset};
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

const ORIGIN_SPA: &str = "https://spa.test";

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    )
}

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

fn strip_request_id(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "request_id")
                .map(|(k, x)| (k.clone(), strip_request_id(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_request_id).collect()),
        _ => v.clone(),
    }
}

async fn user(w: &World, login: &str, project: Uuid, role: Uuid) -> (Uuid, AuthContext) {
    let id = auth::create_user(&w.pool, login, login, PW, false)
        .await
        .unwrap();
    member(&w.pool, project, id, role).await;
    (id, sign_in(&w.pool, &w.policy, login).await)
}

async fn role(w: &World, name: &str, vis: &str, perms: &[&str]) -> Uuid {
    let r = id(
        &w.pool,
        &format!(
            "INSERT INTO roles (name, issues_visibility) VALUES ('{name}','{vis}') RETURNING id"
        ),
    )
    .await;
    grant(&w.pool, r, perms).await;
    r
}

async fn mk(w: &World, subject: &'static str, author: Uuid, private: bool) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.auth_p,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description: "",
            author_id: author,
            assigned_to_id: None,
            is_private: private,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

fn input(s: &TabSettings, tabs: Vec<Value>, default_tab: &str) -> TabsPut {
    serde_json::from_value(
        json!({"lock_version": s.lock_version, "tabs": tabs, "default_tab": default_tab}),
    )
    .unwrap()
}

fn as_input(tabs: &[Tab]) -> Vec<Value> {
    tabs.iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect()
}

async fn get(w: &World, who: AuthContext) -> TabSettings {
    project_tabs::get(&w.app, who, "AUTH").await.unwrap()
}

async fn put(w: &World, who: AuthContext, p: &TabsPut) -> anyhow::Result<TabSettings> {
    project_tabs::put(&w.app, who, ORIGIN_SPA, Uuid::now_v7(), "AUTH", p).await
}

fn kind_of(e: &anyhow::Error) -> String {
    oxledger::error::classify(e)
        .and_then(|a| a.details)
        .and_then(|d| d["kind"].as_str().map(String::from))
        .unwrap_or_default()
}

async fn expect(w: &World, who: AuthContext, p: &TabsPut, kind: &str) {
    let before = get(w, who).await;
    let e = put(w, who, p).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{kind}: {e:?}");
    assert_eq!(kind_of(&e), kind, "{e:?}");
    assert_eq!(get(w, who).await, before, "{kind}: settings unchanged");
}

fn custom(kind: &str, title: &str, extra: Value) -> Value {
    let mut v = json!({"kind": kind, "title": title});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

async fn reset(w: &World, who: AuthContext, lv: i32) -> anyhow::Result<TabSettings> {
    project_tabs::reset(
        &w.app,
        who,
        Uuid::now_v7(),
        "AUTH",
        &TabsReset { lock_version: lv },
    )
    .await
}

#[tokio::test]
async fn s6b_tabs() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_project"]).await;
    let own = role(&w, "Own", "own", &["view_issues"]).await;
    let pm = role(&w, "PM", "default", &["view_issues", "manage_project"]).await;
    let (dave, dave_ctx) = user(&w, "dave", w.auth_p, own).await;
    let (_erin, erin) = user(&w, "erin", w.auth_p, pm).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;

    // ------------------------------------------------ T-PT1: the row comes with the project
    let np = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('NEWP','New') RETURNING id",
    )
    .await;
    let (tabs, d, is_default, lv): (Value, String, bool, i32) = sqlx::query_as(
        "SELECT tabs, default_tab, is_default, lock_version FROM project_tab_settings WHERE project_id = $1",
    )
    .bind(np)
    .fetch_one(&pool)
    .await
    .unwrap();
    let tabs: Vec<Tab> = serde_json::from_value(tabs).unwrap();
    assert_eq!(
        tabs.iter()
            .map(|t| (t.id.as_str(), t.title.as_str()))
            .collect::<Vec<_>>(),
        BUILTIN.to_vec(),
        "the SQL default and project_tabs::BUILTIN agree"
    );
    assert!(tabs.iter().all(|t| t.kind == "builtin"
        && t.hidden == project_tabs::BUILTIN_HIDDEN.contains(&t.id.as_str())));
    assert_eq!((d.as_str(), is_default, lv), ("timeline", true, 0));
    let s = get(&w, a).await;
    assert!(s.is_default && s.can_manage);
    assert_eq!(s.default_tab, "timeline");

    // ------------------------------------------------ T-PT2, T-PT12: built-ins and ids
    let five = as_input(&s.tabs);
    expect(
        &w,
        a,
        &input(&s, five[..4].to_vec(), "timeline"),
        "builtin_missing",
    )
    .await;
    let mut dup = five.clone();
    dup.push(five[0].clone());
    expect(&w, a, &input(&s, dup, "timeline"), "duplicate_id").await;
    let mut unknown = five.clone();
    unknown.push(custom(
        "query",
        "x",
        json!({"id": "q_aaaaaaaaaa", "query": "status:open"}),
    ));
    expect(&w, a, &input(&s, unknown, "timeline"), "unknown_id").await;
    let mut fake = five.clone();
    fake.push(json!({"id": "gantt", "kind": "builtin", "title": "Gantt"}));
    expect(&w, a, &input(&s, fake, "timeline"), "unknown_id").await;

    // ------------------------------------------------ T-PT3: eleven custom tabs
    let mut many = five.clone();
    for n in 0..11 {
        many.push(custom(
            "link",
            &format!("link {n}"),
            json!({"url": format!("https://x.test/{n}")}),
        ));
    }
    expect(&w, a, &input(&s, many, "timeline"), "tab_limit").await;

    // ------------------------------------------------ T-PT4: titles
    for (x, y) in [("本週", "本週 "), ("Bugs", "bugs"), ("時程", "時程")] {
        let mut t = five.clone();
        if x != "時程" {
            t.push(custom("query", x, json!({"query": "status:open"})));
        }
        t.push(custom("query", y, json!({"query": "status:open"})));
        expect(&w, a, &input(&s, t, "timeline"), "tab_title_taken").await;
    }
    for bad in [
        "   ",
        "a\u{7}b",
        "一二三四五六七八九十一二三四五六七八九十一",
    ] {
        let mut t = five.clone();
        t.push(custom("query", bad, json!({"query": "status:open"})));
        expect(&w, a, &input(&s, t, "timeline"), "tab_title_invalid").await;
    }

    // ------------------------------------------------ T-PT5: query scope and syntax
    for q in ["project:WEB status:open", "project:AUTH project:WEB"] {
        let mut t = five.clone();
        t.push(custom("query", "q", json!({"query": q})));
        expect(&w, a, &input(&s, t, "timeline"), "tab_query_scope").await;
    }
    let mut t = five.clone();
    t.push(custom("query", "q", json!({"query": "status:\"open"})));
    expect(&w, a, &input(&s, t, "timeline"), "tab_query_syntax").await;

    // ------------------------------------------------ T-PT7: urls
    for u in [
        "http://x.test/a".to_string(),
        "https://u:p@x.com/".into(),
        "javascript:alert(1)".into(),
        format!("{ORIGIN_SPA}/issue/AUTH-1"),
        format!("https://x.test/{}", "a".repeat(3000)),
    ] {
        let mut t = five.clone();
        t.push(custom("link", "l", json!({"url": u})));
        expect(&w, a, &input(&s, t, "timeline"), "tab_url_invalid").await;
    }

    // ------------------------------------------------ T-PT8: the default tab
    let mut hidden = five.clone();
    hidden[0]["hidden"] = json!(true);
    expect(&w, a, &input(&s, hidden, "timeline"), "tab_default_invalid").await;
    let mut t = five.clone();
    t.push(custom(
        "link",
        "docs",
        json!({"url": "https://docs.example.com/x"}),
    ));
    let s1 = put(&w, a, &input(&s, t, "timeline")).await.unwrap();
    let link_id = s1.tabs.last().unwrap().id.clone();
    assert!(link_id.starts_with("l_") && link_id.len() == 12);
    for bad in [link_id.as_str(), "nope"] {
        let e = put(&w, a, &input(&s1, as_input(&s1.tabs), bad))
            .await
            .unwrap_err();
        assert_eq!(kind_of(&e), "tab_default_invalid");
    }

    // A good PUT: a saved query (project: added), renames, reorder, hide.
    let mut five = as_input(&s1.tabs[..8]);
    five.swap(0, 2);
    five[1]["title"] = json!("  里程碑 2  ");
    five[4]["hidden"] = json!(true);
    let mut t = five.clone();
    t.push(custom(
        "query",
        "Open work",
        json!({"query": "status:open  "}),
    ));
    let s2 = put(&w, a, &input(&s1, t, "issues")).await.unwrap();
    assert_eq!(s2.lock_version, s1.lock_version + 1);
    assert!(!s2.is_default);
    assert_eq!(s2.tabs[0].id, "issues");
    assert_eq!(s2.tabs[1].title, "里程碑 2");
    assert!(s2.tabs[4].hidden);
    let q = s2.tabs.last().unwrap().clone();
    assert!(q.id.starts_with("q_"));
    assert_eq!(q.query.as_deref(), Some("project:AUTH status:open"));
    assert!(
        !s2.tabs.iter().any(|t| t.id == link_id),
        "the link was dropped"
    );
    // T-PT12: a deleted tab's id cannot come back.
    let mut back = as_input(&s2.tabs);
    back.push(custom(
        "link",
        "again",
        json!({"id": link_id, "url": "https://docs.example.com/x"}),
    ));
    let e = put(&w, a, &input(&s2, back, "issues")).await.unwrap_err();
    assert_eq!(kind_of(&e), "unknown_id");

    // ------------------------------------------------ T-PT6: the opener's visibility
    let (_mine, mine_key) = mk(&w, "dave's own", dave, false).await;
    let (_other, _) = mk(&w, "someone else's", w.bob, false).await;
    let r = project_tabs::run_query(&w.app, dave_ctx, "AUTH", &q.id, None)
        .await
        .unwrap();
    let keys: Vec<_> = r.items.iter().map(|i| i.key.clone()).collect();
    assert_eq!(
        keys,
        std::slice::from_ref(&mine_key),
        "dave (own) sees only his own issue"
    );
    let r = project_tabs::run_query(&w.app, a, "AUTH", &q.id, None)
        .await
        .unwrap();
    assert_eq!(r.items.len(), 2);
    let body = serde_json::to_value(&r).unwrap();
    assert!(
        body.get("total").is_none() && body.get("count").is_none(),
        "no totals"
    );

    // ------------------------------------------------ T-PT10: no manage_project
    let ds = get(&w, dave_ctx).await;
    assert!(!ds.can_manage);
    let e = put(&w, dave_ctx, &input(&ds, as_input(&ds.tabs), "issues"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = project_tabs::events(&w.app, dave_ctx, "AUTH", None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // ------------------------------------------------ T-PT14: same key replays
    let key = Uuid::now_v7();
    let mut t = as_input(&s2.tabs);
    t[0]["title"] = json!("Issues");
    let p = input(&s2, t, "issues");
    let r1 = project_tabs::put(&w.app, a, ORIGIN_SPA, key, "AUTH", &p)
        .await
        .unwrap();
    let r2 = project_tabs::put(&w.app, a, ORIGIN_SPA, key, "AUTH", &p)
        .await
        .unwrap();
    assert_eq!(r1, r2);
    assert_eq!(r2.lock_version, s2.lock_version + 1, "lock_version once");
    let n = count(
        &pool,
        "SELECT count(*) FROM project_setting_events WHERE kind = 'tabs_updated'",
    )
    .await;
    assert_eq!(n, 3);

    // ------------------------------------------------ T-PT16: only default_tab changes
    let s3 = put(&w, a, &input(&r2, as_input(&r2.tabs), "timeline"))
        .await
        .unwrap();
    let ev = project_tabs::events(&w.app, a, "AUTH", None).await.unwrap();
    let last = &ev.items[0];
    assert_eq!(last.kind, "tabs_updated");
    assert_eq!(last.payload["before"]["default_tab"], "issues");
    assert_eq!(last.payload["after"]["default_tab"], "timeline");
    assert_eq!(
        last.payload["before"]["tabs"],
        last.payload["after"]["tabs"]
    );

    // ------------------------------------------------ T-PT9: concurrent managers
    // (a) changed before: both from the same lock_version.
    let mut t = as_input(&s3.tabs);
    t[0]["title"] = json!("A");
    put(&w, a, &input(&s3, t.clone(), "timeline"))
        .await
        .unwrap();
    t[0]["title"] = json!("B");
    let e = put(&w, bob, &input(&s3, t, "timeline")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    let cur = oxledger::error::classify(&e).unwrap().details.unwrap();
    assert_eq!(
        cur["current"]["tabs"][0]["title"], "A",
        "conflict carries the latest"
    );
    // (c) one changes, one resets.
    let s4 = get(&w, a).await;
    let mut t = as_input(&s4.tabs);
    t[0]["title"] = json!("C");
    put(&w, a, &input(&s4, t, "timeline")).await.unwrap();
    let e = reset(&w, bob, s4.lock_version).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    let s5 = get(&w, a).await;
    let rs = reset(&w, bob, s5.lock_version).await.unwrap();
    assert!(rs.is_default);
    assert_eq!(rs.default_tab, "timeline");
    assert_eq!(rs.tabs.len(), 8);
    // (b) never changed (NEWP).
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects WHERE key = 'NEWP'",
    )
    .await;
    member(&pool, np, w.alice, w.dev).await;
    member(&pool, np, w.bob, w.dev).await;
    let n0 = project_tabs::get(&w.app, a, "NEWP").await.unwrap();
    assert_eq!(n0.lock_version, 0);
    let mut t = as_input(&n0.tabs);
    t[0]["title"] = json!("X");
    let pa = input(&n0, t, "timeline");
    project_tabs::put(&w.app, a, ORIGIN_SPA, Uuid::now_v7(), "NEWP", &pa)
        .await
        .unwrap();
    let e = project_tabs::put(&w.app, bob, ORIGIN_SPA, Uuid::now_v7(), "NEWP", &pa)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);

    // ------------------------------------------------ T-PT17: PR events stay hidden from managers
    let ev_before = project_tabs::events(&w.app, erin, "AUTH", None)
        .await
        .unwrap();
    sqlx::query(
        "WITH r AS (INSERT INTO operation_requests (caller_id, operation_id, action, fingerprint, status)
                    VALUES ($1, uuidv7(), 'pr_hide', decode(repeat('00', 32), 'hex'), 'completed') RETURNING id)
         INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
         SELECT $2, v.k, $1, r.id, v.p::jsonb FROM r,
           (VALUES ('pr_hidden', '{\"pr_id\": \"x\"}'), ('pr_unhidden', '{\"pr_id\": \"x\"}'),
                   ('github_content_revoked', '{\"pr_ids\": [\"x\"]}')) v(k, p)",
    )
    .bind(w.alice)
    .bind(w.auth_p)
    .execute(&pool)
    .await
    .unwrap();
    let ev_after = project_tabs::events(&w.app, erin, "AUTH", None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&ev_before).unwrap(),
        serde_json::to_value(&ev_after).unwrap(),
        "a manager's history does not change shape"
    );
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let ev_root = project_tabs::events(&w.app, root, "AUTH", None)
        .await
        .unwrap();
    assert_eq!(
        ev_root.items.len(),
        ev_after.items.len() + 3,
        "an administrator sees them"
    );

    // Paging the history (50 per page, newest first).
    for n in 0..55 {
        let s = get(&w, a).await;
        let mut t = as_input(&s.tabs);
        t[0]["title"] = json!(format!("T{n}"));
        put(&w, a, &input(&s, t, "timeline")).await.unwrap();
    }
    let p1 = project_tabs::events(&w.app, erin, "AUTH", None)
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 50);
    let p2 = project_tabs::events(&w.app, erin, "AUTH", p1.next_cursor.as_deref())
        .await
        .unwrap();
    assert!(!p2.items.is_empty());
    assert!(p1.items.last().unwrap().at >= p2.items[0].at);
    assert!(
        !p1.items
            .iter()
            .any(|e| p2.items.iter().any(|x| x.id == e.id))
    );
    assert!(
        p2.items
            .iter()
            .all(|e| !e.kind.starts_with("pr_") && e.kind != "github_content_revoked")
    );
    let e = project_tabs::events(&w.app, erin, "AUTH", Some("junk"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);

    // ------------------------------------------------ T-PT11: people only
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    let (token, agent) = w.token(ctx).await;
    let e = project_tabs::get(&w.app, agent, "AUTH").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::HumanSessionRequired);
    let s = get(&w, a).await;
    let e = put(&w, agent, &input(&s, as_input(&s.tabs), "timeline"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::HumanSessionRequired);
    let r = router(&w);
    let resp = r
        .clone()
        .oneshot(
            Request::get("/api/v1/projects/AUTH/tabs")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "§4.2: a bearer on /api/v1 is unauthenticated"
    );

    // ------------------------------------------------ T-PT13: missing and invisible look the same
    let (_p50, k50) = mk(&w, "private", w.bob, true).await;
    let br = login(&r, "erin").await;
    let mut bodies = Vec::new();
    for key in ["AUTH-999".to_string(), k50.clone()] {
        let resp = r
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/issues/{key}"))
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={}", br.session))
                    .header("x-csrf-token", &br.csrf)
                    .header(header::ORIGIN, ORIGIN_SPA)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{key}");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        // The message echoes the key that was asked for; nothing else differs.
        let text = String::from_utf8(bytes.to_vec())
            .unwrap()
            .replace(&key, "<key>");
        let v: Value = serde_json::from_str(&text).unwrap();
        bodies.push(strip_request_id(&v));
    }
    assert_eq!(bodies[0], bodies[1]);

    // ------------------------------------------------ T-PT15: a closed project
    exec(
        &pool,
        "UPDATE projects SET status = 'closed' WHERE key = 'AUTH'",
    )
    .await;
    let s = get(&w, a).await;
    assert!(!s.can_manage);
    let e = put(&w, a, &input(&s, as_input(&s.tabs), "timeline"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = reset(&w, a, s.lock_version).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
}
