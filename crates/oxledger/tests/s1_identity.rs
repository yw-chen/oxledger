// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S1 (§3, §9.1): T-A1 … T-A10.
//!
//! HTTP tests drive the real router (`api::router`) with `tower::ServiceExt::oneshot`;
//! agent reads go through `auth::authenticate_bearer` + the same services MCP uses.
//! T-A7's proposal part (existing proposals cannot be applied) arrives with S5.
//!
//! Needs PostgreSQL 18; see `common::test_pool` for which database it uses.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents::{self, DelegationInput};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, SessionPolicy};
use oxledger::custom_fields;
use oxledger::error::{ErrorCode, as_api_error};
use oxledger::mcp::{GetParams, ResolveParams};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App, SearchParams};
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://ledger.test";
const PW: &str = "correct horse battery";

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

async fn id(pool: &PgPool, sql: &str) -> Uuid {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

fn code(e: &anyhow::Error) -> ErrorCode {
    as_api_error(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .code
}

struct World {
    app: Arc<App>,
    router: Router,
    alice: Uuid,
    bob: Uuid,
    group: Uuid,
    auth_p: Uuid,
    own_p: Uuid,
    client: Uuid,
    public_key: String,
    private_key: String,
    secret_field: i32,
}

async fn world(pool: &PgPool) -> World {
    reset(pool).await;
    let status = id(
        pool,
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo') RETURNING id",
    )
    .await;
    let tracker = id(
        pool,
        "INSERT INTO trackers (name) VALUES ('Bug') RETURNING id",
    )
    .await;
    let alice = auth::create_user(pool, "alice", "Alice", PW, false)
        .await
        .unwrap();
    let bob = auth::create_user(pool, "bob", "Bob", PW, false)
        .await
        .unwrap();
    let group = id(
        pool,
        "INSERT INTO principals (kind, name) VALUES ('group','Devs') RETURNING id",
    )
    .await;
    let auth_p = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('AUTH','Auth') RETURNING id",
    )
    .await;
    let own_p = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('OWN','Own') RETURNING id",
    )
    .await;
    exec(
        pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    let dev = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    let reporter = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','own') RETURNING id",
    )
    .await;
    exec(
        pool,
        "INSERT INTO role_permissions (role_id, permission) SELECT id, 'view_issues' FROM roles",
    )
    .await;
    for (project, role) in [(auth_p, dev), (own_p, dev), (own_p, reporter)] {
        let m: Uuid = sqlx::query_scalar(
            "INSERT INTO members (project_id, principal_id) VALUES ($1, $2)
             ON CONFLICT (project_id, principal_id) DO UPDATE SET project_id = EXCLUDED.project_id RETURNING id",
        )
        .bind(project)
        .bind(alice)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
            .bind(m)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
    }
    // OWN also has bob as a Reporter (own): not audience-consistent.
    let m: Uuid = sqlx::query_scalar(
        "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
    )
    .bind(own_p)
    .bind(bob)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(reporter)
        .execute(pool)
        .await
        .unwrap();

    let mut keys = Vec::new();
    for private in [false, true] {
        let (_, key) = create_issue(
            pool,
            NewIssue {
                project_id: auth_p,
                tracker_id: tracker,
                status_id: status,
                subject: if private {
                    "private plan"
                } else {
                    "public bug"
                },
                description: "",
                author_id: bob,
                assigned_to_id: None,
                is_private: private,
                parent_id: None,
            },
        )
        .await
        .unwrap();
        keys.push(key);
    }
    // A restricted field readable by the Dev role (alice can read it, her agent must not).
    let secret_field: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ('Secret', 'string', true, false) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO field_trackers VALUES ($1, $2)")
        .bind(secret_field)
        .bind(tracker)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO field_roles VALUES ($1, $2)")
        .bind(secret_field)
        .bind(dev)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_text) SELECT issue_id, $1, 'salary 9000' FROM issue_keys WHERE key = $2")
        .bind(secret_field)
        .bind(&keys[0])
        .execute(pool)
        .await
        .unwrap();

    let client = agents::create_agent_client(pool, "Claude Code", "policy-2026-01")
        .await
        .unwrap();
    agents::set_agent_client_enabled(pool, "Claude Code", true)
        .await
        .unwrap();

    let app = Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    });
    app.index.sync_all(pool).await.unwrap();
    let router = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    World {
        app,
        router,
        alice,
        bob,
        group,
        auth_p,
        own_p,
        client,
        public_key: keys[0].clone(),
        private_key: keys[1].clone(),
        secret_field,
    }
}

// ---------------------------------------------------------------- HTTP helpers

struct Resp {
    status: StatusCode,
    body: Value,
    set_cookies: Vec<String>,
    cache: Option<String>,
}

impl Resp {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies.iter().find_map(|c| {
            let (kv, _) = c.split_once(';')?;
            let (k, v) = kv.split_once('=')?;
            (k == name && !v.is_empty()).then(|| v.to_string())
        })
    }
}

