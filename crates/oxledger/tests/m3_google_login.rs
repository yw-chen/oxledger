// SPDX-License-Identifier: GPL-2.0-or-later
//! Google sign-in and opening a new site with it (migration 0042,
//! `oxledger::google`): the round trip over HTTP against a fake token
//! endpoint. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, OIDC_COOKIE, SESSION_COOKIE, SETUP_COOKIE};
use oxledger::auth::{self, NewUser, SessionPolicy};
use oxledger::google::{self, Exchange, Fut, GoogleApi, GoogleSettings};
use oxledger::search::SearchIndex;
use oxledger::service::App;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use world::{PW, count, exec, reset};

const ORIGIN: &str = "https://ledger.test";
const CLIENT: &str = "client-1.apps.googleusercontent.com";

/// Google's token endpoint: answers the claims the test set, after checking
/// what the server sent.
#[derive(Default)]
struct Fake {
    claims: Mutex<Value>,
    /// The `code_challenge` of the round trip in progress.
    challenge: Mutex<String>,
    calls: Mutex<u32>,
}

impl GoogleApi for Fake {
    fn exchange<'a>(&'a self, x: Exchange<'a>) -> Fut<'a, Value> {
        Box::pin(async move {
            *self.calls.lock().unwrap() += 1;
            assert_eq!(x.client_id, CLIENT);
            assert_eq!(x.client_secret, "secret-1");
            assert_eq!(x.code, "code-1");
            assert_eq!(
                x.redirect_uri,
                format!("{ORIGIN}/api/v1/auth/google/callback")
            );
            // PKCE: the verifier belongs to the challenge Google was shown.
            assert_eq!(
                URL_SAFE_NO_PAD.encode(Sha256::digest(x.verifier.as_bytes())),
                *self.challenge.lock().unwrap()
            );
            let c = self.claims.lock().unwrap().clone();
            if c.is_null() {
                anyhow::bail!("token endpoint down");
            }
            Ok(c)
        })
    }
}

fn install(fake: &Arc<Fake>, domains: &[&str], setup: &[&str]) {
    google::install(Some(GoogleSettings {
        client_id: CLIENT.into(),
        client_secret: "secret-1".into(),
        allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
        setup_emails: setup.iter().map(|d| d.to_string()).collect(),
        api: fake.clone(),
    }));
}

struct Resp {
    status: StatusCode,
    location: String,
    /// Cookies set: name → (value, the whole header).
    cookies: HashMap<String, (String, String)>,
    body: Value,
}

