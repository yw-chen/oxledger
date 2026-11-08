// SPDX-License-Identifier: GPL-2.0-or-later
//! A fake GitHub REST API on 127.0.0.1 (plain HTTP) for S6b.8. It checks
//! the App JWT's RS256 signature with the test key's public half, issues
//! installation tokens, pages lists with `Link`, can simulate the rate
//! limit, and replays redelivered webhooks into oxledger's router.

#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use oxledger::github::http::testing;
use oxledger::github::webhook;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const KEY_PEM: &str = include_str!("../fixtures/github-test-app.pem");
pub const APP_ID: i64 = 4242;

/// (suites, runs, statuses) of one commit.
pub type Checks = (Vec<Value>, Vec<Value>, Vec<Value>);

#[derive(Default)]
pub struct MockState {
    pub public_key: Vec<u8>,
    pub base: String,
    /// Installation token lifetime.
    pub token_ttl: i64,
    pub tokens: Vec<(String, DateTime<Utc>)>,
    pub bad_auth: usize,
    pub installations: Vec<Value>,
    pub repos: HashMap<i64, Vec<Value>>,
    /// `(full_name, number)` → PR object.
    pub pulls: BTreeMap<(String, i64), Value>,
    pub commits: HashMap<i64, Vec<String>>,
    pub reviews: HashMap<i64, Vec<Value>>,
    /// sha → (suites, runs, statuses)
    pub checks: HashMap<String, Checks>,
    pub deliveries: Vec<Value>,
    /// delivery id → (event, body)
    pub bodies: HashMap<i64, (String, Vec<u8>)>,
    pub next_delivery: i64,
    pub page_size: usize,
    /// Answer this many more API calls, then 403 with the limit spent.
    pub calls_left: Option<usize>,
    pub reset_at: i64,
    /// Paths containing this answer 500.
    pub fail_path: Option<String>,
    pub calls: HashMap<String, usize>,
    pub hook: Option<Router>,
    pub secret: Vec<u8>,
}

pub type Mock = Arc<Mutex<MockState>>;