#[derive(Default, Clone)]
struct Req {
    method: &'static str,
    path: String,
    body: Option<Value>,
    cookies: Vec<(String, String)>,
    csrf: Option<String>,
    origin: Option<&'static str>,
    content_type: Option<&'static str>,
    bearer: Option<String>,
}

impl Req {
    fn get(path: &str) -> Self {
        Req {
            method: "GET",
            path: path.into(),
            ..Default::default()
        }
    }
    fn post(path: &str, body: Value) -> Self {
        Req {
            method: "POST",
            path: path.into(),
            body: Some(body),
            origin: Some(ORIGIN),
            content_type: Some("application/json"),
            ..Default::default()
        }
    }
    fn cookie(mut self, k: &str, v: &str) -> Self {
        self.cookies.push((k.into(), v.into()));
        self
    }
    fn csrf(mut self, t: &str) -> Self {
        self.csrf = Some(t.into());
        self
    }
}

async fn send(router: &Router, r: Req) -> Resp {
    let mut b = Request::builder()
        .method(r.method)
        .uri(format!("/api/v1{}", r.path));
    if !r.cookies.is_empty() {
        let c: Vec<String> = r.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
        b = b.header(header::COOKIE, c.join("; "));
    }
    if let Some(t) = &r.csrf {
        b = b.header("x-csrf-token", t);
    }
    if let Some(o) = r.origin {
        b = b.header(header::ORIGIN, o);
    }
    if let Some(ct) = r.content_type {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    if let Some(t) = &r.bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match &r.body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    let resp = router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let set_cookies = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Resp {
        status,
        body,
        set_cookies,
        cache,
    }
}

/// A signed-in browser: session cookie + CSRF token.
#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
    session_id: Uuid,
}