impl Resp {
    fn cookie(&self, name: &str) -> String {
        self.cookies
            .get(name)
            .unwrap_or_else(|| panic!("no {name} cookie in {:?}", self.cookies.keys()))
            .0
            .clone()
    }
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

async fn send(
    r: &Router,
    method: &str,
    path: &str,
    cookies: &[(&str, &str)],
    body: Option<Value>,
) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"));
    if !cookies.is_empty() {
        let c: Vec<String> = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
        b = b.header(header::COOKIE, c.join("; "));
    }
    if method != "GET" {
        b = b
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map(|v| Body::from(v.to_string())).unwrap_or_default();
    let resp = r.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let cookies = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| {
            let v = v.to_str().ok()?;
            let (name, rest) = v.split_once('=')?;
            let value = rest.split(';').next().unwrap_or("").to_string();
            Some((name.to_string(), (value, v.to_string())))
        })
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        location,
        cookies,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn param(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

fn claims(sub: &str, email: &str, nonce: &str) -> Value {
    json!({
        "iss": "https://accounts.google.com", "aud": CLIENT, "sub": sub,
        "exp": chrono::Utc::now().timestamp() + 600, "nonce": nonce,
        "email": email, "email_verified": true, "name": "Somebody Chen",
    })
}

/// One round trip as the Google account (`sub`, `email`): start, then the
/// callback with whatever session the browser has.
async fn round_trip(
    r: &Router,
    fake: &Fake,
    start: &str,
    sub: &str,
    email: &str,
    session: Option<&str>,
) -> Resp {
    let cookies: Vec<(&str, &str)> = session.map(|s| (SESSION_COOKIE, s)).into_iter().collect();
    let s = send(r, "GET", start, &cookies, None).await;
    assert_eq!(s.status, StatusCode::FOUND, "{}", s.location);
    assert!(s.location.starts_with(google::AUTH_URL), "{}", s.location);
    *fake.challenge.lock().unwrap() = param(&s.location, "code_challenge");
    *fake.claims.lock().unwrap() = claims(sub, email, &param(&s.location, "nonce"));
    let oidc = s.cookie(OIDC_COOKIE);
    let mut cookies = vec![(OIDC_COOKIE, oidc.as_str())];
    if let Some(s) = session {
        cookies.push((SESSION_COOKIE, s));
    }
    let state = param(&s.location, "state");
    send(
        r,
        "GET",
        &format!("/auth/google/callback?state={state}&code=code-1"),
        &cookies,
        None,
    )
    .await
}

fn refused(resp: &Resp, code: &str) {
    assert_eq!(resp.status, StatusCode::FOUND);
    assert_eq!(
        resp.location,
        format!("{ORIGIN}/login?google_error={code}"),
        "expected {code}"
    );
    assert!(!resp.cookies.contains_key(SESSION_COOKIE));
    assert!(!resp.cookies.contains_key(SETUP_COOKIE));
}

async fn fresh(pool: &PgPool) {
    reset(pool).await;
    exec(
        pool,
        "TRUNCATE wiki_pages, wiki_settings, oidc_states, setup_tickets, user_identities CASCADE;
         UPDATE org_settings SET name = '', timezone = 'Asia/Taipei'",
    )
    .await;
}

fn router(pool: &PgPool) -> Router {
    api::router(
        Arc::new(App {
            pool: pool.clone(),
            index: SearchIndex::open(None).unwrap(),
            wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
        }),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    )
}

#[tokio::test]
async fn google_login_and_site_setup() {
    let pool = test_pool().await;
    fresh(&pool).await;
    let r = router(&pool);
    let fake = Arc::new(Fake::default());

    // ---- off: the sign-in page is told so, the start goes back to it.
    google::install(None);
    let ctx = send(&r, "GET", "/auth/login-context", &[], None).await;
    assert_eq!(ctx.body["data"]["google"], false);
    assert_eq!(ctx.body["data"]["needs_setup"], false);
    let s = send(&r, "GET", "/auth/google/start", &[], None).await;
    refused(&s, "not_configured");

    // ---- on, and the site has no user: it needs setting up.
    install(&fake, &[], &[]);
    let ctx = send(&r, "GET", "/auth/login-context", &[], None).await;
    assert_eq!(ctx.body["data"]["google"], true);
    assert_eq!(ctx.body["data"]["needs_setup"], true);
    assert_eq!(ctx.body["data"]["org_name"], "");

    // The start: Google's URL and a cookie that comes back with the redirect.
    let s = send(&r, "GET", "/auth/google/start?next=/projects", &[], None).await;
    assert_eq!(s.status, StatusCode::FOUND);
    assert_eq!(param(&s.location, "client_id"), CLIENT);
    assert_eq!(param(&s.location, "response_type"), "code");
    assert_eq!(param(&s.location, "scope"), "openid email profile");
    assert_eq!(param(&s.location, "code_challenge_method"), "S256");
    assert_eq!(
        param(&s.location, "redirect_uri"),
        format!("{ORIGIN}/api/v1/auth/google/callback")
    );
    assert!(param(&s.location, "state").len() >= 43);
    assert!(param(&s.location, "nonce").len() >= 43);
    let header = &s.cookies[OIDC_COOKIE].1;
    for part in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(header.contains(part), "{header}");
    }
    // Another browser (no cookie, or another one) cannot finish it, and the
    // state is used up by the attempt.
    let state = param(&s.location, "state");
    let path = format!("/auth/google/callback?state={state}&code=code-1");
    let other = send(&r, "GET", &path, &[(OIDC_COOKIE, "someone-else")], None).await;
    refused(&other, "state");
    let oidc = s.cookie(OIDC_COOKIE);
    let replay = send(&r, "GET", &path, &[(OIDC_COOKIE, &oidc)], None).await;
    refused(&replay, "state");
    assert_eq!(*fake.calls.lock().unwrap(), 0, "no code was traded");
    // No state at all, and cancelling at Google.
    refused(
        &send(&r, "GET", "/auth/google/callback?code=code-1", &[], None).await,
        "state",
    );
    let s = send(&r, "GET", "/auth/google/start", &[], None).await;
    let oidc = s.cookie(OIDC_COOKIE);
    let cancelled = send(
        &r,
        "GET",
        &format!(
            "/auth/google/callback?state={}&error=access_denied",
            param(&s.location, "state")
        ),
        &[(OIDC_COOKIE, &oidc)],
        None,
    )
    .await;
    refused(&cancelled, "cancelled");
    // The cookie is cleared on the way back.
    assert!(cancelled.cookies[OIDC_COOKIE].1.contains("Max-Age=0"));

    // An address Google has not verified opens nothing.
    let s = send(&r, "GET", "/auth/google/start", &[], None).await;
    *fake.challenge.lock().unwrap() = param(&s.location, "code_challenge");
    let mut c = claims(
        "sub-owner",
        "owner@example.com",
        &param(&s.location, "nonce"),
    );
    c["email_verified"] = json!(false);
    *fake.claims.lock().unwrap() = c;
    let oidc = s.cookie(OIDC_COOKIE);
    let path = format!(
        "/auth/google/callback?state={}&code=code-1",
        param(&s.location, "state")
    );
    refused(
        &send(&r, "GET", &path, &[(OIDC_COOKIE, &oidc)], None).await,
        "unverified",
    );
    // The token endpoint failing is not a 500.
    let s = send(&r, "GET", "/auth/google/start", &[], None).await;
    *fake.challenge.lock().unwrap() = param(&s.location, "code_challenge");
    *fake.claims.lock().unwrap() = Value::Null;
    let oidc = s.cookie(OIDC_COOKIE);
    let path = format!(
        "/auth/google/callback?state={}&code=code-1",
        param(&s.location, "state")
    );
    refused(
        &send(&r, "GET", &path, &[(OIDC_COOKIE, &oidc)], None).await,
        "exchange",
    );

    // ---- the first Google sign-in gets the setup form, not a session.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-owner",
        "Owner@Example.com",
        None,
    )
    .await;
    assert_eq!(cb.status, StatusCode::FOUND);
    assert_eq!(cb.location, format!("{ORIGIN}/setup"));
    assert!(!cb.cookies.contains_key(SESSION_COOKIE));
    let header = &cb.cookies[SETUP_COOKIE].1;
    for part in ["Secure", "HttpOnly", "SameSite=Strict"] {
        assert!(header.contains(part), "{header}");
    }
    let ticket = cb.cookie(SETUP_COOKIE);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM principals WHERE kind = 'user'").await,
        0
    );

    let none = send(&r, "GET", "/setup/context", &[], None).await;
    assert_eq!(none.code(), "not_found");
    let sc = send(
        &r,
        "GET",
        "/setup/context",
        &[(SETUP_COOKIE, &ticket)],
        None,
    )
    .await;
    assert_eq!(sc.status, StatusCode::OK);
    assert_eq!(sc.body["data"]["email"], "owner@example.com");
    assert_eq!(sc.body["data"]["login"], "owner");
    assert_eq!(sc.body["data"]["name"], "Somebody Chen");
    assert_eq!(sc.body["data"]["timezone"], "Asia/Taipei");

    let body = |org: &str, tz: &str, login: &str| json!({"org_name": org, "timezone": tz, "login": login, "name": "王小明"});
    for (b, path) in [
        (body("", "Asia/Taipei", "owner"), "org_name"),
        (body("奧克斯", "Mars/Olympus", "owner"), "timezone"),
        (body("奧克斯", "Asia/Taipei", "ow ner"), "login"),
    ] {
        let bad = send(&r, "POST", "/setup", &[(SETUP_COOKIE, &ticket)], Some(b)).await;
        assert_eq!(bad.code(), "invalid_value", "{path}");
        assert_eq!(bad.body["error"]["details"]["path"], path);
    }
    // Without the ticket the form opens nothing.
    let no_ticket = send(
        &r,
        "POST",
        "/setup",
        &[],
        Some(body("奧克斯", "Asia/Tokyo", "owner")),
    )
    .await;
    assert_eq!(no_ticket.code(), "not_found");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM principals WHERE kind = 'user'").await,
        0
    );

    let done = send(
        &r,
        "POST",
        "/setup",
        &[(SETUP_COOKIE, &ticket)],
        Some(body("奧克斯軟體", "Asia/Tokyo", "owner")),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let owner = done.cookie(SESSION_COOKIE);
    assert!(done.cookies[SETUP_COOKIE].1.contains("Max-Age=0"));
    assert!(done.body["data"]["csrf_token"].is_string());
    let (login, name, is_admin, email): (String, String, bool, String) =
        sqlx::query_as("SELECT login, name, is_admin, email FROM principals WHERE kind = 'user'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (login.as_str(), name.as_str(), is_admin),
        ("owner", "王小明", true)
    );
    assert_eq!(email, "owner@example.com");
    let (org, tz): (String, String) = sqlx::query_as("SELECT name, timezone FROM org_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((org.as_str(), tz.as_str()), ("奧克斯軟體", "Asia/Tokyo"));
    // The defaults were loaded (the body did not say otherwise).
    assert!(count(&pool, "SELECT count(*) FROM roles").await >= 3);
    assert!(count(&pool, "SELECT count(*) FROM issue_statuses").await >= 3);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM auth_events WHERE kind = 'site_setup'"
        )
        .await,
        1
    );

    // The owner's session: no password, Google for step-up.
    let me = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &owner)],
        None,
    )
    .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["data"]["login"], "owner");
    assert_eq!(me.body["data"]["is_admin"], true);
    assert_eq!(me.body["data"]["has_password"], false);
    assert_eq!(me.body["data"]["google"], true);

    // It works once: the ticket is gone, the site is no longer new.
    let again = send(
        &r,
        "POST",
        "/setup",
        &[(SETUP_COOKIE, &ticket)],
        Some(body("別的", "Asia/Taipei", "intruder")),
    )
    .await;
    assert_eq!(again.code(), "not_found");
    let sc = send(
        &r,
        "GET",
        "/setup/context",
        &[(SETUP_COOKIE, &ticket)],
        None,
    )
    .await;
    assert_eq!(sc.code(), "not_found");
    let ctx = send(&r, "GET", "/auth/login-context", &[], None).await;
    assert_eq!(ctx.body["data"]["needs_setup"], false);
    assert_eq!(ctx.body["data"]["org_name"], "奧克斯軟體");

    // ---- later sign-ins go by the Google account, even after its address
    // changed; the session the browser had is ended; `next` is honoured.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start?next=/issue/AUTH-1%3Ftab%3Dwiki",
        "sub-owner",
        "renamed@example.com",
        Some(&owner),
    )
    .await;
    assert_eq!(cb.status, StatusCode::FOUND);
    assert_eq!(cb.location, format!("{ORIGIN}/issue/AUTH-1?tab=wiki"));
    let owner2 = cb.cookie(SESSION_COOKIE);
    assert_ne!(owner, owner2);
    let old = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &owner)],
        None,
    )
    .await;
    assert_eq!(old.code(), "unauthenticated");
    // `next` never leaves this origin.
    for next in ["//evil.test/x", "https%3A%2F%2Fevil.test", "/%5Cevil.test"] {
        let cb = round_trip(
            &r,
            &fake,
            &format!("/auth/google/start?next={next}"),
            "sub-owner",
            "owner@example.com",
            None,
        )
        .await;
        assert_eq!(cb.location, format!("{ORIGIN}/"), "{next}");
    }

    // ---- no self sign-up: a Google account nobody made a user for.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-eve",
        "eve@example.com",
        None,
    )
    .await;
    refused(&cb, "no_account");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM principals WHERE kind = 'user'").await,
        1
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM setup_tickets").await, 0);

    // ---- an administrator makes users; Google signs in as them by address.
    let bob = auth::create_user_with(
        &pool,
        NewUser {
            login: "bob",
            name: "Bob",
            password: None,
            is_admin: false,
            email: Some("Bob@Example.com"),
        },
    )
    .await
    .unwrap();
    let alice = auth::create_user_with(
        &pool,
        NewUser {
            login: "alice",
            name: "Alice",
            password: Some(PW),
            is_admin: false,
            email: Some("alice@example.com"),
        },
    )
    .await
    .unwrap();
    // Neither a password nor an address, a bad address, a taken address.
    for (password, email) in [
        (None, None),
        (Some(PW), Some("not an address")),
        (Some(PW), Some("BOB@example.com")),
    ] {
        let e = auth::create_user_with(
            &pool,
            NewUser {
                login: "carol",
                name: "Carol",
                password,
                is_admin: false,
                email,
            },
        )
        .await;
        assert!(e.is_err(), "{password:?} {email:?}");
    }
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-bob",
        "BOB@example.com",
        None,
    )
    .await;
    assert_eq!(cb.location, format!("{ORIGIN}/"));
    let bob_session = cb.cookie(SESSION_COOKIE);
    let me = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &bob_session)],
        None,
    )
    .await;
    assert_eq!(me.body["data"]["login"], "bob");
    assert_eq!(me.body["data"]["is_admin"], false);
    assert_eq!(me.body["data"]["has_password"], false);
    let linked: String =
        sqlx::query_scalar("SELECT subject FROM user_identities WHERE user_id = $1")
            .bind(bob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked, "sub-bob");
    // Another Google account claiming bob's address does not take him over.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-mallory",
        "bob@example.com",
        None,
    )
    .await;
    refused(&cb, "other_account");
    // Alice has both: Google, and her password still works.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-alice",
        "alice@example.com",
        None,
    )
    .await;
    let alice_session = cb.cookie(SESSION_COOKIE);
    let me = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &alice_session)],
        None,
    )
    .await;
    assert_eq!(me.body["data"]["has_password"], true);
    assert_eq!(me.body["data"]["google"], true);

    // Password sign-in: bob has none, and it costs the same as a wrong one.
    let password_login = |login: &'static str, password: &'static str| {
        let r = r.clone();
        async move {
            let lc = send(&r, "GET", "/auth/login-context", &[], None).await;
            let cookie = lc.cookie(api::LOGIN_COOKIE);
            let token = lc.body["data"]["csrf_token"].as_str().unwrap().to_string();
            let req = Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{}={cookie}", api::LOGIN_COOKIE))
                .header("x-csrf-token", token)
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"login": login, "password": password}).to_string(),
                ))
                .unwrap();
            r.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(password_login("bob", "").await, StatusCode::UNAUTHORIZED);
    assert_eq!(password_login("bob", PW).await, StatusCode::UNAUTHORIZED);
    assert_eq!(password_login("alice", PW).await, StatusCode::OK);

    // A locked user does not get in through Google either.
    auth::set_user_status(&pool, bob, false).await.unwrap();
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-bob",
        "bob@example.com",
        None,
    )
    .await;
    refused(&cb, "locked");
    auth::set_user_status(&pool, bob, true).await.unwrap();

    // ---- step-up through Google.
    let stale = "UPDATE browser_sessions SET reauthenticated_at = now() - interval '1 hour'";
    exec(&pool, stale).await;
    let fresh_reauth = "SELECT count(*) FROM browser_sessions
                         WHERE revoked_at IS NULL AND reauthenticated_at > now() - interval '1 minute'";
    // Without a session there is nothing to step up.
    let s = send(&r, "GET", "/auth/google/start?reauth=1", &[], None).await;
    refused(&s, "signed_out");
    // Another person's Google account does not step up alice's session.
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start?reauth=1&next=/agents",
        "sub-bob",
        "bob@example.com",
        Some(&alice_session),
    )
    .await;
    refused(&cb, "other_account");
    assert_eq!(count(&pool, fresh_reauth).await, 0);
    // A step-up started in one session does not finish in another.
    let s = send(
        &r,
        "GET",
        "/auth/google/start?reauth=1",
        &[(SESSION_COOKIE, &alice_session)],
        None,
    )
    .await;
    assert_eq!(param(&s.location, "login_hint"), "alice@example.com");
    *fake.challenge.lock().unwrap() = param(&s.location, "code_challenge");
    *fake.claims.lock().unwrap() = claims(
        "sub-alice",
        "alice@example.com",
        &param(&s.location, "nonce"),
    );
    let oidc = s.cookie(OIDC_COOKIE);
    let crossed = send(
        &r,
        "GET",
        &format!(
            "/auth/google/callback?state={}&code=code-1",
            param(&s.location, "state")
        ),
        &[(OIDC_COOKIE, &oidc), (SESSION_COOKIE, &owner2)],
        None,
    )
    .await;
    refused(&crossed, "signed_out");
    assert_eq!(count(&pool, fresh_reauth).await, 0);
    // Her own account does, for that session only, and no new session is made.
    let sessions = count(&pool, "SELECT count(*) FROM browser_sessions").await;
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start?reauth=1&next=/agents",
        "sub-alice",
        "alice@example.com",
        Some(&alice_session),
    )
    .await;
    assert_eq!(cb.location, format!("{ORIGIN}/agents"));
    assert!(!cb.cookies.contains_key(SESSION_COOKIE));
    assert_eq!(count(&pool, fresh_reauth).await, 1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM browser_sessions").await,
        sessions
    );
    let me = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &alice_session)],
        None,
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);

    // ---- changing a user's address ends the link and the sessions.
    auth::set_email(&pool, alice, Some("alice@corp.example"))
        .await
        .unwrap();
    let me = send(
        &r,
        "GET",
        "/auth/session",
        &[(SESSION_COOKIE, &alice_session)],
        None,
    )
    .await;
    assert_eq!(me.code(), "unauthenticated");
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-alice",
        "alice@example.com",
        None,
    )
    .await;
    refused(&cb, "no_account");
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-alice-2",
        "alice@corp.example",
        None,
    )
    .await;
    assert!(cb.cookies.contains_key(SESSION_COOKIE));

    // ---- only some domains.
    install(&fake, &["corp.example"], &[]);
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-bob",
        "bob@example.com",
        None,
    )
    .await;
    refused(&cb, "domain");
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-alice-2",
        "alice@corp.example",
        None,
    )
    .await;
    assert!(cb.cookies.contains_key(SESSION_COOKIE));

    // ---- a new site that only some addresses may open.
    fresh(&pool).await;
    install(&fake, &[], &["boss@example.com"]);
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-eve",
        "eve@example.com",
        None,
    )
    .await;
    refused(&cb, "setup_not_allowed");
    assert_eq!(count(&pool, "SELECT count(*) FROM setup_tickets").await, 0);
    let cb = round_trip(
        &r,
        &fake,
        "/auth/google/start",
        "sub-boss",
        "boss@example.com",
        None,
    )
    .await;
    assert_eq!(cb.location, format!("{ORIGIN}/setup"));
    // A site someone set up from the command line in the meantime is not new.
    let ticket = cb.cookie(SETUP_COOKIE);
    auth::create_user(&pool, "root", "Root", PW, true)
        .await
        .unwrap();
    let late = send(
        &r,
        "POST",
        "/setup",
        &[(SETUP_COOKIE, &ticket)],
        Some(
            json!({"org_name": "晚了", "timezone": "Asia/Taipei", "login": "boss", "name": "Boss",
                    "load_defaults": false}),
        ),
    )
    .await;
    assert_eq!(late.code(), "not_found");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM principals WHERE kind = 'user'").await,
        1
    );

    // ---- the sweeper removes what expired.
    send(&r, "GET", "/auth/google/start", &[], None).await;
    exec(
        &pool,
        "UPDATE oidc_states SET expires_at = now() - interval '1 second';
                 UPDATE setup_tickets SET expires_at = now() - interval '1 second'",
    )
    .await;
    google::sweep(&pool).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM oidc_states").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM setup_tickets").await, 0);

    google::install(None);
}