pub async fn start() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let m: Mock = Arc::new(Mutex::new(MockState {
        public_key: testing::public_key(KEY_PEM).unwrap(),
        base: format!("http://{addr}"),
        token_ttl: 3600,
        page_size: 100,
        next_delivery: 1,
        ..Default::default()
    }));
    let state = m.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
        let m = state.clone();
        async move {
            let r = respond(&m, method, uri, headers);
            match r {
                Ok(res) => res,
                Err((hook, secret, event, guid, body)) => {
                    deliver_with(&m, &hook, &secret, &event, &guid, body).await;
                    StatusCode::ACCEPTED.into_response()
                }
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    m
}

pub fn base(m: &Mock) -> String {
    m.lock().unwrap().base.clone()
}

pub fn calls(m: &Mock, k: &str) -> usize {
    *m.lock().unwrap().calls.get(k).unwrap_or(&0)
}

fn json_res(status: StatusCode, v: Value, link: Option<String>) -> Response {
    let mut r = (status, axum::Json(v)).into_response();
    if let Some(l) = link {
        r.headers_mut().insert("link", l.parse().unwrap());
    }
    r
}

enum Who {
    App,
    Installation,
}

fn check_auth(m: &mut MockState, h: &HeaderMap, want: Who) -> bool {
    let a = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ok = match want {
        Who::App => a
            .strip_prefix("Bearer ")
            .and_then(|j| testing::verify_jwt(&m.public_key, j))
            .is_some_and(|c| {
                let (iat, exp) = (
                    c["iat"].as_i64().unwrap_or(0),
                    c["exp"].as_i64().unwrap_or(0),
                );
                c["iss"] == json!(APP_ID.to_string())
                    && exp - iat <= 600
                    && iat <= Utc::now().timestamp()
                    && exp > Utc::now().timestamp()
            }),
        Who::Installation => a
            .strip_prefix("token ")
            .is_some_and(|t| m.tokens.iter().any(|(x, exp)| x == t && *exp > Utc::now())),
    };
    if !ok {
        m.bad_auth += 1;
    }
    ok
}

fn page<T: Clone>(m: &MockState, items: &[T], uri: &Uri) -> (Vec<T>, Option<String>) {
    let q: HashMap<String, String> = uri
        .query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let p: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let size = m.page_size;
    let start = (p - 1) * size;
    let chunk: Vec<T> = items.iter().skip(start).take(size).cloned().collect();
    let link = (start + size < items.len()).then(|| {
        let mut q2: Vec<(String, String)> = q.into_iter().filter(|(k, _)| k != "page").collect();
        q2.sort();
        q2.push(("page".into(), (p + 1).to_string()));
        let qs = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(q2)
            .finish();
        format!("<{}{}?{}>; rel=\"next\"", m.base, uri.path(), qs)
    });
    (chunk, link)
}

type Replay = (Router, Vec<u8>, String, String, Vec<u8>);

fn respond(m: &Mock, method: Method, uri: Uri, headers: HeaderMap) -> Result<Response, Replay> {
    let path = uri.path().to_string();
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let key = match (method.as_str(), seg.as_slice()) {
        ("POST", ["app", "installations", _, "access_tokens"]) => "access_tokens",
        ("GET", ["app", "installations"]) => "installations",
        ("GET", ["installation", "repositories"]) => "installation_repos",
        ("GET", ["app", "hook", "deliveries"]) => "deliveries",
        ("POST", ["app", "hook", "deliveries", _, "attempts"]) => "redeliver",
        ("GET", ["repos", _, _, "pulls"]) => "list_pulls",
        ("GET", ["repos", _, _, "pulls", _]) => "get_pull",
        ("GET", ["repos", _, _, "pulls", _, "commits"]) => "commits",
        ("GET", ["repos", _, _, "pulls", _, "reviews"]) => "reviews",
        ("GET", ["repos", _, _, "commits", _, "check-suites"]) => "ref_suites",
        ("GET", ["repos", _, _, "commits", _, "check-runs"]) => "ref_runs",
        ("GET", ["repos", _, _, "commits", _, "statuses"]) => "ref_statuses",
        ("GET", ["repos", _, _, "check-suites", _]) => "get_suite",
        ("GET", ["repos", _, _, "check-runs", _]) => "get_run",
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    let mut g = m.lock().unwrap();
    let g = &mut *g;
    *g.calls.entry(key.to_string()).or_default() += 1;
    if let Some(left) = g.calls_left {
        if left == 0 {
            let mut r = json_res(
                StatusCode::FORBIDDEN,
                json!({"message": "API rate limit exceeded"}),
                None,
            );
            r.headers_mut()
                .insert("x-ratelimit-remaining", "0".parse().unwrap());
            r.headers_mut()
                .insert("x-ratelimit-reset", g.reset_at.to_string().parse().unwrap());
            return Ok(r);
        }
        g.calls_left = Some(left - 1);
    }
    if g.fail_path.as_deref().is_some_and(|f| path.contains(f)) {
        return Ok(json_res(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"message": "boom"}),
            None,
        ));
    }
    let app_level = matches!(
        key,
        "access_tokens" | "installations" | "deliveries" | "redeliver"
    );
    if !check_auth(
        g,
        &headers,
        if app_level {
            Who::App
        } else {
            Who::Installation
        },
    ) {
        return Ok(json_res(
            StatusCode::UNAUTHORIZED,
            json!({"message": "Bad credentials"}),
            None,
        ));
    }
    let repo = |s: &[&str]| format!("{}/{}", s[1], s[2]);
    Ok(match key {
        "access_tokens" => {
            let n = g.tokens.len() + 1;
            let t = format!("ghs_mocktoken{n:04}xyz");
            let exp = Utc::now() + Duration::seconds(g.token_ttl);
            g.tokens.push((t.clone(), exp));
            json_res(
                StatusCode::CREATED,
                json!({"token": t, "expires_at": exp.to_rfc3339()}),
                None,
            )
        }
        "installations" => {
            let v = g.installations.clone();
            let (c, l) = page(g, &v, &uri);
            json_res(StatusCode::OK, json!(c), l)
        }
        "installation_repos" => {
            let v: Vec<Value> = g.repos.values().flatten().cloned().collect();
            let (c, l) = page(g, &v, &uri);
            json_res(
                StatusCode::OK,
                json!({"total_count": v.len(), "repositories": c}),
                l,
            )
        }
        "deliveries" => {
            let mut v = g.deliveries.clone();
            v.reverse();
            let (c, l) = page(g, &v, &uri);
            json_res(StatusCode::OK, json!(c), l)
        }
        "redeliver" => {
            let id: i64 = seg[3].parse().unwrap_or(0);
            let Some((event, body)) = g.bodies.get(&id).cloned() else {
                return Ok(StatusCode::NOT_FOUND.into_response());
            };
            let guid = g
                .deliveries
                .iter()
                .find(|d| d["id"] == json!(id))
                .and_then(|d| d["guid"].as_str())
                .unwrap()
                .to_string();
            return Err((g.hook.clone().unwrap(), g.secret.clone(), event, guid, body));
        }
        "list_pulls" => {
            let q: HashMap<String, String> = uri
                .query()
                .map(|q| {
                    url::form_urlencoded::parse(q.as_bytes())
                        .into_owned()
                        .collect()
                })
                .unwrap_or_default();
            let state = q.get("state").map(String::as_str).unwrap_or("open");
            let r = repo(&seg);
            let mut v: Vec<Value> = g
                .pulls
                .iter()
                .filter(|((n, _), p)| {
                    *n == r
                        && match state {
                            "open" => p["state"] == "open",
                            "closed" => p["state"] == "closed",
                            _ => true,
                        }
                })
                .map(|(_, p)| p.clone())
                .collect();
            v.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
            let (c, l) = page(g, &v, &uri);
            json_res(StatusCode::OK, json!(c), l)
        }
        "get_pull" => {
            let n: i64 = seg[4].parse().unwrap_or(0);
            match g.pulls.get(&(repo(&seg), n)) {
                Some(p) => json_res(StatusCode::OK, p.clone(), None),
                None => json_res(StatusCode::NOT_FOUND, json!({"message": "Not Found"}), None),
            }
        }
        "commits" => {
            let n: i64 = seg[4].parse().unwrap_or(0);
            let v: Vec<Value> = g
                .commits
                .get(&n)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|msg| json!({"commit": {"message": msg}}))
                .collect();
            let (c, l) = page(g, &v, &uri);
            json_res(StatusCode::OK, json!(c), l)
        }
        "reviews" => {
            let n: i64 = seg[4].parse().unwrap_or(0);
            let v = g.reviews.get(&n).cloned().unwrap_or_default();
            let (c, l) = page(g, &v, &uri);
            json_res(StatusCode::OK, json!(c), l)
        }
        "ref_suites" | "ref_runs" | "ref_statuses" => {
            let (s, r, st) = g.checks.get(seg[4]).cloned().unwrap_or_default();
            match key {
                "ref_suites" => {
                    let (c, l) = page(g, &s, &uri);
                    json_res(
                        StatusCode::OK,
                        json!({"total_count": s.len(), "check_suites": c}),
                        l,
                    )
                }
                "ref_runs" => {
                    let (c, l) = page(g, &r, &uri);
                    json_res(
                        StatusCode::OK,
                        json!({"total_count": r.len(), "check_runs": c}),
                        l,
                    )
                }
                _ => {
                    let (c, l) = page(g, &st, &uri);
                    json_res(StatusCode::OK, json!(c), l)
                }
            }
        }
        _ => {
            let id: i64 = seg[4].parse().unwrap_or(0);
            let field = if key == "get_suite" { 0 } else { 1 };
            let found = g.checks.values().find_map(|c| {
                let list = if field == 0 { &c.0 } else { &c.1 };
                list.iter().find(|x| x["id"] == json!(id)).cloned()
            });
            match found {
                Some(v) => json_res(StatusCode::OK, v, None),
                None => json_res(StatusCode::NOT_FOUND, json!({"message": "Not Found"}), None),
            }
        }
    })
}