async fn challenge(router: &Router) -> (String, String) {
    let r = send(router, Req::get("/auth/login-context")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (
        r.cookie(LOGIN_COOKIE).unwrap(),
        r.body["data"]["csrf_token"].as_str().unwrap().to_string(),
    )
}

async fn try_login(router: &Router, login: &str, pw: &str) -> Resp {
    let (c, t) = challenge(router).await;
    send(
        router,
        Req::post("/auth/login", json!({"login": login, "password": pw}))
            .cookie(LOGIN_COOKIE, &c)
            .csrf(&t),
    )
    .await
}

async fn login(router: &Router, login: &str) -> Browser {
    let r = try_login(router, login, PW).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    Browser {
        session: r.cookie(SESSION_COOKIE).expect("session cookie"),
        csrf: r.body["data"]["csrf_token"].as_str().unwrap().into(),
        session_id: r.body["data"]["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    }
}

async fn as_browser(router: &Router, b: &Browser, r: Req) -> Resp {
    send(router, r.cookie(SESSION_COOKIE, &b.session).csrf(&b.csrf)).await
}

#[tokio::test]
async fn s1_identity() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    let r = &w.router;

    // ------------------------------------------------ login basics
    let a = login(r, "alice").await;
    let me = as_browser(r, &a, Req::get("/auth/session")).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["data"]["login"], "alice");
    assert_eq!(
        me.cache.as_deref(),
        Some("no-store"),
        "every /api/v1 response is no-store"
    );
    // Unknown login and wrong password are indistinguishable.
    let wrong = try_login(r, "alice", "wrong password!!").await;
    let unknown = try_login(r, "nobody", "wrong password!!").await;
    assert_eq!(
        (wrong.status, wrong.code()),
        (StatusCode::UNAUTHORIZED, "invalid_credentials")
    );
    assert_eq!(
        (unknown.status, unknown.code()),
        (wrong.status, wrong.code())
    );
    assert_eq!(
        unknown.body["error"]["message"],
        wrong.body["error"]["message"]
    );

    // ------------------------------------------------ T-A3: cookie and bearer together
    let both = send(
        r,
        Req {
            bearer: Some("oxl_x".into()),
            ..Req::get("/auth/session")
        }
        .cookie(SESSION_COOKIE, &a.session),
    )
    .await;
    assert_eq!(
        (both.status, both.code()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    assert_eq!(
        both.body["error"]["details"]["kind"],
        "ambiguous_credentials"
    );

    // ------------------------------------------------ T-A1 (a): bearer on /api/v1 → unauthenticated, everywhere
    for (m, path) in [
        ("POST", "/changesets/0199/approve-and-apply"),
        ("POST", "/changesets/0199/reject"),
        ("POST", "/changesets/0199/revisions"),
        ("POST", "/commands"),
        ("GET", "/auth/session"),
        ("POST", "/delegations"),
    ] {
        let mut q = if m == "GET" {
            Req::get(path)
        } else {
            Req::post(path, json!({}))
        };
        q.bearer = Some("oxa_whatever".into());
        let resp = send(r, q).await;
        assert_eq!(
            (resp.status, resp.code()),
            (StatusCode::UNAUTHORIZED, "unauthenticated"),
            "{m} {path}"
        );
    }

    // ------------------------------------------------ T-A2: unknown fields in bodies
    let (c, t) = challenge(r).await;
    let extra = send(
        r,
        Req::post(
            "/auth/login",
            json!({"login": "alice", "password": PW, "approved": true}),
        )
        .cookie(LOGIN_COOKIE, &c)
        .csrf(&t),
    )
    .await;
    assert_eq!(
        (extra.status, extra.code()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    for field in [
        "origin",
        "approver",
        "on_behalf_of",
        "author_id",
        "approved",
    ] {
        let mut body = json!({"agent_client_id": w.client, "project_id": w.auth_p,
                              "proposal_ops": ["add_note"], "writable_fields": []});
        body[field] = json!(true);
        let resp = as_browser(r, &a, Req::post("/delegations", body)).await;
        assert_eq!(resp.code(), "invalid_request", "{field}: {}", resp.body);
    }

    // ------------------------------------------------ T-A4: CSRF, Origin, Content-Type, login CSRF
    let b = Req::post("/auth/reauthenticate", json!({"password": PW}));
    let no_token = send(r, b.clone().cookie(SESSION_COOKIE, &a.session)).await;
    assert_eq!(no_token.code(), "csrf_failed");
    let bad_token = send(
        r,
        b.clone()
            .cookie(SESSION_COOKIE, &a.session)
            .csrf(&auth::encode(&[7u8; 32])),
    )
    .await;
    assert_eq!(bad_token.code(), "csrf_failed");
    let other_origin = as_browser(
        r,
        &a,
        Req {
            origin: Some("https://evil.test"),
            ..b.clone()
        },
    )
    .await;
    assert_eq!(other_origin.code(), "csrf_failed");
    let no_origin = as_browser(
        r,
        &a,
        Req {
            origin: None,
            ..b.clone()
        },
    )
    .await;
    assert_eq!(no_origin.code(), "csrf_failed");
    let text_plain = as_browser(
        r,
        &a,
        Req {
            content_type: Some("text/plain"),
            ..b.clone()
        },
    )
    .await;
    assert_eq!(text_plain.code(), "csrf_failed");
    assert_eq!(text_plain.status, StatusCode::FORBIDDEN);
    let fine = as_browser(r, &a, b.clone()).await;
    assert_eq!(fine.status, StatusCode::OK, "{}", fine.body);
    // Login CSRF: no challenge, wrong token, reused challenge.
    let no_challenge = send(
        r,
        Req::post("/auth/login", json!({"login": "alice", "password": PW})),
    )
    .await;
    assert_eq!(no_challenge.code(), "csrf_failed");
    let (c, t) = challenge(r).await;
    let (_, t2) = challenge(r).await;
    let mismatched = send(
        r,
        Req::post("/auth/login", json!({"login": "alice", "password": PW}))
            .cookie(LOGIN_COOKIE, &c)
            .csrf(&t2),
    )
    .await;
    assert_eq!(mismatched.code(), "csrf_failed");
    let reused = send(
        r,
        Req::post("/auth/login", json!({"login": "alice", "password": PW}))
            .cookie(LOGIN_COOKIE, &c)
            .csrf(&t),
    )
    .await;
    assert_eq!(
        reused.code(),
        "csrf_failed",
        "a challenge is used up by the first attempt"
    );

    // ------------------------------------------------ T-A5: session fixation
    let before = login(r, "bob").await;
    let (c, t) = challenge(r).await;
    let again = send(
        r,
        Req::post("/auth/login", json!({"login": "bob", "password": PW}))
            .cookie(LOGIN_COOKIE, &c)
            .cookie(SESSION_COOKIE, &before.session)
            .csrf(&t),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    let after = again.cookie(SESSION_COOKIE).unwrap();
    assert_ne!(after, before.session, "login always issues a new token");
    let old = send(
        r,
        Req::get("/auth/session").cookie(SESSION_COOKIE, &before.session),
    )
    .await;
    assert_eq!(
        old.code(),
        "unauthenticated",
        "the pre-login session no longer works"
    );

    // ------------------------------------------------ T-A6: lock, password change, logout, admin/other-session revoke
    let check = |b: Browser| async move {
        send(
            r,
            Req::get("/auth/session").cookie(SESSION_COOKIE, &b.session),
        )
        .await
    };
    let s1 = login(r, "bob").await;
    auth::set_user_status(&pool, w.bob, false).await.unwrap();
    assert_eq!(check(s1.clone()).await.code(), "unauthenticated", "locked");
    assert_eq!(
        try_login(r, "bob", PW).await.code(),
        "invalid_credentials",
        "locked users cannot sign in"
    );
    auth::set_user_status(&pool, w.bob, true).await.unwrap();
    assert_eq!(
        check(s1).await.code(),
        "unauthenticated",
        "unlocking does not revive sessions"
    );

    let s2 = login(r, "bob").await;
    auth::set_password(&pool, w.bob, "another long password")
        .await
        .unwrap();
    assert_eq!(
        check(s2).await.code(),
        "unauthenticated",
        "password changed"
    );
    auth::set_password(&pool, w.bob, PW).await.unwrap();

    let s3 = login(r, "bob").await;
    let out = as_browser(r, &s3, Req::post("/auth/logout", json!({}))).await;
    assert_eq!(out.status, StatusCode::OK, "{}", out.body);
    assert_eq!(check(s3).await.code(), "unauthenticated", "logged out");

    let s4 = login(r, "bob").await;
    let s5 = login(r, "bob").await;
    let list = as_browser(r, &s5, Req::get("/auth/sessions")).await;
    assert!(
        list.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == json!(s4.session_id))
    );
    assert!(
        !list.body.to_string().contains(&s4.session),
        "the list never shows tokens"
    );
    let del = as_browser(
        r,
        &s5,
        Req {
            method: "DELETE",
            ..Req::post(&format!("/auth/sessions/{}", s4.session_id), json!({}))
        },
    )
    .await;
    assert_eq!(del.status, StatusCode::OK, "{}", del.body);
    assert_eq!(
        check(s4).await.code(),
        "unauthenticated",
        "revoked from another session"
    );
    // Someone else's session is not found, not revoked.
    let foreign = as_browser(
        r,
        &s5,
        Req {
            method: "DELETE",
            ..Req::post(&format!("/auth/sessions/{}", a.session_id), json!({}))
        },
    )
    .await;
    assert_eq!(foreign.code(), "not_found");
    assert_eq!(check(a.clone()).await.status, StatusCode::OK);
    // Expired sessions (idle or absolute) stop working.
    let s6 = login(r, "bob").await;
    sqlx::query(
        "UPDATE browser_sessions SET idle_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(s6.session_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(check(s6).await.code(), "unauthenticated", "idle timeout");

    // ------------------------------------------------ T-A8: step-up for delegations and tokens
    let deleg = |project: Uuid| {
        json!({"agent_client_id": w.client, "project_id": project,
                                       "proposal_ops": ["add_note", "update_fields"],
                                       "writable_fields": ["subject"]})
    };
    sqlx::query("UPDATE browser_sessions SET reauthenticated_at = now() - interval '16 minutes' WHERE id = $1")
        .bind(a.session_id)
        .execute(&pool)
        .await
        .unwrap();
    let stale = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(
        (stale.status, stale.code()),
        (StatusCode::UNAUTHORIZED, "reauthentication_required"),
        "{}",
        stale.body
    );
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM delegations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(none, 0, "no effect without step-up");
    let re = as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    assert_eq!(re.status, StatusCode::OK);
    let bad_re = as_browser(
        r,
        &a,
        Req::post(
            "/auth/reauthenticate",
            json!({"password": "nope nope nope"}),
        ),
    )
    .await;
    assert_eq!(bad_re.code(), "invalid_credentials");
    // Not audience-consistent (an "own" role) → refused.
    let own = as_browser(r, &a, Req::post("/delegations", deleg(w.own_p))).await;
    assert_eq!(own.code(), "forbidden_op", "{}", own.body);
    assert_eq!(
        own.body["error"]["details"]["kind"],
        "project_not_audience_consistent"
    );
    let d = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(d.status, StatusCode::OK, "{}", d.body);
    let delegation: Uuid = d.body["data"]["id"].as_str().unwrap().parse().unwrap();
    let context: Uuid = d.body["data"]["context_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let dup = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(dup.body["error"]["details"]["kind"], "already_delegated");

    sqlx::query("UPDATE browser_sessions SET reauthenticated_at = now() - interval '16 minutes' WHERE id = $1")
        .bind(a.session_id)
        .execute(&pool)
        .await
        .unwrap();
    let tok_stale = as_browser(
        r,
        &a,
        Req::post(
            &format!("/agent-contexts/{context}/credentials"),
            json!({"label": "laptop"}),
        ),
    )
    .await;
    assert_eq!(tok_stale.code(), "reauthentication_required");
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let tok = as_browser(
        r,
        &a,
        Req::post(
            &format!("/agent-contexts/{context}/credentials"),
            json!({"label": "laptop"}),
        ),
    )
    .await;
    assert_eq!(tok.status, StatusCode::OK, "{}", tok.body);
    let token = tok.body["data"]["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("oxa_"));

    // ------------------------------------------------ T-A1 (b): service layer refuses non-session contexts
    let agent_ctx = auth::authenticate_bearer(&pool, &token)
        .await
        .unwrap()
        .expect("valid token");
    let legacy_raw = "oxl_legacy_test_key";
    sqlx::query("INSERT INTO api_keys (token_sha256, principal_id) VALUES ($1, $2)")
        .bind(auth::hash_api_key(legacy_raw))
        .bind(w.alice)
        .execute(&pool)
        .await
        .unwrap();
    let legacy_ctx = auth::authenticate_bearer(&pool, legacy_raw)
        .await
        .unwrap()
        .expect("valid key");
    let policy = SessionPolicy::default();
    for ctx in [agent_ctx, legacy_ctx] {
        let e = agents::create_delegation(
            &pool,
            &policy,
            &ctx,
            DelegationInput {
                agent_client_id: w.client,
                project_id: w.auth_p,
                proposal_ops: vec!["add_note".into()],
                writable_fields: vec![],
                max_ops: None,
                expires_in_days: None,
                policies: None,
                read_own_time: false,
                read_own_time_ack: false,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::HumanSessionRequired);
        let e = agents::issue_credential(&pool, &policy, &ctx, context, "x", None)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::HumanSessionRequired);
        let e = agents::revoke_delegation(&pool, &ctx, delegation)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::HumanSessionRequired);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delegations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "zero effect");

    // ------------------------------------------------ T-A7 (reads) and T-A10 (each call re-checks)
    // The agent reads AUTH's non-private issues, without the restricted field.
    let keys =
        |res: service::SearchResult| res.items.into_iter().map(|i| i.key).collect::<Vec<_>>();
    let seen = keys(
        service::search(&w.app, agent_ctx, SearchParams::default())
            .await
            .unwrap(),
    );
    assert_eq!(
        seen,
        vec![w.public_key.clone()],
        "agent sees only the non-private issue"
    );
    let e = service::get(&w.app, agent_ctx, &w.private_key)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let detail = service::get(&w.app, agent_ctx, &w.public_key)
        .await
        .unwrap();
    assert!(
        detail.custom_fields.iter().all(|f| f.name != "Secret"),
        "agent never sees visible = false fields"
    );
    let e = service::search(
        &w.app,
        agent_ctx,
        SearchParams {
            filter: Some(json!({"field": "Secret", "op": "not_null"})),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::InvalidFilter,
        "restricted field is unknown to the agent"
    );
    // Alice herself (legacy key = her full visibility) sees both and the field.
    let alice_view = service::get(&w.app, legacy_ctx, &w.public_key)
        .await
        .unwrap();
    assert!(alice_view.custom_fields.iter().any(|f| f.name == "Secret"));
    assert_eq!(
        keys(
            service::search(&w.app, legacy_ctx, SearchParams::default())
                .await
                .unwrap()
        )
        .len(),
        2
    );

    // Revoke the delegation: the same (fixed) context reads nothing on its next call.
    let rv = as_browser(
        r,
        &a,
        Req::post(&format!("/delegations/{delegation}/revoke"), json!({})),
    )
    .await;
    assert_eq!(rv.status, StatusCode::OK, "{}", rv.body);
    assert!(
        keys(
            service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap()
        )
        .is_empty()
    );
    assert_eq!(
        code(
            &service::get(&w.app, agent_ctx, &w.public_key)
                .await
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    // R keeps the project (monotonic).
    let r_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_context_source_projects WHERE context_id = $1",
    )
    .bind(context)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(r_rows, 1);

    // Delegate again; then expiry.
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let d2 = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(d2.status, StatusCode::OK, "{}", d2.body);
    let d2_id: Uuid = d2.body["data"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        keys(
            service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap()
        )
        .len(),
        1
    );
    sqlx::query("UPDATE delegations SET created_at = now() - interval '2 days', expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(d2_id).execute(&pool).await.unwrap();
    assert!(
        keys(
            service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap()
        )
        .is_empty(),
        "expired"
    );
    sqlx::query("UPDATE delegations SET expires_at = now() + interval '1 day' WHERE id = $1")
        .bind(d2_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        keys(
            service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap()
        )
        .len(),
        1
    );

    // Client disabled → unauthenticated on the next call; enabled again → works.
    agents::set_agent_client_enabled(&pool, "Claude Code", false)
        .await
        .unwrap();
    assert_eq!(
        code(
            &service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
    agents::set_agent_client_enabled(&pool, "Claude Code", true)
        .await
        .unwrap();
    assert!(
        service::search(&w.app, agent_ctx, SearchParams::default())
            .await
            .is_ok()
    );

    // Token rotation: a second token works; revoking the first stops only the first.
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let tok2 = as_browser(
        r,
        &a,
        Req::post(&format!("/agent-contexts/{context}/credentials"), json!({})),
    )
    .await;
    let token2 = tok2.body["data"]["token"].as_str().unwrap().to_string();
    let first_id: Uuid = tok.body["data"]["id"].as_str().unwrap().parse().unwrap();
    let rv = as_browser(
        r,
        &a,
        Req::post(&format!("/agent-credentials/{first_id}/revoke"), json!({})),
    )
    .await;
    assert_eq!(rv.status, StatusCode::OK, "{}", rv.body);
    assert_eq!(
        code(
            &service::search(&w.app, agent_ctx, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
    assert!(
        auth::authenticate_bearer(&pool, &token)
            .await
            .unwrap()
            .is_none()
    );
    let agent2 = auth::authenticate_bearer(&pool, &token2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        keys(
            service::search(&w.app, agent2, SearchParams::default())
                .await
                .unwrap()
        )
        .len(),
        1,
        "rotation keeps the delegation"
    );

    // Delegator locked → the agent stops too.
    auth::set_user_status(&pool, w.alice, false).await.unwrap();
    assert_eq!(
        code(
            &service::search(&w.app, agent2, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
    assert_eq!(
        code(
            &service::search(&w.app, legacy_ctx, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
    auth::set_user_status(&pool, w.alice, true).await.unwrap();
    let a = login(r, "alice").await; // her sessions were revoked by the lock
    // T-X12: locking and unlocking a user bumps every project's epoch; the
    // context stays quarantined even after the user is active again.
    assert_eq!(
        code(
            &service::search(&w.app, agent2, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::ContextUnavailable
    );

    // Legacy key revoked → next call unauthenticated (T-A10 for keys).
    let alice_key = auth::authenticate_bearer(&pool, legacy_raw)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        code(
            &service::search(&w.app, alice_key, SearchParams::default())
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );

    // Close the context ("stop this agent"): tokens and delegations go with it.
    let close = as_browser(
        r,
        &a,
        Req::post(&format!("/agent-contexts/{context}/close"), json!({})),
    )
    .await;
    assert_eq!(close.status, StatusCode::OK, "{}", close.body);
    // T-X20: a token of a closed context says so (kind closed).
    let e = service::search(&w.app, agent2, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    assert_eq!(
        as_api_error(&e).unwrap().details.as_ref().unwrap()["kind"],
        "closed"
    );
    let e = auth::authenticate_bearer(&pool, &token2).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    // A closed context is not silently replaced by a new delegation...
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let after_close = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(
        after_close.code(),
        "context_unavailable",
        "{}",
        after_close.body
    );
    // ...only by an explicit new context with the declaration.
    let no_ack = as_browser(
        r,
        &a,
        Req::post(
            "/agent-contexts",
            json!({"agent_client_id": w.client, "fresh_context_ack": false}),
        ),
    )
    .await;
    assert_eq!(no_ack.code(), "invalid_value");
    // PUB: a public project whose Non member role reads a restricted field.
    let tracker: Uuid = sqlx::query_scalar("SELECT id FROM trackers LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: Uuid = sqlx::query_scalar("SELECT id FROM issue_statuses LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let pub_p = id(
        &pool,
        "INSERT INTO projects (key, name, is_public) VALUES ('PUB','Pub',true) RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects WHERE key = 'PUB'",
    )
    .await;
    let nm = id(&pool, "INSERT INTO roles (name, builtin, issues_visibility) VALUES ('Non member','non_member','default') RETURNING id").await;
    sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, 'view_issues')")
        .bind(nm)
        .execute(&pool)
        .await
        .unwrap();
    let (_, pub_key) = create_issue(
        &pool,
        NewIssue {
            project_id: pub_p,
            tracker_id: tracker,
            status_id: status,
            subject: "public project issue",
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap();
    let nm_field: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ('Budget', 'string', true, false) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO field_trackers VALUES ($1, $2)")
        .bind(nm_field)
        .bind(tracker)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO field_roles VALUES ($1, $2)")
        .bind(nm_field)
        .bind(nm)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_text) SELECT issue_id, $1, 'budget 5' FROM issue_keys WHERE key = $2")
        .bind(nm_field)
        .bind(&pub_key)
        .execute(&pool)
        .await
        .unwrap();
    w.app.index.sync_all(&pool).await.unwrap();
    let nc = as_browser(
        r,
        &a,
        Req::post(
            "/agent-contexts",
            json!({"agent_client_id": w.client, "fresh_context_ack": true}),
        ),
    )
    .await;
    assert_eq!(nc.status, StatusCode::OK, "{}", nc.body);
    let again = as_browser(r, &a, Req::post("/delegations", deleg(w.auth_p))).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    // Another user cannot see or touch alice's delegations.
    let bob = login(r, "bob").await;
    let other: Uuid = again.body["data"]["id"].as_str().unwrap().parse().unwrap();
    let foreign = as_browser(
        r,
        &bob,
        Req::post(&format!("/delegations/{other}/revoke"), json!({})),
    )
    .await;
    assert_eq!(foreign.code(), "not_found");
    let bobs = as_browser(r, &bob, Req::get("/delegations")).await;
    assert_eq!(bobs.body["data"], json!([]));

    // ------------------------------------------------ T-A9: database guards
    let agent_id: Uuid = sqlx::query_scalar("SELECT agent_principal_id FROM agent_principals")
        .fetch_one(&pool)
        .await
        .unwrap();
    let refused = |sql: &'static str, binds: Vec<Uuid>| {
        let pool = pool.clone();
        async move {
            let mut q = sqlx::query(sql);
            for b in binds {
                q = q.bind(b);
            }
            let e = q.execute(&pool).await.expect_err(sql);
            assert_eq!(
                e.as_database_error().and_then(|d| d.code()).as_deref(),
                Some("23514"),
                "{sql}: {e}"
            );
        }
    };
    refused(
        "INSERT INTO members (project_id, principal_id) VALUES ($1, $2)",
        vec![w.auth_p, agent_id],
    )
    .await;
    refused(
        "INSERT INTO group_members (group_id, user_id) VALUES ($1, $2)",
        vec![w.group, agent_id],
    )
    .await;
    refused(
        "INSERT INTO group_members (group_id, user_id) VALUES ($1, $2)",
        vec![w.alice, w.bob],
    )
    .await;
    refused("UPDATE issues SET assigned_to_id = $1", vec![agent_id]).await;
    refused(
        "INSERT INTO api_keys (token_sha256, principal_id) VALUES ('x', $1)",
        vec![agent_id],
    )
    .await;
    refused(
        "UPDATE principals SET kind = 'user' WHERE id = $1",
        vec![agent_id],
    )
    .await;
    refused(
        "UPDATE principals SET kind = 'agent' WHERE id = $1",
        vec![w.bob],
    )
    .await;
    let reviewer: i32 = sqlx::query_scalar("INSERT INTO field_defs (name, ftype, is_for_all) VALUES ('Reviewer','user',true) RETURNING id")
        .fetch_one(&pool).await.unwrap();
    let issue: Uuid = sqlx::query_scalar("SELECT id FROM issues LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let e = sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_ref) VALUES ($1, $2, $3)")
        .bind(issue)
        .bind(reviewer)
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect_err("agent as user field value");
    assert_eq!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("23514")
    );
    sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_ref) VALUES ($1, $2, $3)")
        .bind(issue)
        .bind(reviewer)
        .bind(w.bob)
        .execute(&pool)
        .await
        .expect("a user is fine");

    // ------------------------------------------------ verification follow-ups
    // A restricted (visible = false) field cannot be granted for writing.
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let hidden = as_browser(
        r,
        &a,
        Req::post(
            "/delegations",
            json!({"agent_client_id": w.client, "project_id": w.own_p, "proposal_ops": [],
                   "writable_fields": [format!("cf:{}", w.secret_field)]}),
        ),
    )
    .await;
    assert_eq!(hidden.code(), "invalid_value", "{}", hidden.body);

    // A fresh token for the new context.
    let ctxs = as_browser(r, &a, Req::get("/agent-contexts")).await;
    let live_ctx = ctxs.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["state"] == "active")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let t3 = as_browser(
        r,
        &a,
        Req::post(
            &format!("/agent-contexts/{live_ctx}/credentials"),
            json!({}),
        ),
    )
    .await;
    let token3 = t3.body["data"]["token"].as_str().unwrap().to_string();
    let agent3 = auth::authenticate_bearer(&pool, &token3)
        .await
        .unwrap()
        .unwrap();
    let n_agent = |res: service::SearchResult| res.items.len();
    assert_eq!(
        n_agent(
            service::search(&w.app, agent3, SearchParams::default())
                .await
                .unwrap()
        ),
        1
    );

    // resolve never names agent principals.
    let found = service::resolve(&w.app, agent3, "Claude Code", Some("principal"))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&found)
            .unwrap()
            .find("Claude")
            .is_none(),
        "agent principals are not resolvable: {}",
        serde_json::to_string(&found).unwrap()
    );

    // Restricted field readable via a public project's Non member role: the
    // agent still must not see or filter on it (PUB was set up before the new
    // context: configuration changes after delegating quarantine it).
    as_browser(
        r,
        &a,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    let dp = as_browser(r, &a, Req::post("/delegations", deleg(pub_p))).await;
    assert_eq!(dp.status, StatusCode::OK, "{}", dp.body);
    let pub_detail = service::get(&w.app, agent3, &pub_key).await.unwrap();
    assert!(pub_detail.custom_fields.iter().all(|f| f.name != "Budget"));
    let e = service::search(
        &w.app,
        agent3,
        SearchParams {
            filter: Some(json!({"field": "Budget", "op": "eq", "value": "budget 5"})),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::InvalidFilter,
        "Non member readability is not the agent's"
    );
    // Alice (via a fresh legacy key) does read it.
    sqlx::query("INSERT INTO api_keys (token_sha256, principal_id) VALUES ($1, $2)")
        .bind(auth::hash_api_key("oxl_second"))
        .bind(w.alice)
        .execute(&pool)
        .await
        .unwrap();
    let alice2 = auth::authenticate_bearer(&pool, "oxl_second")
        .await
        .unwrap()
        .unwrap();
    assert!(
        service::get(&w.app, alice2, &pub_key)
            .await
            .unwrap()
            .custom_fields
            .iter()
            .any(|f| f.name == "Budget")
    );

    // The delegator's role on AUTH becomes "own" after delegating: AUTH stops
    // being audience-consistent and the agent reads nothing there.
    assert_eq!(
        n_agent(
            service::search(
                &w.app,
                agent3,
                SearchParams {
                    project: Some("AUTH".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
        ),
        1
    );
    sqlx::query(
        "UPDATE member_roles SET role_id = (SELECT id FROM roles WHERE name = 'Reporter')
         WHERE member_id = (SELECT id FROM members WHERE project_id = $1 AND principal_id = $2)",
    )
    .bind(w.auth_p)
    .bind(w.alice)
    .execute(&pool)
    .await
    .unwrap();
    // The role change bumps AUTH's epoch: the whole context is quarantined,
    // PUB reads included (§3.8 rule 5, M7).
    let e = service::search(&w.app, agent3, SearchParams::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    assert_eq!(
        as_api_error(&e).unwrap().details.as_ref().unwrap()["kind"],
        "quarantined"
    );

    // MCP read parameters reject unknown fields (T-A2 for the existing tools).
    assert!(
        serde_json::from_value::<GetParams>(json!({"key": "AUTH-1", "approved": true})).is_err()
    );
    assert!(
        serde_json::from_value::<ResolveParams>(json!({"text": "x", "on_behalf_of": "bob"}))
            .is_err()
    );
    assert!(serde_json::from_value::<SearchParams>(json!({"q": "x", "author_id": "bob"})).is_err());
    assert!(serde_json::from_value::<GetParams>(json!({"key": "AUTH-1"})).is_ok());

    // /mcp: cookie + bearer is the §4.3 error object; every response is no-store.
    let mcp = |headers: Vec<(&'static str, String)>| {
        let router = r.clone();
        async move {
            let mut b = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream");
            for (k, v) in headers {
                b = b.header(k, v);
            }
            let resp = router
                .oneshot(
                    b.body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let cache = resp
                .headers()
                .get(header::CACHE_CONTROL)
                .map(|v| v.to_str().unwrap().to_string());
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                cache,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let (st, cache, body) = mcp(vec![
        ("authorization", format!("Bearer {token3}")),
        ("cookie", format!("{SESSION_COOKIE}={}", a.session)),
    ])
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(body["error"]["details"]["kind"], "ambiguous_credentials");
    assert_eq!(cache.as_deref(), Some("no-store"));
    let (st, cache, body) = mcp(vec![]).await;
    assert_eq!(
        (st, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("unauthenticated"))
    );
    assert_eq!(cache.as_deref(), Some("no-store"));

    // 405 is an error object too.
    let wrong_method = as_browser(
        r,
        &a,
        Req {
            method: "PUT",
            ..Req::post("/auth/session", json!({}))
        },
    )
    .await;
    assert_eq!(
        wrong_method.code(),
        "invalid_request",
        "{}",
        wrong_method.body
    );
    assert_eq!(
        wrong_method.body["error"]["details"]["kind"],
        "method_not_allowed"
    );
    assert_eq!(wrong_method.cache.as_deref(), Some("no-store"));

    // Step-up is not a password oracle: 5 wrong passwords end the session.
    let victim = login(r, "bob").await;
    for i in 1..5 {
        let resp = as_browser(
            r,
            &victim,
            Req::post(
                "/auth/reauthenticate",
                json!({"password": "guess guess guess"}),
            ),
        )
        .await;
        assert_eq!(resp.code(), "invalid_credentials", "attempt {i}");
    }
    let fifth = as_browser(
        r,
        &victim,
        Req::post(
            "/auth/reauthenticate",
            json!({"password": "guess guess guess"}),
        ),
    )
    .await;
    assert_eq!(fifth.code(), "unauthenticated");
    let after = as_browser(
        r,
        &victim,
        Req::post("/auth/reauthenticate", json!({"password": PW})),
    )
    .await;
    assert_eq!(
        after.code(),
        "unauthenticated",
        "the session is gone, even with the right password"
    );

    // Administrator revokes all of a user's sessions (T-A6).
    let b1 = login(r, "bob").await;
    let b2 = login(r, "bob").await;
    assert!(auth::admin_revoke_sessions(&pool, w.bob).await.unwrap() >= 2);
    assert_eq!(check(b1).await.code(), "unauthenticated");
    assert_eq!(check(b2).await.code(), "unauthenticated");

    // ------------------------------------------------ login rate limit
    for _ in 0..10 {
        try_login(r, "carol-nobody", "bad password 1").await;
    }
    let limited = try_login(r, "carol-nobody", "bad password 1").await;
    assert_eq!(
        (limited.status, limited.code()),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert!(limited.body["error"]["retry_after_ms"].as_u64().unwrap() > 0);

    // Audit trail exists for the security-relevant actions.
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT kind FROM auth_events ORDER BY kind")
            .fetch_all(&pool)
            .await
            .unwrap();
    for k in [
        "agent_token_issued",
        "agent_token_revoked",
        "context_closed",
        "context_created",
        "delegation_created",
        "delegation_revoked",
        "login",
        "password_set",
        "user_locked",
        "sessions_revoked_by_admin",
        "session_revoked",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing audit {k}: {kinds:?}");
    }
    reset(&pool).await;
}
