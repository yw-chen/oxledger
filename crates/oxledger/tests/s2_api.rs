// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S2 (§4): `/api/v1` skeleton — REST search/get/resolve over the
//! shared services, opaque cursors, the error shape of `not_found`, the
//! registry, and the generated OpenAPI / TypeScript files.
//!
//! The DB test needs PostgreSQL 18; see `common::test_pool`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, SessionPolicy};
use oxledger::custom_fields;
use oxledger::mcp::OxServer;
use oxledger::search::SearchIndex;
use oxledger::service::App;
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::{openapi, registry};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://ledger.test";
const PW: &str = "correct horse battery";

// ---------------------------------------------------------------- no DB

fn api_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../api")
}

/// The committed `api/openapi.json` and `api/types.ts` match the code.
/// Regenerate with `oxledger api-spec` (or OXLEDGER_UPDATE_API_SPEC=1 cargo test).
#[test]
fn api_spec_is_current() {
    let doc = openapi::document();
    let json = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    let ts = openapi::typescript(&doc);
    if std::env::var_os("OXLEDGER_UPDATE_API_SPEC").is_some() {
        std::fs::create_dir_all(api_dir()).unwrap();
        std::fs::write(api_dir().join("openapi.json"), &json).unwrap();
        std::fs::write(api_dir().join("types.ts"), &ts).unwrap();
    }
    let norm = |s: String| s.replace("\r\n", "\n");
    let on_disk = |f: &str| norm(std::fs::read_to_string(api_dir().join(f)).unwrap_or_default());
    assert!(
        on_disk("openapi.json") == json,
        "api/openapi.json is stale: run `oxledger api-spec`"
    );
    assert!(
        on_disk("types.ts") == ts,
        "api/types.ts is stale: run `oxledger api-spec`"
    );
    // Spot checks on the generated contract.
    assert_eq!(doc["openapi"], "3.1.0");
    assert!(doc["paths"]["/issues/search"]["post"]["requestBody"].is_object());
    assert!(doc["paths"]["/issues/{key}"]["get"]["parameters"][0]["name"] == "key");
    assert!(ts.contains("export type ErrorCode ="));
    assert!(ts.contains("\"invalid_cursor\""));
    assert!(ts.contains("\"POST /issues/search\": { body: SearchParams; data: SearchResult }"));
}

/// MCP exposes exactly the registry's read/propose tools (§4.6).
#[test]
fn registry_matches_mcp() {
    let mut want: Vec<String> = registry::mcp_tools()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    want.sort();
    assert_eq!(OxServer::tool_names(), want);
    for t in registry::tools() {
        assert!(
            !t.name.contains('.'),
            "MCP names use underscores: {}",
            t.name
        );
        assert!(
            !t.entrypoints.mcp()
                || matches!(
                    t.effect,
                    registry::EffectClass::Read | registry::EffectClass::Propose
                ),
            "{} must not be on MCP",
            t.name
        );
    }
}

// ---------------------------------------------------------------- DB

struct Resp {
    status: StatusCode,
    body: Value,
}

impl Resp {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
}

async fn call(
    r: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    who: Option<&Browser>,
    extra: &[(&str, String)],
) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"));
    if let Some(w) = who {
        b = b
            .header(header::COOKIE, format!("{SESSION_COOKIE}={}", w.session))
            .header("x-csrf-token", &w.csrf);
    }
    if method != "GET" {
        b = b
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json");
    }
    for (k, v) in extra {
        b = b.header(*k, v);
    }
    let body = body.map(|v| Body::from(v.to_string())).unwrap_or_default();
    let resp = r.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
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
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{LOGIN_COOKIE}="))
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
                .header(header::ORIGIN, ORIGIN)
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
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{SESSION_COOKIE}="))
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

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, workflow_transitions, project_trackers,
                  member_roles, members, role_permission_trackers, role_permissions, roles, enabled_modules,
                  projects, api_keys, group_members, aliases, principals, trackers, issue_statuses,
                  agent_clients, auth_events, login_challenges CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    custom_fields::reconcile_indexes(pool).await.unwrap();
}