/// GitHub sends a webhook: posts it to oxledger and records the delivery
/// with the status oxledger answered.
pub async fn deliver_with(
    m: &Mock,
    hook: &Router,
    secret: &[u8],
    event: &str,
    guid: &str,
    body: Vec<u8>,
) -> StatusCode {
    let req = Request::post(webhook::PATH)
        .header("x-github-event", event)
        .header("x-github-delivery", guid)
        .header("x-hub-signature-256", webhook::sign(secret, &body))
        .header("content-type", "application/json")
        .body(Body::from(body.clone()))
        .unwrap();
    let st = hook.clone().oneshot(req).await.unwrap().status();
    let mut g = m.lock().unwrap();
    let id = g.next_delivery;
    g.next_delivery += 1;
    g.deliveries
        .push(json!({"id": id, "guid": guid, "status_code": st.as_u16(),
        "delivered_at": ts(Utc::now()), "event": event}));
    g.bodies.insert(id, (event.to_string(), body));
    st
}

pub async fn deliver(m: &Mock, event: &str, guid: &str, payload: &Value) -> StatusCode {
    let (hook, secret) = {
        let g = m.lock().unwrap();
        (g.hook.clone().unwrap(), g.secret.clone())
    };
    deliver_with(
        m,
        &hook,
        &secret,
        event,
        guid,
        serde_json::to_vec(payload).unwrap(),
    )
    .await
}

pub fn pr(
    n: i64,
    title: &str,
    branch: &str,
    sha: &str,
    state: &str,
    merged: bool,
    updated: DateTime<Utc>,
) -> Value {
    json!({"number": n, "title": title, "head": {"ref": branch, "sha": sha},
           "state": state, "draft": false,
           "merged_at": if merged { json!(ts(updated)) } else { Value::Null },
           "html_url": format!("https://github.com/acme/app/pull/{n}"),
           "updated_at": ts(updated), "requested_reviewers": [], "requested_teams": []})
}

pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
