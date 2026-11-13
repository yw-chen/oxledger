// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S3: permission evaluator (per-permission trackers), workflow
//! transitions and field rules, the assignee function (T-P10), both kinds of
//! describe, and migration 0007 (versions, date CHECKs and the date repair).
//!
//! T-P1 and T-P3 are unit tests in `ox-core::workflow`; here they are checked
//! end to end through describe. Needs PostgreSQL 18; see `common::test_pool`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use ox_core::filter::FieldType;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::custom_fields::{self, NewField};
use oxledger::describe::{self, DescribeParams, DescribeScope, IssueActions, TrackerSchema};
use oxledger::error::{ErrorCode, classify};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App};
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::{agents, db, perm};
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
                  outbox, outbox_cursors, issue_keys, issues, workflow_transitions, workflow_field_rules,
                  project_trackers, member_roles, members, role_permission_trackers, role_permissions,
                  roles, enabled_modules, versions, projects, api_keys, group_members, aliases,
                  principals, trackers, issue_statuses, agent_clients, auth_events, login_challenges CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE org_settings SET issue_group_assignment = false")
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

fn code(e: &anyhow::Error) -> ErrorCode {
    classify(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .code
}

fn msg(e: &anyhow::Error) -> String {
    classify(e).map(|a| a.message).unwrap_or_default()
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

async fn transition(pool: &PgPool, tracker: Uuid, role: Uuid, from: Uuid, to: Uuid, cond: &str) {
    sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,$5)")
        .bind(tracker)
        .bind(role)
        .bind(from)
        .bind(to)
        .bind(cond)
        .execute(pool)
        .await
        .unwrap();
}

async fn rule(pool: &PgPool, tracker: Uuid, role: Uuid, status: Uuid, key: &str, r: &str) {
    sqlx::query("INSERT INTO workflow_field_rules VALUES ($1,$2,$3,$4,$5)")
        .bind(tracker)
        .bind(role)
        .bind(status)
        .bind(key)
        .bind(r)
        .execute(pool)
        .await
        .unwrap();
}

fn field<'a>(s: &'a TrackerSchema, key: &str) -> Option<&'a describe::FieldSchema> {
    s.fields.iter().find(|f| f.key == key)
}

fn action<'a>(a: &'a IssueActions, key: &str) -> Option<&'a describe::FieldAction> {
    a.fields.iter().find(|f| f.key == key)
}

fn targets(a: &IssueActions) -> Vec<String> {
    a.transitions.iter().map(|t| t.name.clone()).collect()
}

// ---------------------------------------------------------------- REST helpers

struct Resp {
    status: StatusCode,
    body: Value,
}

#[derive(Clone)]
struct Browser {
    session: String,
    csrf: String,
}

