// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S4 (U39): milestone REST (§17.8), R1–R10 on the version side,
//! stats and health over visible issues only, and the next version (§17.10).
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::commands;
use oxledger::custom_fields;
use oxledger::error::{ErrorCode, classify};
use oxledger::search::SearchIndex;
use oxledger::service::App;
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
                  outbox, outbox_cursors, issue_keys, issues, issue_events, issue_notes, issue_duplicates,
                  issue_field_origins, execution_field_changes, execution_relation_changes,
                  version_events, version_field_origins, execution_version_changes,
                  command_executions, execution_refs, operation_requests,
                  workflow_transitions, workflow_field_rules, project_schedule_settings,
                  project_trackers, member_roles, members, role_permission_trackers, role_permissions,
                  roles, enabled_modules, versions, projects, api_keys, group_members, aliases,
                  principals, trackers, issue_statuses, agent_clients, auth_events, login_challenges CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE org_settings SET issue_group_assignment = false, health_at_risk_days = 7,
                health_at_risk_open_pct = 50, timezone = 'Asia/Taipei'",
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn grant(pool: &PgPool, role: Uuid, perms: &[&str]) {
    for p in perms {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role)
            .bind(p)
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn member(pool: &PgPool, project: Uuid, principal: Uuid, role: Uuid) {
    let m: Uuid = sqlx::query_scalar(
        "INSERT INTO members (project_id, principal_id) VALUES ($1,$2) RETURNING id",
    )
    .bind(project)
    .bind(principal)
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

#[derive(Debug)]
struct Resp {
    status: StatusCode,
    body: Value,
}

impl Resp {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    fn data(&self) -> &Value {
        &self.body["data"]
    }
}

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
}

async fn req(
    r: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    who: &Browser,
    key: Option<Uuid>,
) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", who.session))
        .header("x-csrf-token", &who.csrf)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = key {
        b = b.header("Idempotency-Key", k.to_string());
    }
    let body = body
        .map(|v| Body::from(v.to_string()))
        .unwrap_or_else(Body::empty);
    let resp = r.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn set_cookie(resp: &axum::response::Response, name: &str) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let v = v.to_str().ok()?;
            v.strip_prefix(&format!("{name}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap()
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
    let cookie = set_cookie(&lc, LOGIN_COOKIE);
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
    let session = set_cookie(&resp, SESSION_COOKIE);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

fn k() -> Option<Uuid> {
    Some(Uuid::now_v7())
}

#[tokio::test]
async fn s4_versions() {
    let pool = test_pool().await;
    reset(&pool).await;

    let new = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let done = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Done','done',2) RETURNING id").await;
    let bug = id(&pool, "INSERT INTO trackers (name, position, default_status_id) SELECT 'Bug', 1, id FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let mut u = std::collections::BTreeMap::new();
    for login in ["alice", "bob", "carol", "dan"] {
        u.insert(
            login,
            auth::create_user(&pool, login, login, PW, false)
                .await
                .unwrap(),
        );
    }
    let (alice, bob, dan) = (u["alice"], u["bob"], u["dan"]);
    let web = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('WEB','Web') RETURNING id",
    )
    .await;
    let oth = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OTH','Other') RETURNING id",
    )
    .await;
    let old = id(
        &pool,
        "INSERT INTO projects (key, name, status) VALUES ('OLD','Old','closed') RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO project_trackers SELECT id, '{bug}' FROM projects"),
    )
    .await;
    // alice: manager, sees everything. dan: manager who cannot see private
    // issues of others. bob: reader only. carol: no membership.
    let mgr = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(
        &pool,
        mgr,
        &[
            "view_issues",
            "add_issues",
            "edit_issues",
            "manage_versions",
        ],
    )
    .await;
    let mgr_default = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('MgrD','default') RETURNING id",
    )
    .await;
    grant(
        &pool,
        mgr_default,
        &["view_issues", "edit_issues", "manage_versions"],
    )
    .await;
    let reader = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reader','default') RETURNING id",
    )
    .await;
    grant(&pool, reader, &["view_issues"]).await;
    for p in [web, oth, old] {
        member(&pool, p, alice, mgr).await;
    }
    member(&pool, web, dan, mgr_default).await;
    member(&pool, web, bob, reader).await;

    let app = Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    });
    let router = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let (a, b, c, d) = (
        login(&router, "alice").await,
        login(&router, "bob").await,
        login(&router, "carol").await,
        login(&router, "dan").await,
    );
    let today: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (now() AT TIME ZONE 'Asia/Taipei')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    let day = |n: i64| (today + chrono::Duration::days(n)).to_string();
    let wv = format!("/projects/{web}/versions");

    // ---------------------------------------------- create (§17.8, R8, R9)
    let body = json!({"name": " v2.0 ", "effective_date": day(13), "description": "SSO 第二版"});
    let r = req(&router, "POST", &wv, Some(body.clone()), &a, None).await;
    assert_eq!(
        (r.status, r.body["error"]["details"]["kind"].clone()),
        (StatusCode::BAD_REQUEST, json!("idempotency_key"))
    );
    let key = Uuid::now_v7();
    let r = req(&router, "POST", &wv, Some(body.clone()), &a, Some(key)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v2 = r.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    assert_eq!(r.data()["version"]["name"], json!("v2.0"), "trimmed");
    assert_eq!(r.data()["version"]["lock_version"], json!(0));
    assert_eq!(r.data()["version"]["health"], json!("on_track"));
    assert_eq!(r.data()["changed"], json!(true));
    let again = req(&router, "POST", &wv, Some(body.clone()), &a, Some(key)).await;
    assert_eq!(
        again.data()["execution_id"],
        r.data()["execution_id"],
        "replay"
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM versions").await, 1);
    let other = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "v9"})),
        &a,
        Some(key),
    )
    .await;
    assert_eq!(other.code(), "idempotency_key_reused");
    let r = req(&router, "POST", &wv, Some(json!({"name": "V2.0"})), &a, k()).await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::CONFLICT, "version_name_taken")
    );
    assert_eq!(r.body["error"]["details"]["version_id"], json!(v2));
    // NFC: "é" composed and decomposed are one name.
    let r1 = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "caf\u{e9}"})),
        &a,
        k(),
    )
    .await;
    assert_eq!(r1.status, StatusCode::OK);
    let r2 = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "cafe\u{301}"})),
        &a,
        k(),
    )
    .await;
    assert_eq!(r2.code(), "version_name_taken");
    for (bad, kind) in [
        (json!({"name": "x".repeat(61)}), "invalid_value"),
        (json!({"name": "   "}), "invalid_value"),
        (
            json!({"name": "d", "effective_date": "2200-01-01"}),
            "invalid_value",
        ),
        (
            json!({"name": "d", "effective_date": "2026-1-1"}),
            "invalid_request",
        ),
        (json!({"name": "d", "status": "frozen"}), "invalid_value"),
        (
            json!({"name": "d", "description": "x".repeat(256)}),
            "invalid_value",
        ),
        (json!({"name": "d", "sharing": "tree"}), "invalid_request"),
    ] {
        let r = req(&router, "POST", &wv, Some(bad.clone()), &a, k()).await;
        assert_eq!(r.code(), kind, "{bad}: {}", r.body);
    }
    let r = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "d", "effective_date": "2200-01-01"})),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        r.body["error"]["details"]["kind"],
        json!("date_out_of_range")
    );
    let r = req(&router, "POST", &wv, Some(json!({"name": "b"})), &b, k()).await;
    assert_eq!(
        (r.code(), r.body["error"]["details"]["reason"].clone()),
        ("forbidden_op", json!("permission"))
    );
    let r = req(&router, "POST", &wv, Some(json!({"name": "c"})), &c, k()).await;
    assert_eq!(r.code(), "not_found");
    let r = req(
        &router,
        "POST",
        &format!("/projects/{old}/versions"),
        Some(json!({"name": "o"})),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        r.body["error"]["details"]["reason"],
        json!("project_not_writable")
    );
    let r = req(
        &router,
        "POST",
        "/projects/WEB/versions",
        Some(json!({"name": "v3", "effective_date": day(30)})),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "by project key");
    let v3 = r.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let r = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "Backlog"})),
        &a,
        k(),
    )
    .await;
    let backlog = r.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let oth_v = req(
        &router,
        "POST",
        &format!("/projects/{oth}/versions"),
        Some(json!({"name": "o1"})),
        &a,
        k(),
    )
    .await;
    let oth_v = oth_v.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    // The event and origins of a create.
    let (seq, kind, origin, exec_id): (i64, String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT seq, kind, origin, execution_id FROM version_events WHERE version_id = $1",
    )
    .bind(v2)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (seq, kind.as_str(), origin.as_str()),
        (1, "created", "human_direct")
    );
    assert!(exec_id.is_some());
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM version_field_origins WHERE version_id = '{v2}'")
        )
        .await,
        4
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM execution_version_changes").await,
        0,
        "people only (agents: S6)"
    );

    // ---------------------------------------------- issues in versions; stats over visible issues
    let mk = |author: Uuid, subject: &'static str, private: bool| NewIssue {
        project_id: web,
        tracker_id: bug,
        status_id: new,
        subject,
        description: "",
        author_id: author,
        assigned_to_id: None,
        is_private: private,
        parent_id: None,
    };
    let (i1, _) = create_issue(&pool, mk(alice, "overdue", false))
        .await
        .unwrap();
    let (i2, _) = create_issue(&pool, mk(alice, "late", false)).await.unwrap();
    let (i3, _) = create_issue(&pool, mk(alice, "done", false)).await.unwrap();
    let (secret, _) = create_issue(&pool, mk(alice, "secret", true))
        .await
        .unwrap();
    exec(
        &pool,
        &format!(
        "UPDATE issues SET fixed_version_id = '{v2}' WHERE id IN ('{i1}','{i2}','{i3}','{secret}');
         UPDATE issues SET due_date = '{}' WHERE id = '{i1}';
         UPDATE issues SET due_date = '{}' WHERE id = '{i2}';
         UPDATE issues SET status_id = '{done}' WHERE id = '{i3}';
         UPDATE issues SET due_date = '{}' WHERE id = '{secret}';",
        day(-1), day(20), day(-5)
    ),
    )
    .await;
    let r = req(&router, "GET", &format!("/versions/{v2}"), None, &a, None).await;
    assert_eq!(
        r.data()["stats"],
        json!({"total": 4, "closed": 1, "open": 3, "overdue": 2, "late_for_version": 1, "no_dates": 1})
    );
    assert_eq!(r.data()["health"], json!("at_risk"));
    assert_eq!(
        r.data()["health_reasons"],
        json!(["issue_overdue", "issue_late_for_version"])
    );
    assert_eq!(
        r.data()["health_thresholds"],
        json!({"at_risk_days": 7, "at_risk_open_pct": 50, "source": "org"})
    );
    // bob cannot see the private issue: every number is as if it did not exist.
    let r = req(&router, "GET", &format!("/versions/{v2}"), None, &b, None).await;
    assert_eq!(
        r.data()["stats"],
        json!({"total": 3, "closed": 1, "open": 2, "overdue": 1, "late_for_version": 1, "no_dates": 1})
    );
    let r = req(&router, "GET", &format!("/versions/{v2}"), None, &c, None).await;
    assert_eq!(r.code(), "not_found");
    let r = req(
        &router,
        "GET",
        &format!("/versions/{oth_v}"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r.code(), "not_found", "another project's version");
    // Project override of the thresholds.
    exec(&pool, &format!("UPDATE project_schedule_settings SET inherit_org = false, health_at_risk_days = NULL WHERE project_id = '{web}'")).await;
    let r = req(&router, "GET", &format!("/versions/{v2}"), None, &a, None).await;
    assert_eq!(
        r.data()["health_thresholds"],
        json!({"at_risk_days": null, "at_risk_open_pct": null, "source": "project"})
    );
    exec(
        &pool,
        &format!(
            "UPDATE project_schedule_settings SET inherit_org = true WHERE project_id = '{web}'"
        ),
    )
    .await;

    // ---------------------------------------------- list, next (§17.10)
    let r = req(
        &router,
        "GET",
        &format!("{wv}?include=next"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.data()["today"], json!(today.to_string()));
    let names: Vec<&str> = r.data()["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["v2.0", "v3", "Backlog", "café"],
        "dated first, then by name_key"
    );
    assert!(
        r.data()["versions"][0].get("stats").is_none(),
        "stats only on request"
    );
    assert_eq!(r.data()["next"]["outcome"], json!("resolved"));
    assert_eq!(r.data()["next"]["version"]["id"], json!(v2));
    let r = req(
        &router,
        "GET",
        &format!("{wv}?include=stats,next&status=open"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r.data()["versions"][0]["stats"]["total"], json!(3));
    assert_eq!(
        code_of(
            &req(
                &router,
                "GET",
                &format!("{wv}?status=frozen"),
                None,
                &b,
                None
            )
            .await
        ),
        "invalid_argument"
    );
    assert_eq!(
        code_of(
            &req(
                &router,
                "GET",
                &format!("{wv}?include=timeline"),
                None,
                &b,
                None
            )
            .await
        ),
        "invalid_argument"
    );

    // ---------------------------------------------- patch (§17.8, R6, R7)
    let p = |lv: i64, set: Value| json!({"lock_version": lv, "set": set});
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v2}"),
        Some(p(5, json!({"name": "x"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        (r.code(), r.body["error"]["details"]["lock_version"].clone()),
        ("conflict", json!(0))
    );
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v2}"),
        Some(p(0, json!({"name": "V3"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        (r.code(), r.body["error"]["details"]["version_id"].clone()),
        ("version_name_taken", json!(v3))
    );
    // Same values: no_effect, no event, no bump.
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v2}"),
        Some(p(0, json!({"name": "v2.0", "description": "SSO 第二版"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        (
            r.data()["changed"].clone(),
            r.data()["version"]["lock_version"].clone()
        ),
        (json!(false), json!(0))
    );
    // Moving the date earlier: impact over visible issues (dan cannot see the private one).
    let r = req(
        &router,
        "GET",
        &format!("/versions/{v2}/impact?effective_date={}", day(10)),
        None,
        &d,
        None,
    )
    .await;
    assert_eq!(
        r.data()["impact"],
        json!({"open_issues": 2, "due_after_new_date": 1, "becomes_overdue_version": false})
    );
    let r = req(
        &router,
        "GET",
        &format!("/versions/{v2}/impact?effective_date={}", day(-2)),
        None,
        &a,
        None,
    )
    .await;
    assert_eq!(r.data()["impact"]["becomes_overdue_version"], json!(true));
    let r = req(
        &router,
        "GET",
        &format!("/versions/{v2}/impact?status=closed"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r.body["error"]["details"]["reason"], json!("permission"));
    let lv_issue: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v2}"),
        Some(p(0, json!({"effective_date": day(10), "status": "locked"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.data()["changed"], json!(true));
    assert_eq!(r.data()["version"]["lock_version"], json!(1));
    assert_eq!(
        r.data()["impact"],
        json!({"open_issues": 3, "due_after_new_date": 1, "becomes_overdue_version": false})
    );
    let lv_after: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lv_issue, lv_after, "R6: issues are not touched");
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM version_events WHERE version_id = $1 AND seq = 2")
            .bind(v2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        payload,
        json!({"changes": {"effective_date": {"old": day(13), "new": day(10)}, "status": {"old": "open", "new": "locked"}}})
    );
    // R1 from the issue side: locked → version_not_open; v3 open → fine.
    let cmd = |target: Uuid, version: Uuid, lv: i32| {
        json!({"command_schema_version": 1,
               "ops": [{"op_id": Uuid::now_v7(), "kind": "update_fields", "target": {"type": "issue", "id": target},
                        "set": {"fixed_version_id": version}}],
               "preconditions": [{"target": {"type": "issue", "id": target}, "lock_version": lv}]})
    };
    let (i4, _) = create_issue(&pool, mk(alice, "free", false)).await.unwrap();
    let e = commands::submit(
        &app,
        AuthContext::test_user(alice),
        Uuid::now_v7(),
        &cmd(i4, v2, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::VersionNotOpen);
    let e = commands::submit(
        &app,
        AuthContext::test_user(alice),
        Uuid::now_v7(),
        &cmd(i4, oth_v, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(
        classify(&e).unwrap().code,
        ErrorCode::InvalidValue,
        "another project's version"
    );
    commands::submit(
        &app,
        AuthContext::test_user(alice),
        Uuid::now_v7(),
        &cmd(i4, v3, 0),
    )
    .await
    .unwrap();
    // R7: closing a version with open issues is allowed.
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v3}"),
        Some(p(0, json!({"status": "closed", "effective_date": null}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.data()["impact"]["open_issues"], json!(1));
    assert_eq!(r.data()["version"]["effective_date"], json!(null));
    assert_eq!(r.data()["version"]["health"], json!("closed"));
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v3}"),
        Some(p(1, json!({"sharing": "tree"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.code(), "invalid_request");
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{v3}"),
        Some(p(1, json!({"name": "z"}))),
        &b,
        k(),
    )
    .await;
    assert_eq!(r.code(), "forbidden_op");

    // ---------------------------------------------- next: overdue open / same date / none
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{backlog}"),
        Some(p(0, json!({"effective_date": day(-3)}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = req(
        &router,
        "GET",
        &format!("{wv}?include=next"),
        None,
        &a,
        None,
    )
    .await;
    assert_eq!(
        (
            r.data()["next"]["outcome"].clone(),
            r.data()["next"]["reason"].clone()
        ),
        (json!("ambiguous"), json!("overdue_open_exists"))
    );
    assert_eq!(r.data()["next"]["candidates"][0]["overdue"], json!(true));

    // ---------------------------------------------- delete (R5)
    let r = req(
        &router,
        "DELETE",
        &format!("/versions/{v2}?lock_version=1"),
        None,
        &a,
        k(),
    )
    .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::CONFLICT, "version_in_use")
    );
    let used = r.body["error"]["details"]["issues"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(used, 4, "alice sees all four");
    let r = req(
        &router,
        "DELETE",
        &format!("/versions/{v2}?lock_version=1"),
        None,
        &d,
        k(),
    )
    .await;
    assert_eq!(
        r.body["error"]["details"]["issues"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "only what dan sees"
    );
    // A version used only by an issue dan cannot see vs. an unused one: the same answer.
    let only_secret = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "hidden-use"})),
        &a,
        k(),
    )
    .await;
    let only_secret = only_secret.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let unused = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "unused"})),
        &a,
        k(),
    )
    .await;
    let unused = unused.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    exec(
        &pool,
        &format!("UPDATE issues SET fixed_version_id = '{only_secret}' WHERE id = '{secret}'"),
    )
    .await;
    let r1 = req(
        &router,
        "DELETE",
        &format!("/versions/{only_secret}?lock_version=0"),
        None,
        &d,
        k(),
    )
    .await;
    let r2 = req(
        &router,
        "DELETE",
        &format!("/versions/{unused}?lock_version=0"),
        None,
        &d,
        k(),
    )
    .await;
    assert_eq!(
        (r1.status, r2.status),
        (StatusCode::OK, StatusCode::OK),
        "{} {}",
        r1.body,
        r2.body
    );
    assert_eq!(r1.data()["version"]["deleted"], json!(true));
    assert_eq!(
        r1.data()["version"]["stats"],
        r2.data()["version"]["stats"],
        "the same for dan"
    );
    // The private issue keeps the deleted version (R2), can be cleared, but nobody can choose it anew.
    let fixed: Option<Uuid> =
        sqlx::query_scalar("SELECT fixed_version_id FROM issues WHERE id = $1")
            .bind(secret)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(fixed, Some(only_secret));
    let e = commands::submit(
        &app,
        AuthContext::test_user(alice),
        Uuid::now_v7(),
        &cmd(i2, only_secret, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidValue);
    // Deleted: get still answers (deleted = true); the list and next leave it out; the name is free.
    let r = req(
        &router,
        "GET",
        &format!("/versions/{unused}"),
        None,
        &a,
        None,
    )
    .await;
    assert_eq!(r.data()["deleted"], json!(true));
    let r = req(
        &router,
        "GET",
        &format!("{wv}?status=open,locked,closed"),
        None,
        &a,
        None,
    )
    .await;
    assert!(
        r.data()["versions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["id"] != json!(unused))
    );
    let r = req(
        &router,
        "POST",
        &wv,
        Some(json!({"name": "UNUSED"})),
        &a,
        k(),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "the name of a deleted version is free"
    );
    let r = req(
        &router,
        "PATCH",
        &format!("/versions/{unused}"),
        Some(p(1, json!({"name": "again"}))),
        &a,
        k(),
    )
    .await;
    assert_eq!(r.code(), "not_found", "a deleted version cannot be changed");
    let r = req(
        &router,
        "DELETE",
        &format!("/versions/{unused}?lock_version=1"),
        None,
        &a,
        k(),
    )
    .await;
    assert_eq!(r.code(), "not_found");
    let r = req(&router, "DELETE", &format!("/versions/{v3}"), None, &a, k()).await;
    assert_eq!(r.code(), "invalid_argument", "lock_version is required");

    // ---------------------------------------------- events, paged
    let r = req(
        &router,
        "GET",
        &format!("/versions/{v2}/events?limit=1"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.data()["items"][0]["kind"], json!("created"));
    assert_eq!(r.data()["items"][0]["actor"]["name"], json!("alice"));
    let cur = r.data()["next_cursor"].as_str().unwrap().to_string();
    let r2 = req(
        &router,
        "GET",
        &format!("/versions/{v2}/events?limit=1&cursor={cur}"),
        None,
        &b,
        None,
    )
    .await;
    assert_eq!(r2.data()["items"][0]["seq"], json!(2));
    assert_eq!(r2.data()["next_cursor"], json!(null));
    let r3 = req(
        &router,
        "GET",
        &format!("/versions/{v2}/events?limit=1&cursor={cur}"),
        None,
        &a,
        None,
    )
    .await;
    assert_eq!(
        r3.code(),
        "invalid_cursor",
        "a cursor is bound to who asked"
    );
    let r = req(
        &router,
        "GET",
        &format!("/versions/{v2}/events"),
        None,
        &c,
        None,
    )
    .await;
    assert_eq!(r.code(), "not_found");

    // ---------------------------------------------- concurrency: close vs. assign (T-V4, one order)
    let r = req(&router, "POST", &wv, Some(json!({"name": "race"})), &a, k()).await;
    let race = r.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    // An issue transaction holds the version FOR SHARE; the close waits, then succeeds.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM versions WHERE id = $1 FOR SHARE")
        .bind(race)
        .execute(&mut *holder)
        .await
        .unwrap();
    let closing = {
        let (router, a) = (router.clone(), a.clone());
        tokio::spawn(async move {
            req(
                &router,
                "PATCH",
                &format!("/versions/{race}"),
                Some(json!({"lock_version": 0, "set": {"status": "closed"}})),
                &a,
                k(),
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !closing.is_finished(),
        "the close waits for the issue transaction"
    );
    holder.commit().await.unwrap();
    assert_eq!(closing.await.unwrap().status, StatusCode::OK);
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i4)
        .fetch_one(&pool)
        .await
        .unwrap();
    let e = commands::submit(
        &app,
        AuthContext::test_user(alice),
        Uuid::now_v7(),
        &cmd(i4, race, lv),
    )
    .await
    .unwrap_err();
    assert_eq!(
        classify(&e).unwrap().code,
        ErrorCode::VersionNotOpen,
        "closed first → not open"
    );

    // M15: two changes from the same lock_version: one wins, the other conflicts.
    let r = req(&router, "POST", &wv, Some(json!({"name": "duel"})), &a, k()).await;
    let duel = r.data()["version"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let path = format!("/versions/{duel}");
    // Both requests queue behind a held project row, then race for the version.
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR UPDATE")
        .bind(web)
        .execute(&mut *gate)
        .await
        .unwrap();
    let spawn = |who: Browser, text: &'static str| {
        let (router, path) = (router.clone(), path.clone());
        tokio::spawn(async move {
            req(
                &router,
                "PATCH",
                &path,
                Some(json!({"lock_version": 0, "set": {"description": text}})),
                &who,
                k(),
            )
            .await
        })
    };
    let (hx, hy) = (spawn(a.clone(), "from a"), spawn(d.clone(), "from d"));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    gate.commit().await.unwrap();
    let (x, y) = (hx.await.unwrap(), hy.await.unwrap());
    let mut codes = vec![x.code().to_string(), y.code().to_string()];
    codes.sort();
    assert_eq!(codes, ["", "conflict"], "{} / {}", x.body, y.body);
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
        .bind(duel)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lv, 1, "no lost update");
    // The write result carries the receipt (§6.9), and a replay the same one.
    let key = Uuid::now_v7();
    let w1 = req(
        &router,
        "PATCH",
        &path,
        Some(json!({"lock_version": 1, "set": {"description": "again"}})),
        &a,
        Some(key),
    )
    .await;
    let tok = w1.data()["search"]["consistent_after"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(tok.starts_with("c1."));
    let w2 = req(
        &router,
        "PATCH",
        &path,
        Some(json!({"lock_version": 1, "set": {"description": "again"}})),
        &a,
        Some(key),
    )
    .await;
    assert_eq!(w2.data()["search"]["consistent_after"], json!(tok));
    let _ = bob;
}

fn code_of(r: &Resp) -> &str {
    r.code()
}