async fn one(pool: &PgPool, sql: &str) -> Uuid {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn s2_api() {
    let pool = test_pool().await;
    reset(&pool).await;
    let status = one(
        &pool,
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo') RETURNING id",
    )
    .await;
    let tracker = one(
        &pool,
        "INSERT INTO trackers (name) VALUES ('Bug') RETURNING id",
    )
    .await;
    let alice = auth::create_user(&pool, "alice", "Alice", PW, false)
        .await
        .unwrap();
    let bob = auth::create_user(&pool, "bob", "Bob", PW, false)
        .await
        .unwrap();
    let p = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('PAGE','Paging') RETURNING id",
    )
    .await;
    let q = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('BOBS','Bob only') RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects")
        .execute(&pool)
        .await
        .unwrap();
    let dev = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, 'view_issues')")
        .bind(dev)
        .execute(&pool)
        .await
        .unwrap();
    for (project, user) in [(p, alice), (q, bob)] {
        let m: Uuid = sqlx::query_scalar(
            "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(project)
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
            .bind(m)
            .bind(dev)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut page_keys = Vec::new();
    for i in 0..6 {
        let (_, key) = create_issue(
            &pool,
            NewIssue {
                project_id: p,
                tracker_id: tracker,
                status_id: status,
                subject: &format!("widget problem {i}"),
                description: "",
                author_id: alice,
                assigned_to_id: None,
                is_private: i == 5,
                parent_id: None,
            },
        )
        .await
        .unwrap();
        page_keys.push(key);
    }
    let (_, bobs_key) = create_issue(
        &pool,
        NewIssue {
            project_id: q,
            tracker_id: tracker,
            status_id: status,
            subject: "widget secret plan",
            description: "",
            author_id: bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap();
    let app = Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    });
    app.index.sync(&pool).await.unwrap();
    let r = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let a = login(&r, "alice").await;
    let b = login(&r, "bob").await;

    // ---------------------------------------------- every documented route is routed
    // A throwaway session: the sweep includes logout.
    let sweeper = login(&r, "alice").await;
    for d in openapi::routes() {
        let path = d
            .path
            .replace("{id}", &Uuid::nil().to_string())
            .replace("{key}", "NOPE-1");
        let body = (d.method != "GET" && d.method != "DELETE").then(|| json!({}));
        let resp = call(&r, d.method, &path, body, Some(&sweeper), &[]).await;
        assert_ne!(
            resp.body["error"]["message"], "no such endpoint",
            "{} {} is documented but not routed",
            d.method, d.path
        );
        assert_ne!(
            resp.body["error"]["details"]["kind"], "method_not_allowed",
            "{} {} is documented with the wrong method",
            d.method, d.path
        );
    }

    // ---------------------------------------------- REST search, keyset paging
    let search = |body: Value, who: Browser| {
        let r = r.clone();
        async move { call(&r, "POST", "/issues/search", Some(body), Some(&who), &[]).await }
    };
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut body = json!({"project": "PAGE", "limit": 2});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = search(body, a.clone()).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
        assert!(resp.body["meta"]["request_id"].is_string());
        for it in resp.body["data"]["items"].as_array().unwrap() {
            seen.push(it["key"].as_str().unwrap().to_string());
        }
        pages += 1;
        match resp.body["data"]["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
        assert!(pages < 10);
    }
    assert_eq!(pages, 3);
    let unique: HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), 6, "no duplicates, nothing missed: {seen:?}");
    let mut expected = page_keys.clone();
    expected.reverse(); // newest first
    assert_eq!(seen, expected);

    // Ties on updated_at: the id decides, nothing repeats or goes missing.
    sqlx::query("UPDATE issues SET updated_at = '2026-01-01T00:00:00Z' WHERE project_id = $1")
        .bind(p)
        .execute(&pool)
        .await
        .unwrap();
    let mut tied = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({"project": "PAGE", "limit": 4});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = search(body, a.clone()).await;
        for it in resp.body["data"]["items"].as_array().unwrap() {
            tied.push(it["key"].as_str().unwrap().to_string());
        }
        match resp.body["data"]["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    let mut by_id: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT i.id, k.key FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current WHERE i.project_id = $1",
    )
    .bind(p)
    .fetch_all(&pool)
    .await
    .unwrap();
    by_id.sort_by_key(|x| std::cmp::Reverse(x.0)); // id DESC
    assert_eq!(
        tied,
        by_id.into_iter().map(|x| x.1).collect::<Vec<_>>(),
        "ties ordered by id desc, no gaps"
    );

    // Responses carry every field the generated schema calls required.
    let spec = openapi::document();
    let required_of = |schema_name: &str| -> Vec<String> {
        spec["components"]["schemas"][schema_name]["required"]
            .as_array()
            .map(|r| r.iter().map(|x| x.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    };
    let check = |data: &Value, schema_name: &str| {
        for k in required_of(schema_name) {
            assert!(
                data.get(&k).is_some(),
                "{schema_name}.{k} is required in the schema but missing: {data}"
            );
        }
    };
    let sr = search(json!({"project": "PAGE"}), a.clone()).await;
    check(&sr.body["data"], "SearchResult");
    check(&sr.body["data"]["items"][0], "IssueBrief");
    let rr = call(
        &r,
        "POST",
        "/resolve",
        Some(json!({"text": "zzz-nothing"})),
        Some(&a),
        &[],
    )
    .await;
    check(&rr.body["data"], "ResolveResult");
    let gr = call(
        &r,
        "GET",
        &format!("/issues/{}", page_keys[0]),
        None,
        Some(&a),
        &[],
    )
    .await;
    check(&gr.body["data"], "IssueDetail");
    let sd = call(&r, "GET", "/auth/session", None, Some(&a), &[]).await;
    check(&sd.body["data"], "SessionData");
    let er = call(&r, "GET", "/issues/NOPE-1", None, Some(&a), &[]).await;
    check(&er.body["error"], "ErrorBody");

    // ---------------------------------------------- cursor: opaque, bound, tamper-proof
    let first = search(json!({"project": "PAGE", "limit": 2}), a.clone()).await;
    let c = first.body["data"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let raw = URL_SAFE_NO_PAD.decode(&c).unwrap();
    let issue_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM issues")
        .fetch_all(&pool)
        .await
        .unwrap();
    for id in issue_ids {
        assert!(!c.contains(&id.to_string()));
        assert!(
            !raw.windows(16).any(|w| w == id.as_bytes()),
            "cursor leaks an id"
        );
    }
    assert!(
        !String::from_utf8_lossy(&raw).contains("updated"),
        "cursor is encrypted, not encoded"
    );
    let mut tampered = raw.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 1;
    let bad_cursors = [
        URL_SAFE_NO_PAD.encode(&tampered),
        "2".to_string(), // the old offset format
        "not base64 !!".to_string(),
        URL_SAFE_NO_PAD.encode([0u8; 60]),
        String::new(),
    ];
    for bad in &bad_cursors {
        let resp = search(
            json!({"project": "PAGE", "limit": 2, "cursor": bad}),
            a.clone(),
        )
        .await;
        assert_eq!(
            (resp.status, resp.code()),
            (StatusCode::BAD_REQUEST, "invalid_cursor"),
            "{bad:?}"
        );
    }
    // Another query, another person: the same cursor is refused.
    let other_query = search(
        json!({"project": "PAGE", "limit": 2, "status_category": "open", "cursor": c}),
        a.clone(),
    )
    .await;
    assert_eq!(other_query.code(), "invalid_cursor");
    let other_person = search(
        json!({"project": "PAGE", "limit": 2, "cursor": c}),
        b.clone(),
    )
    .await;
    assert_eq!(other_person.code(), "invalid_cursor");
    // A different page size is fine (limit is not part of the query identity).
    let bigger = search(
        json!({"project": "PAGE", "limit": 3, "cursor": c}),
        a.clone(),
    )
    .await;
    assert_eq!(bigger.status, StatusCode::OK, "{}", bigger.body);
    assert_eq!(bigger.body["data"]["items"].as_array().unwrap().len(), 3);

    // Ranked full text pages too, and its cursor is not a keyset cursor.
    let ranked = search(json!({"q": "widget", "limit": 4}), a.clone()).await;
    assert_eq!(ranked.status, StatusCode::OK, "{}", ranked.body);
    assert_eq!(ranked.body["data"]["items"].as_array().unwrap().len(), 4);
    let rc = ranked.body["data"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let rest = search(json!({"q": "widget", "limit": 4, "cursor": rc}), a.clone()).await;
    let rest_items = rest.body["data"]["items"].as_array().unwrap();
    assert_eq!(rest_items.len(), 2, "6 visible widget issues in total");
    assert!(
        rest_items.iter().all(|i| i["key"] != bobs_key),
        "bob's project stays hidden"
    );
    let mixed = search(json!({"limit": 4, "cursor": rc}), a.clone()).await;
    assert_eq!(mixed.code(), "invalid_cursor");

    // ---------------------------------------------- not_found has one shape
    let invisible = call(
        &r,
        "GET",
        &format!("/issues/{bobs_key}"),
        None,
        Some(&a),
        &[],
    )
    .await;
    let missing = call(&r, "GET", "/issues/NOPE-999", None, Some(&a), &[]).await;
    let mangled = call(&r, "GET", "/issues/%00%27", None, Some(&a), &[]).await;
    for x in [&invisible, &missing, &mangled] {
        assert_eq!(
            (x.status, x.code()),
            (StatusCode::NOT_FOUND, "not_found"),
            "{}",
            x.body
        );
    }
    let shape = |x: &Resp| {
        let mut e = x.body["error"].clone();
        let o = e.as_object_mut().unwrap();
        o.remove("request_id");
        let msg = o.remove("message").unwrap();
        let mut keys: Vec<String> = o.keys().cloned().collect();
        keys.sort();
        (
            keys,
            e,
            msg.as_str()
                .unwrap()
                .split(' ')
                .take(2)
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    assert_eq!(
        shape(&invisible),
        shape(&missing),
        "invisible and missing look the same"
    );
    assert_eq!(
        invisible.body["error"]["message"],
        format!("no issue {bobs_key}")
    );
    // Bob does see it.
    let bobs = call(
        &r,
        "GET",
        &format!("/issues/{bobs_key}"),
        None,
        Some(&b),
        &[],
    )
    .await;
    assert_eq!(bobs.status, StatusCode::OK);
    assert_eq!(bobs.body["data"]["key"], bobs_key.as_str());
    let mine = call(
        &r,
        "GET",
        &format!("/issues/{}", page_keys[0].to_lowercase()),
        None,
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(
        mine.body["data"]["key"],
        page_keys[0].as_str(),
        "keys are case-insensitive"
    );

    // ---------------------------------------------- resolve
    let res = call(
        &r,
        "POST",
        "/resolve",
        Some(json!({"text": "PAGE", "kind": "project"})),
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["data"]["candidates"][0]["name"], "Paging");
    let hidden = call(
        &r,
        "POST",
        "/resolve",
        Some(json!({"text": "BOBS"})),
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(
        hidden.body["data"]["candidates"],
        json!([]),
        "invisible projects do not resolve"
    );
    let extra = call(
        &r,
        "POST",
        "/resolve",
        Some(json!({"text": "x", "approved": true})),
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(extra.code(), "invalid_request");
    let bad_kind = call(
        &r,
        "POST",
        "/resolve",
        Some(json!({"text": "x", "kind": "issue"})),
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(
        (bad_kind.status, bad_kind.code()),
        (StatusCode::BAD_REQUEST, "invalid_argument")
    );

    // ---------------------------------------------- the API rules hold for the new routes
    let bearer = call(
        &r,
        "POST",
        "/issues/search",
        Some(json!({})),
        None,
        &[("authorization", "Bearer oxa_x".into())],
    )
    .await;
    assert_eq!(bearer.code(), "unauthenticated");
    let anon = call(&r, "POST", "/issues/search", Some(json!({})), None, &[]).await;
    assert_eq!(anon.code(), "unauthenticated", "no session");
    let no_csrf = call(
        &r,
        "POST",
        "/issues/search",
        Some(json!({})),
        None,
        &[("cookie", format!("{SESSION_COOKIE}={}", a.session))],
    )
    .await;
    assert_eq!(no_csrf.code(), "csrf_failed");
    let anon_get = call(
        &r,
        "GET",
        &format!("/issues/{}", page_keys[0]),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(anon_get.code(), "unauthenticated");
    let unknown = call(
        &r,
        "POST",
        "/issues/search",
        Some(json!({"author_id": "x"})),
        Some(&a),
        &[],
    )
    .await;
    assert_eq!(unknown.code(), "invalid_request");

    reset(&pool).await;
}