async fn call(r: &Router, method: &str, path: &str, body: Option<Value>, who: &Browser) -> Resp {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::COOKIE, format!("{SESSION_COOKIE}={}", who.session))
        .header("x-csrf-token", &who.csrf);
    if method != "GET" {
        b = b
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json");
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

// ---------------------------------------------------------------- the test

#[tokio::test]
async fn s3_permissions() {
    let pool = test_pool().await;
    reset(&pool).await;

    // ---------------------------------------------- world
    let new = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let doing = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Doing','doing',2) RETURNING id").await;
    let done = id(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Done','done',3) RETURNING id").await;
    let bug = id(&pool, "INSERT INTO trackers (name, position, default_status_id) SELECT 'Bug', 1, id FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let feature = id(&pool, "INSERT INTO trackers (name, position, default_status_id, private_by_default) SELECT 'Feature', 2, id, true FROM issue_statuses WHERE name = 'New' RETURNING id").await;
    let task = id(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('Task', 3) RETURNING id",
    )
    .await;

    let mut users = std::collections::BTreeMap::new();
    for (login, admin) in [
        ("alice", false),
        ("carol", false),
        ("dave", false),
        ("erin", false),
        ("gina", false),
        ("lou", false),
        ("tim", false),
        ("root", true),
    ] {
        let u = auth::create_user(&pool, login, login, PW, admin)
            .await
            .unwrap();
        users.insert(login, u);
    }
    let tim = users["tim"];
    let (alice, carol, dave, erin, gina, lou, root) = (
        users["alice"],
        users["carol"],
        users["dave"],
        users["erin"],
        users["gina"],
        users["lou"],
        users["root"],
    );
    let team = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group','Team') RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO group_members VALUES ($1,$2)")
        .bind(team)
        .bind(gina)
        .execute(&pool)
        .await
        .unwrap();

    let web = id(
        &pool,
        "INSERT INTO projects (key, name, is_public) VALUES ('WEB','Web',true) RETURNING id",
    )
    .await;
    let private_p = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('PRIV','Private') RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects",
    )
    .await;
    for (p, t) in [
        (web, bug),
        (web, feature),
        (private_p, bug),
        (private_p, feature),
    ] {
        sqlx::query("INSERT INTO project_trackers VALUES ($1,$2)")
            .bind(p)
            .bind(t)
            .execute(&pool)
            .await
            .unwrap();
    }

    let dev = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    let reporter = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','default') RETURNING id",
    )
    .await;
    let viewer = id(&pool, "INSERT INTO roles (name, issues_visibility, assignable) VALUES ('Viewer','default', false) RETURNING id").await;
    let non_member = id(
        &pool,
        "INSERT INTO roles (name, builtin) VALUES ('Non member','non_member') RETURNING id",
    )
    .await;
    grant(
        &pool,
        dev,
        &[
            perm::VIEW_ISSUES,
            perm::ADD_ISSUES,
            perm::EDIT_ISSUES,
            perm::ADD_ISSUE_NOTES,
            perm::SET_NOTES_PRIVATE,
            perm::MANAGE_ISSUE_RELATIONS,
            perm::SET_ISSUES_PRIVATE,
        ],
    )
    .await;
    // Dev edits Bugs only: per-permission tracker set.
    exec(
        &pool,
        "UPDATE role_permissions SET all_trackers = false WHERE permission = 'edit_issues'",
    )
    .await;
    sqlx::query("INSERT INTO role_permission_trackers VALUES ($1, 'edit_issues', $2)")
        .bind(dev)
        .bind(bug)
        .execute(&pool)
        .await
        .unwrap();
    grant(
        &pool,
        reporter,
        &[
            perm::VIEW_ISSUES,
            perm::ADD_ISSUES,
            perm::EDIT_OWN_ISSUES,
            perm::ADD_ISSUE_NOTES,
            perm::SET_OWN_ISSUES_PRIVATE,
        ],
    )
    .await;
    grant(&pool, viewer, &[perm::VIEW_ISSUES, perm::ADD_ISSUE_NOTES]).await;
    grant(&pool, non_member, &[perm::VIEW_ISSUES]).await;
    member(&pool, web, alice, dev).await;
    member(&pool, web, carol, reporter).await;
    member(&pool, web, dave, viewer).await;
    member(&pool, web, team, dev).await;
    member(&pool, web, lou, dev).await;
    member(&pool, private_p, carol, dev).await;
    // Triage (PRIV): views Bugs and Features, edits Bugs only. Two tracker-
    // limited permissions with different sets must not be merged.
    let triage = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Triage','all') RETURNING id",
    )
    .await;
    for (perm_name, trackers) in [
        (perm::VIEW_ISSUES, vec![bug, feature]),
        (perm::EDIT_ISSUES, vec![bug]),
    ] {
        sqlx::query("INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ($1, $2, false)")
            .bind(triage)
            .bind(perm_name)
            .execute(&pool)
            .await
            .unwrap();
        for t in trackers {
            sqlx::query("INSERT INTO role_permission_trackers VALUES ($1, $2, $3)")
                .bind(triage)
                .bind(perm_name)
                .bind(t)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    member(&pool, private_p, tim, triage).await;

    // Workflow (Bug): Dev New→Doing always, New→Done assignee only;
    // Reporter New→Done author only. Feature: Dev New→Doing.
    transition(&pool, bug, dev, new, doing, "always").await;
    transition(&pool, bug, dev, new, done, "assignee").await;
    transition(&pool, bug, reporter, new, done, "author").await;
    transition(&pool, feature, dev, new, doing, "always").await;

    let budget = custom_fields::create_field(
        &pool,
        NewField {
            name: "Budget",
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[bug, feature],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[dev],
        },
    )
    .await
    .unwrap();
    let secret = custom_fields::create_field(
        &pool,
        NewField {
            name: "Secret",
            ty: FieldType::String,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[reporter],
        },
    )
    .await
    .unwrap();
    let browser_f = custom_fields::create_field(
        &pool,
        NewField {
            name: "Browser",
            ty: FieldType::List,
            description: "",
            is_required: true,
            is_for_all: true,
            trackers: &[bug],
            projects: &[],
            options: &["Safari", "Firefox"],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    exec(
        &pool,
        "UPDATE field_options SET active = false WHERE label = 'Firefox'",
    )
    .await;
    let (budget_k, secret_k, browser_k) = (
        format!("cf:{budget}"),
        format!("cf:{secret}"),
        format!("cf:{browser_f}"),
    );
    // Field rules (Bug): at New both Dev and Reporter make subject readonly;
    // at Doing, Dev requires due_date, Budget, and Secret. Dev cannot read
    // Secret, so (Redmine) its rule for Dev becomes readonly; agents cannot
    // read Budget, so for them the Doing transition is blocked.
    rule(&pool, bug, dev, new, "subject", "readonly").await;
    rule(&pool, bug, reporter, new, "subject", "readonly").await;
    rule(&pool, bug, dev, doing, "due_date", "required").await;
    rule(&pool, bug, dev, doing, &secret_k, "required").await;
    rule(&pool, bug, dev, doing, &budget_k, "required").await;
    rule(&pool, bug, reporter, done, "description", "required").await;

    let mk = |project, tracker, author, assignee: Option<Uuid>, private| NewIssue {
        project_id: project,
        tracker_id: tracker,
        status_id: new,
        subject: "issue",
        description: "",
        author_id: author,
        assigned_to_id: assignee,
        is_private: private,
        parent_id: None,
    };
    let (_, w1) = create_issue(&pool, mk(web, bug, carol, None, false))
        .await
        .unwrap();
    let (_, w2) = create_issue(&pool, mk(web, bug, carol, Some(alice), false))
        .await
        .unwrap();
    let (_, w3) = create_issue(&pool, mk(web, feature, carol, None, false))
        .await
        .unwrap();
    let (_, w4) = create_issue(&pool, mk(web, bug, carol, Some(team), false))
        .await
        .unwrap();
    let (_, p1) = create_issue(&pool, mk(private_p, bug, carol, None, false))
        .await
        .unwrap();
    let (_, p2) = create_issue(&pool, mk(private_p, feature, carol, None, false))
        .await
        .unwrap();

    let app = Arc::new(App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    });
    let as_ = AuthContext::test_user;
    let schema = |who: AuthContext, p: &str, t: &str| {
        let app = app.clone();
        let (p, t) = (p.to_string(), t.to_string());
        async move { describe::tracker_schema(&app, who, &p, &t).await }
    };
    let actions = |who: AuthContext, key: &str| {
        let app = app.clone();
        let key = key.to_string();
        async move { describe::issue_actions(&app, who, &key).await }
    };

    // ---------------------------------------------- static describe
    let s = schema(as_(alice), "WEB", "Bug").await.unwrap();
    assert_eq!(s.project_id, web);
    assert_eq!(s.tracker_id, bug);
    assert_eq!(field(&s, "subject").unwrap().ops, ["create", "update"]);
    assert!(field(&s, "subject").unwrap().required);
    assert_eq!(field(&s, "status_id").unwrap().ops, ["transition"]);
    assert_eq!(
        field(&s, "assigned_to_id")
            .unwrap()
            .options_source
            .as_deref(),
        Some(format!("/api/v1/projects/{web}/assignees").as_str())
    );
    assert!(field(&s, "fixed_version_id").is_some());
    assert!(field(&s, &budget_k).is_some(), "Dev reads Budget");
    assert!(
        field(&s, &secret_k).is_none(),
        "restricted field leaks nothing"
    );
    let serialized = serde_json::to_string(&s).unwrap();
    assert!(!serialized.contains("Secret"), "{serialized}");
    let b = field(&s, &browser_k).unwrap();
    assert!(b.delegable, "visible custom fields are delegable");
    assert!(b.required);
    let opts = b.options.as_ref().unwrap();
    assert_eq!(opts.len(), 2);
    assert!(opts.iter().any(|o| o.label == "Firefox" && !o.active));
    assert!(s.creation.allowed);
    assert_eq!(s.creation.initial_status_id, Some(new));
    assert!(!s.creation.is_private.default);
    assert_eq!(s.creation.is_private.allowed_values, [false, true]);
    assert_eq!(s.limits.max_ops, 10);
    assert_eq!(s.command_schema_version, 1);
    assert!(s.unsupported.iter().any(|u| u == "delete_issue"));

    // Per-permission trackers: Dev may add Features but edits only Bugs.
    let s = schema(as_(alice), "WEB", &feature.to_string())
        .await
        .unwrap();
    assert_eq!(field(&s, "subject").unwrap().ops, ["create"]);
    assert!(field(&s, "status_id").unwrap().ops.is_empty());
    assert!(
        s.creation.is_private.default,
        "Feature is private by default"
    );

    // Reporter (edit_own): update is possible on own issues, and own-private.
    let s = schema(as_(carol), "web", "bug").await.unwrap();
    assert_eq!(field(&s, "subject").unwrap().ops, ["create", "update"]);
    assert!(field(&s, &secret_k).is_some(), "Reporter reads Secret");
    assert!(field(&s, &budget_k).is_none());
    // The SPA's delegation wizard offers only fields an agent may be given.
    assert!(
        !field(&s, &secret_k).unwrap().delegable,
        "visible = false is not delegable"
    );
    assert!(field(&s, "subject").unwrap().delegable);
    assert_eq!(s.creation.is_private.allowed_values, [false, true]);

    // Viewer: reads, no ops, cannot create.
    let s = schema(as_(dave), "WEB", "Bug").await.unwrap();
    assert!(s.fields.iter().all(|f| f.ops.is_empty()));
    assert!(!s.creation.allowed);
    assert_eq!(s.creation.is_private.allowed_values, [false]);

    // Non-member of the public project: Non member role, reads only.
    let s = schema(as_(erin), "WEB", "Bug").await.unwrap();
    assert!(s.fields.iter().all(|f| f.ops.is_empty()));
    assert!(field(&s, &budget_k).is_none() && field(&s, &secret_k).is_none());

    // Admin, not a member: everything, every field.
    let s = schema(as_(root), "WEB", "Feature").await.unwrap();
    assert_eq!(field(&s, "subject").unwrap().ops, ["create", "update"]);
    assert!(field(&s, &budget_k).is_some());

    // not_found: unknown project, invisible project, unknown or disabled
    // tracker; same code and message for each pair.
    let missing_p = schema(as_(alice), &Uuid::now_v7().to_string(), "Bug")
        .await
        .unwrap_err();
    let hidden_p = schema(as_(alice), &private_p.to_string(), "Bug")
        .await
        .unwrap_err();
    let hidden_k = schema(as_(alice), "PRIV", "Bug").await.unwrap_err();
    let junk_p = schema(as_(alice), "no such project!", "Bug")
        .await
        .unwrap_err();
    for e in [&missing_p, &hidden_p, &hidden_k, &junk_p] {
        assert_eq!(code(e), ErrorCode::NotFound);
        assert_eq!(msg(e), "no project");
    }
    let no_t = schema(as_(alice), "WEB", "Epic").await.unwrap_err();
    let off_t = schema(as_(alice), "WEB", &task.to_string())
        .await
        .unwrap_err();
    for e in [&no_t, &off_t] {
        assert_eq!(code(e), ErrorCode::NotFound);
        assert_eq!(msg(e), "no tracker in this project");
    }

    // ---------------------------------------------- dynamic describe
    // alice (Dev) on a Bug she did not write, not assigned.
    let a = actions(as_(alice), &w1).await.unwrap();
    let subj = action(&a, "subject").unwrap();
    assert!(!subj.editable && subj.required);
    assert_eq!(subj.reason.as_deref(), Some("workflow_readonly"));
    assert!(action(&a, "description").unwrap().editable);
    assert!(action(&a, &budget_k).is_some());
    assert!(action(&a, &secret_k).is_none());
    assert!(action(&a, &browser_k).unwrap().required);
    assert_eq!(targets(&a), ["Doing"], "Done needs the assignee condition");
    let t = &a.transitions[0];
    assert_eq!(t.to_status_id, doing);
    assert_eq!(t.then_required, ["due_date".to_string(), budget_k.clone()]);
    assert!(
        !t.blocked,
        "an unreadable field's rule turns readonly for that role, so it never blocks a person"
    );
    assert!(!serde_json::to_string(&a).unwrap().contains(&secret_k));
    assert!(a.notes.as_ref().unwrap().can_add && a.notes.as_ref().unwrap().can_set_private);
    assert!(a.duplicate_of.as_ref().unwrap().can_propose);
    assert!(
        !serde_json::to_string(&a).unwrap().contains("can_apply"),
        "no execution ticket"
    );

    // Assigned to alice: the assignee transition opens (baseline).
    let a = actions(as_(alice), &w2).await.unwrap();
    assert_eq!(targets(&a), ["Doing", "Done"]);
    // get agrees with describe.
    let g = service::get(&app, as_(alice), &w2).await.unwrap();
    assert_eq!(g.allowed_statuses, ["New", "Doing", "Done"]);
    let g = service::get(&app, as_(dave), &w2).await.unwrap();
    assert_eq!(
        g.allowed_statuses,
        ["New"],
        "no edit permission, no transitions"
    );

    // Assigned to a group: its member gina gets the assignee transition,
    // alice (not in the group) does not.
    assert_eq!(
        targets(&actions(as_(gina), &w4).await.unwrap()),
        ["Doing", "Done"]
    );
    assert_eq!(targets(&actions(as_(alice), &w4).await.unwrap()), ["Doing"]);

    // carol (Reporter, the author): edit_own applies, author transition.
    let a = actions(as_(carol), &w1).await.unwrap();
    assert_eq!(
        action(&a, "subject").unwrap().reason.as_deref(),
        Some("workflow_readonly")
    );
    assert!(action(&a, "description").unwrap().editable);
    assert!(action(&a, &secret_k).unwrap().editable);
    assert_eq!(targets(&a), ["Done"]);
    assert_eq!(a.transitions[0].then_required, ["description"]);
    assert!(!a.transitions[0].blocked);
    assert!(a.notes.as_ref().unwrap().can_add);
    assert!(!a.notes.as_ref().unwrap().can_set_private);
    assert!(!a.duplicate_of.as_ref().unwrap().can_propose);

    // dave (Viewer): notes only.
    let a = actions(as_(dave), &w1).await.unwrap();
    assert!(
        a.fields
            .iter()
            .all(|f| !f.editable && f.reason.as_deref() == Some("not_editable"))
    );
    assert!(a.transitions.is_empty());
    assert!(a.notes.as_ref().unwrap().can_add);

    // erin (non-member, public project): reads, nothing else.
    let a = actions(as_(erin), &w1).await.unwrap();
    assert!(a.fields.iter().all(|f| !f.editable));
    assert!(!a.notes.as_ref().unwrap().can_add);

    // alice on a Feature: Dev cannot edit Features (per-permission tracker),
    // so she cannot mark it a duplicate either (both ends must be editable).
    let a = actions(as_(alice), &w3).await.unwrap();
    assert!(a.fields.iter().all(|f| !f.editable));
    assert!(a.transitions.is_empty());
    assert!(a.notes.as_ref().unwrap().can_add);
    assert!(!a.duplicate_of.as_ref().unwrap().can_propose);

    // tim (Triage): sees both trackers, edits only the Bug.
    let a = actions(as_(tim), &p1).await.unwrap();
    assert!(action(&a, "description").unwrap().editable);
    let a = actions(as_(tim), &p2).await.unwrap();
    assert!(a.fields.iter().all(|f| !f.editable), "edit set is Bug only");
    let s = schema(as_(tim), "PRIV", "Feature").await.unwrap();
    assert!(field(&s, "subject").unwrap().ops.is_empty());

    // Two trackers whose names differ only in case: a name is ambiguous, the id is not.
    let bug2 = id(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('BUG', 9) RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO project_trackers VALUES ($1,$2)")
        .bind(web)
        .bind(bug2)
        .execute(&pool)
        .await
        .unwrap();
    let e = schema(as_(alice), "WEB", "bug").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AmbiguousReference);
    assert_eq!(
        schema(as_(alice), "WEB", &bug.to_string())
            .await
            .unwrap()
            .tracker_id,
        bug
    );
    assert_eq!(
        schema(as_(alice), "WEB", &bug2.to_string())
            .await
            .unwrap()
            .tracker_id,
        bug2
    );
    exec(&pool, "DELETE FROM trackers WHERE name = 'BUG'").await;

    // Admin: every role that considers workflow (Dev, Reporter, Triage);
    // all make subject readonly at New; only the always transition applies.
    rule(&pool, bug, triage, new, "subject", "readonly").await;
    let a = actions(as_(root), &w1).await.unwrap();
    assert_eq!(
        action(&a, "subject").unwrap().reason.as_deref(),
        Some("workflow_readonly")
    );
    assert!(action(&a, &secret_k).is_some() && action(&a, &budget_k).is_some());
    assert_eq!(targets(&a), ["Doing"]);
    assert!(!a.transitions[0].blocked);
    assert_eq!(
        a.transitions[0].then_required,
        std::slice::from_ref(&budget_k),
        "admin = Dev + Reporter: Reporter has no due_date rule (no rule overall); Reporter          cannot read Budget (readonly) + Dev required → required; Dev cannot read Secret          (readonly) + Reporter no rule → no rule"
    );

    // Closed project: reads only.
    exec(
        &pool,
        "UPDATE projects SET status = 'closed' WHERE key = 'WEB'",
    )
    .await;
    let a = actions(as_(alice), &w1).await.unwrap();
    assert!(
        a.fields
            .iter()
            .all(|f| f.reason.as_deref() == Some("project_not_writable"))
    );
    assert!(a.transitions.is_empty());
    assert!(!a.notes.as_ref().unwrap().can_add);
    let s = schema(as_(alice), "WEB", "Bug").await.unwrap();
    assert!(s.fields.iter().all(|f| f.ops.is_empty()) && !s.creation.allowed);
    exec(
        &pool,
        "UPDATE projects SET status = 'active' WHERE key = 'WEB'",
    )
    .await;

    // E4w-1 (describe): invisible and missing keys look the same.
    let shape = |e: &anyhow::Error, key: &str| {
        assert_eq!(code(e), ErrorCode::NotFound);
        assert_eq!(msg(e), format!("no issue {key}"));
    };
    shape(&actions(as_(alice), &p1).await.unwrap_err(), &p1);
    shape(
        &actions(as_(alice), "PRIV-999").await.unwrap_err(),
        "PRIV-999",
    );
    shape(
        &actions(as_(alice), "WEB-999").await.unwrap_err(),
        "WEB-999",
    );
    shape(
        &actions(as_(alice), "bad\0key").await.unwrap_err(),
        "BAD\0KEY",
    );
    assert!(actions(as_(carol), &p1).await.is_ok());

    // ---------------------------------------------- T-P10 assignees
    let names = |list: Vec<describe::Assignee>| -> Vec<String> {
        list.into_iter().map(|a| a.name).collect()
    };
    let l = describe::assignees(&app, as_(alice), "WEB").await.unwrap();
    assert_eq!(
        names(l.assignees),
        ["alice", "carol", "gina", "lou"],
        "Viewer is not assignable; gina through the Team group; groups off"
    );
    exec(
        &pool,
        "UPDATE org_settings SET issue_group_assignment = true",
    )
    .await;
    let l = describe::assignees(&app, as_(alice), "WEB").await.unwrap();
    // Gap G48 (design 06 §6.2): each user carries its own login, a group
    // none; the list is the same people as without logins.
    for a in &l.assignees {
        let login: Option<String> =
            sqlx::query_scalar("SELECT login FROM principals WHERE id = $1")
                .bind(a.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        match a.kind.as_str() {
            "user" => assert_eq!(a.login, login, "{a:?}"),
            _ => assert_eq!(a.login, None, "{a:?}"),
        }
    }
    assert!(l.assignees.iter().any(|a| a.kind == "group"));
    assert_eq!(
        names(l.assignees),
        ["alice", "carol", "gina", "lou", "Team"]
    );
    exec(
        &pool,
        "UPDATE principals SET status = 'locked' WHERE login = 'lou'",
    )
    .await;
    let l = describe::assignees(&app, as_(alice), "WEB").await.unwrap();
    assert_eq!(
        names(l.assignees),
        ["alice", "carol", "gina", "Team"],
        "locked users drop out"
    );
    exec(
        &pool,
        "UPDATE org_settings SET issue_group_assignment = false",
    )
    .await;
    // The author, when active, is assignable on their issue even without a role.
    let mut conn = pool.acquire().await.unwrap();
    let with_author = describe::assignable(&mut conn, web, Some(erin))
        .await
        .unwrap();
    assert!(with_author.iter().any(|a| a.id == erin));
    let with_locked = describe::assignable(&mut conn, web, Some(lou))
        .await
        .unwrap();
    assert!(!with_locked.iter().any(|a| a.id == lou));
    drop(conn);
    let e = describe::assignees(&app, as_(alice), "PRIV")
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);

    // ---------------------------------------------- REST and MCP entry points
    let router = api::router(
        app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let br = login(&router, "alice").await;
    let r = call(&router, "GET", &format!("/issues/{w1}/actions"), None, &br).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["key"], json!(w1));
    let r = call(
        &router,
        "GET",
        &format!("/projects/{web}/trackers/{bug}/schema"),
        None,
        &br,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["data"]["tracker_id"], json!(bug));
    let r = call(
        &router,
        "GET",
        &format!("/projects/{web}/assignees"),
        None,
        &br,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["data"]["assignees"].as_array().unwrap().len() >= 3);
    // G48 over REST: users have `login`; the Viewer (not assignable) and
    // people outside the project are not listed, with or without it.
    let logins: Vec<&str> = r.body["data"]["assignees"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["login"].as_str())
        .collect();
    assert_eq!(logins, ["alice", "carol", "gina"], "{}", r.body);
    let r = call(&router, "GET", &format!("/issues/{p1}/actions"), None, &br).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["error"]["code"], "not_found");

    let d = describe::describe(
        &app,
        as_(alice),
        DescribeParams {
            scope: DescribeScope::IssueActions,
            project: None,
            tracker: None,
            issue_key: Some(w1.clone()),
        },
    )
    .await
    .unwrap();
    assert!(d.issue_actions.is_some() && d.project_tracker.is_none());
    let e = describe::describe(
        &app,
        as_(alice),
        DescribeParams {
            scope: DescribeScope::ProjectTracker,
            project: Some("WEB".into()),
            tracker: None,
            issue_key: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let v: Value =
        serde_json::from_value(json!({"scope": "issue_actions", "issue_key": "X-1", "extra": 1}))
            .map(|p: Value| p)
            .unwrap();
    assert!(
        serde_json::from_value::<DescribeParams>(v).is_err(),
        "unknown fields rejected"
    );

    // ---------------------------------------------- agent: delegator ∩ delegation
    let client = agents::create_agent_client(&pool, "s3-agent", "egress-test")
        .await
        .unwrap();
    agents::set_agent_client_enabled(&pool, "s3-agent", true)
        .await
        .unwrap();
    let d = call(
        &router,
        "POST",
        "/delegations",
        Some(json!({
            "agent_client_id": client,
            "project_id": web,
            "proposal_ops": ["update_fields", "add_note", "transition"],
            "writable_fields": ["description"],
            "max_ops": 3,
        })),
        &br,
    )
    .await;
    assert_eq!(d.status, StatusCode::OK, "{}", d.body);
    let principal = d.body["data"]["agent_principal_id"]
        .as_str()
        .unwrap()
        .to_string();
    let t = call(
        &router,
        "POST",
        &format!("/ai-team/{principal}/credentials"),
        Some(json!({"label": "s3"})),
        &br,
    )
    .await;
    assert_eq!(t.status, StatusCode::CREATED, "{}", t.body);
    let token = t.body["data"]["token"].as_str().unwrap();
    let agent = auth::authenticate_bearer(&pool, token)
        .await
        .unwrap()
        .unwrap();

    let a = actions(agent, &w2).await.unwrap();
    let keys: Vec<&str> = a.fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(
        keys,
        ["description"],
        "only delegated, visible fields are listed"
    );
    assert!(a.fields[0].editable);
    assert_eq!(
        targets(&a),
        ["Doing", "Done"],
        "the delegator is the assignee"
    );
    let doing_t = &a.transitions[0];
    assert!(
        doing_t.blocked,
        "Budget is required after Doing and agents cannot read it"
    );
    assert_eq!(doing_t.then_required, ["due_date"]);
    assert!(!serde_json::to_string(&a).unwrap().contains(&budget_k));
    assert!(a.notes.as_ref().unwrap().can_add);
    assert!(
        a.duplicate_of.is_none(),
        "outside the delegation: not listed"
    );
    let s = schema(agent, "WEB", "Bug").await.unwrap();
    assert!(
        field(&s, &budget_k).is_none(),
        "agents read visible fields only"
    );
    assert_eq!(field(&s, "description").unwrap().ops, ["update"]);
    assert!(field(&s, "subject").unwrap().ops.is_empty());
    assert_eq!(field(&s, "status_id").unwrap().ops, ["transition"]);
    assert!(!s.creation.allowed);
    assert_eq!(s.limits.max_ops, 3);
    let g = service::get(&app, agent, &w2).await.unwrap();
    assert_eq!(g.allowed_statuses, ["New", "Doing", "Done"]);
    // Not delegated / not readable → not_found.
    shape(&actions(agent, &p1).await.unwrap_err(), &p1);
    let e = schema(agent, "PRIV", "Bug").await.unwrap_err();
    assert_eq!(msg(&e), "no project");

    // carol's agent: only update_fields, and only WEB, although carol (the
    // delegator) can read PRIV. Transitions are outside the delegation.
    let bc = login(&router, "carol").await;
    let d = call(
        &router,
        "POST",
        "/delegations",
        Some(json!({
            "agent_client_id": client,
            "project_id": web,
            "proposal_ops": ["update_fields"],
            "writable_fields": ["description"],
        })),
        &bc,
    )
    .await;
    assert_eq!(d.status, StatusCode::OK, "{}", d.body);
    let principal = d.body["data"]["agent_principal_id"]
        .as_str()
        .unwrap()
        .to_string();
    let t = call(
        &router,
        "POST",
        &format!("/ai-team/{principal}/credentials"),
        Some(json!({"label": "s3c"})),
        &bc,
    )
    .await;
    assert_eq!(t.status, StatusCode::CREATED, "{}", t.body);
    let carols_agent = auth::authenticate_bearer(&pool, t.body["data"]["token"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(targets(&actions(as_(carol), &w1).await.unwrap()), ["Done"]);
    let a = actions(carols_agent, &w1).await.unwrap();
    assert!(a.transitions.is_empty(), "transition is not delegated");
    assert!(a.notes.is_none() && a.duplicate_of.is_none());
    let g = service::get(&app, carols_agent, &w1).await.unwrap();
    assert_eq!(g.allowed_statuses, ["New"]);
    let s = schema(carols_agent, "WEB", "Bug").await.unwrap();
    assert!(field(&s, "status_id").unwrap().ops.is_empty());
    assert!(
        actions(as_(carol), &p1).await.is_ok(),
        "the delegator reads PRIV"
    );
    shape(&actions(carols_agent, &p1).await.unwrap_err(), &p1);
    assert_eq!(
        msg(&schema(carols_agent, "PRIV", "Bug").await.unwrap_err()),
        "no project"
    );

    // ---------------------------------------------- settings versions
    let before = schema(as_(alice), "WEB", "Bug").await.unwrap();
    exec(&pool, "UPDATE field_options SET position = position + 1").await;
    exec(
        &pool,
        "DELETE FROM workflow_field_rules WHERE rule = 'required' AND field_key = 'description'",
    )
    .await;
    let after = schema(as_(alice), "WEB", "Bug").await.unwrap();
    assert_eq!(after.schema_version, before.schema_version + 1);
    assert_eq!(after.workflow_version, before.workflow_version + 1);

    // ---------------------------------------------- schema guards (0007)
    // Permission names: the code's list is the DB's list.
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'role_permissions_known'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for p in perm::PERMISSIONS {
        assert!(def.contains(&format!("'{p}'")), "{p} missing from {def}");
    }
    assert_eq!(
        def.matches("::text").count(),
        perm::PERMISSIONS.len(),
        "{def}"
    );
    let e =
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, 'edit_isues')")
            .bind(dev)
            .execute(&pool)
            .await;
    assert!(e.is_err(), "typo in a permission name is rejected");

    // Every project gets a schedule settings row, also new ones.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM project_schedule_settings WHERE inherit_org")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 2);

    // A version belongs to its project: an issue elsewhere cannot use it.
    let v = id(&pool, &format!(
        "INSERT INTO versions (project_id, name, name_key, created_by) VALUES ('{web}', 'v1', 'v1', '{alice}') RETURNING id"
    ))
    .await;
    let ok = sqlx::query("UPDATE issues SET fixed_version_id = $1 WHERE project_id = $2")
        .bind(v)
        .bind(web)
        .execute(&pool)
        .await;
    assert!(ok.is_ok());
    let bad = sqlx::query("UPDATE issues SET fixed_version_id = $1 WHERE project_id = $2")
        .bind(v)
        .bind(private_p)
        .execute(&pool)
        .await;
    assert!(
        bad.is_err(),
        "cross-project version rejected by the composite FK"
    );
    let dup = sqlx::query(
        "INSERT INTO versions (project_id, name, name_key, created_by) VALUES ($1, 'V1', 'v1', $2)",
    )
    .bind(web)
    .bind(alice)
    .execute(&pool)
    .await;
    assert!(dup.is_err(), "name_key is unique per project");
    // Dates: order and range.
    for (start, due) in [
        ("2026-12-01", "2026-11-01"),
        ("1899-12-31", "2026-01-01"),
        ("2026-01-01", "2200-01-01"),
    ] {
        let e = sqlx::query(
            "UPDATE issues SET start_date = $1::date, due_date = $2::date WHERE project_id = $3",
        )
        .bind(start)
        .bind(due)
        .bind(web)
        .execute(&pool)
        .await;
        assert!(e.is_err(), "{start}..{due} must be rejected");
    }
    let validated: Vec<bool> = sqlx::query_scalar(
        "SELECT convalidated FROM pg_constraint WHERE conname IN ('issues_date_order', 'issues_date_range')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(validated, [true, true]);
}

/// Migration 0007 on a database with bad dates: out-of-range dates become
/// NULL, then start > due clears both; each fix is logged with the old value
/// and reason, lock_version goes up once, and the CHECKs end up validated.
#[tokio::test]
async fn migration_0007_repairs_dates() {
    let base = test_pool().await;
    // Next to the test database, so parallel verifiers do not share it.
    let test_db = common::test_db();
    let name = &test_db.scratch("mig0007");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&base)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&base)
        .await
        .unwrap();
    let url = test_db.url_for(name);
    let pool = PgPool::connect(&url).await.unwrap();
    let run = |version: i64| {
        let pool = pool.clone();
        async move {
            let m = db::MIGRATOR
                .iter()
                .find(|m| m.version == version)
                .expect("migration");
            sqlx::raw_sql(sqlx::AssertSqlSafe(m.sql.as_str().to_string()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("migration {version}: {e}"));
        }
    };
    for v in 1..=6 {
        run(v).await;
    }
    exec(
        &pool,
        "
        INSERT INTO principals (kind, login, name) VALUES ('user', 'u', 'U');
        INSERT INTO issue_statuses (name, category) VALUES ('New', 'todo');
        INSERT INTO trackers (name) VALUES ('Bug');
        INSERT INTO projects (key, name) VALUES ('OLD', 'Old');",
    )
    .await;
    let ins = |subject: &str, start: Option<&str>, due: Option<&str>| {
        let pool = pool.clone();
        let (subject, start, due) = (
            subject.to_string(),
            start.map(String::from),
            due.map(String::from),
        );
        async move {
            sqlx::query(
                "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id, start_date, due_date)
                 SELECT p.id, t.id, s.id, $1, u.id, $2::date, $3::date
                 FROM projects p, trackers t, issue_statuses s, principals u",
            )
            .bind(subject)
            .bind(start)
            .bind(due)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    ins("fine", Some("2026-01-01"), Some("2026-02-01")).await;
    ins("order", Some("2026-03-01"), Some("2026-02-01")).await;
    ins("start out", Some("1800-01-01"), Some("2026-02-01")).await;
    ins("due out, then fine", Some("2026-01-01"), Some("3000-01-01")).await;
    ins("both out", Some("1800-01-01"), Some("3000-01-01")).await;
    ins("start out, due alone", Some("2300-01-01"), None).await;
    // Out of range and (raw) start > due: only the out-of-range end goes; the
    // order rule looks at the values left after step 1.
    ins(
        "start out, order raw",
        Some("2300-01-01"),
        Some("2026-02-01"),
    )
    .await;
    run(7).await;

    type Row = (String, Option<String>, Option<String>, i32);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT subject, start_date::text, due_date::text, lock_version FROM issues ORDER BY subject",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let get = |s: &str| rows.iter().find(|r| r.0 == s).unwrap().clone();
    assert_eq!(
        get("fine"),
        (
            "fine".into(),
            Some("2026-01-01".into()),
            Some("2026-02-01".into()),
            0
        )
    );
    assert_eq!(get("order"), ("order".into(), None, None, 1));
    assert_eq!(
        get("start out"),
        ("start out".into(), None, Some("2026-02-01".into()), 1)
    );
    assert_eq!(
        get("due out, then fine"),
        (
            "due out, then fine".into(),
            Some("2026-01-01".into()),
            None,
            1
        )
    );
    assert_eq!(get("both out"), ("both out".into(), None, None, 1));
    assert_eq!(
        get("start out, due alone"),
        ("start out, due alone".into(), None, None, 1)
    );
    assert_eq!(
        get("start out, order raw"),
        (
            "start out, order raw".into(),
            None,
            Some("2026-02-01".into()),
            1
        )
    );

    let log: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT i.subject, f.field_key, f.old_value::text, f.reason
         FROM migration_date_fixes f JOIN issues i ON i.id = f.issue_id ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let log: Vec<(&str, &str, &str, &str)> = log
        .iter()
        .map(|(a, b, c, d)| (a.as_str(), b.as_str(), c.as_str(), d.as_str()))
        .collect();
    assert_eq!(
        log,
        [
            ("both out", "due_date", "3000-01-01", "out_of_range"),
            ("both out", "start_date", "1800-01-01", "out_of_range"),
            (
                "due out, then fine",
                "due_date",
                "3000-01-01",
                "out_of_range"
            ),
            ("order", "due_date", "2026-02-01", "date_order"),
            ("order", "start_date", "2026-03-01", "date_order"),
            ("start out", "start_date", "1800-01-01", "out_of_range"),
            (
                "start out, due alone",
                "start_date",
                "2300-01-01",
                "out_of_range"
            ),
            (
                "start out, order raw",
                "start_date",
                "2300-01-01",
                "out_of_range"
            ),
        ]
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM project_schedule_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "existing projects get a schedule settings row");

    // 0008 (S4): every fixed issue gets one `fields_changed` import event with
    // the old values and reasons, field origins, and the log table is gone.
    run(8).await;
    type Ev = (String, i64, String, String, Option<String>, Value);
    let events: Vec<Ev> = sqlx::query_as(
        "SELECT i.subject, e.seq, e.kind, e.origin, e.system_operation, e.payload
         FROM issue_events e JOIN issues i ON i.id = e.issue_id ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        events.len(),
        6,
        "one event per fixed issue; 'fine' has none"
    );
    for (subject, seq, kind, origin, sysop, _) in &events {
        assert_eq!(
            (*seq, kind.as_str(), origin.as_str(), sysop.as_deref()),
            (1, "fields_changed", "import", Some("u39_date_fix")),
            "{subject}"
        );
    }
    let order = &events.iter().find(|e| e.0 == "order").unwrap().5;
    assert_eq!(
        order,
        &json!({
            "changes": {
                "start_date": {"old": "2026-03-01", "new": null},
                "due_date": {"old": "2026-02-01", "new": null}
            },
            "reasons": {"start_date": "date_order", "due_date": "date_order"}
        })
    );
    let both = &events.iter().find(|e| e.0 == "both out").unwrap().5;
    assert_eq!(
        both["reasons"],
        json!({"start_date": "out_of_range", "due_date": "out_of_range"})
    );
    let seqs: Vec<(String, i64)> =
        sqlx::query_as("SELECT subject, last_event_seq FROM issues ORDER BY subject")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (s, seq) in &seqs {
        assert_eq!(*seq, if s == "fine" { 0 } else { 1 }, "{s}");
    }
    let origins: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_field_origins")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(origins, 8, "one per fixed field (the eight log rows above)");
    let gone: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('migration_date_fixes')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(gone, None);
    let prio: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issues i JOIN issue_priorities p ON p.id = i.priority_id AND p.is_default",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(prio, 7, "existing issues get the default priority");
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&base)
    .await
    .unwrap();
}
