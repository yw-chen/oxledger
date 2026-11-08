// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S0: P3 (authorization functions take a connection, so they run
//! inside a write transaction), P4 (one place decides whether the caller is an
//! active user) and the error codes the read tools return (P9).
//!
//! Needs PostgreSQL 18; see `common::test_pool` for which database it uses.

mod common;

use common::test_pool;
use oxledger::custom_fields;
use oxledger::db::{self, Caller};
use oxledger::error::{ErrorCode, as_api_error};
use oxledger::search::SearchIndex;
use oxledger::service::{self, App, SearchParams};
use oxledger::test_support::{NewIssue, create_issue};
use sqlx::PgPool;
use uuid::Uuid;

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, workflow_transitions, project_trackers,
                  member_roles, members, role_permission_trackers, role_permissions, roles, enabled_modules,
                  projects, api_keys, group_members, aliases, principals, trackers, issue_statuses CASCADE",
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

fn code(e: &anyhow::Error) -> ErrorCode {
    as_api_error(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .code
}

#[tokio::test]
async fn s0_scope_in_transaction_active_check_and_error_codes() {
    let pool = test_pool().await;
    reset(&pool).await;
    let status = id(
        &pool,
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo') RETURNING id",
    )
    .await;
    let tracker = id(
        &pool,
        "INSERT INTO trackers (name) VALUES ('Bug') RETURNING id",
    )
    .await;
    let other = id(
        &pool,
        "INSERT INTO trackers (name) VALUES ('Defect') RETURNING id",
    )
    .await;
    let admin = id(
        &pool,
        "INSERT INTO principals (kind, login, name, is_admin) VALUES ('user','root','Root',true) RETURNING id",
    )
    .await;
    let mia = id(
        &pool,
        "INSERT INTO principals (kind, login, name) VALUES ('user','mia','Mia') RETURNING id",
    )
    .await;
    let group = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group','Devs') RETURNING id",
    )
    .await;
    let project = id(
        &pool,
        "INSERT INTO projects (key, name, is_public) VALUES ('AUTH','Auth',false) RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO enabled_modules VALUES ($1, 'issue_tracking')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    // One alias on two trackers: an ambiguous name.
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label) VALUES ('tracker', $1, 'bugish'), ('tracker', $2, 'bugish')",
    )
    .bind(tracker)
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    let (_, key) = create_issue(
        &pool,
        NewIssue {
            project_id: project,
            tracker_id: tracker,
            status_id: status,
            subject: "login fails",
            description: "",
            author_id: admin,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap();

    // P3: load_scope, role_ids_for and is_admin run on the transaction's own
    // connection and see its uncommitted rows (a pool connection would not).
    let mut tx = pool.begin().await.unwrap();
    let p2: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (key, name, is_public) VALUES ('TX','In tx',true) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO enabled_modules VALUES ($1, 'issue_tracking')")
        .bind(p2)
        .execute(&mut *tx)
        .await
        .unwrap();
    let scope = db::load_scope(&mut tx, Caller(Some(admin))).await.unwrap();
    assert!(
        scope.project_ids().any(|p| p == p2),
        "scope sees the transaction's project"
    );
    let roles = db::role_ids_for(&mut tx, Caller(Some(mia)), p2)
        .await
        .unwrap();
    assert!(roles.is_empty(), "no non_member role is defined");
    assert!(db::is_admin(&mut tx, Caller(Some(admin))).await.unwrap());
    tx.rollback().await.unwrap();

    // P4: anonymous is fine; a group, an unknown id and a locked user are all
    // `unauthenticated`, in every authorization function.
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        db::effective(&mut conn, Caller(None))
            .await
            .unwrap()
            .user_id
            .is_none()
    );
    assert!(!db::is_admin(&mut conn, Caller(None)).await.unwrap());
    sqlx::query("UPDATE principals SET status = 'locked' WHERE id = $1")
        .bind(admin)
        .execute(&pool)
        .await
        .unwrap();
    for who in [group, Uuid::now_v7(), admin] {
        let c = Caller(Some(who));
        let unauth = ErrorCode::Unauthenticated;
        assert_eq!(
            code(&db::effective(&mut conn, c).await.unwrap_err()),
            unauth
        );
        assert_eq!(
            code(&db::load_scope(&mut conn, c).await.unwrap_err()),
            unauth
        );
        assert_eq!(code(&db::is_admin(&mut conn, c).await.unwrap_err()), unauth);
        assert_eq!(
            code(&db::role_ids_for(&mut conn, c, project).await.unwrap_err()),
            unauth,
            "role_ids_for used to treat {who} as a plain non-member"
        );
    }
    drop(conn);
    sqlx::query("UPDATE principals SET status = 'active' WHERE id = $1")
        .bind(admin)
        .execute(&pool)
        .await
        .unwrap();

    // P9: read tools return stable codes.
    let app = App {
        pool: pool.clone(),
        index: SearchIndex::open(None).unwrap(),
    };
    app.index.sync(&pool).await.unwrap();
    let root = Caller(Some(admin));
    assert!(service::get(&app, root, &key).await.is_ok());
    let missing = service::get(&app, root, "AUTH-999").await.unwrap_err();
    assert_eq!(code(&missing), ErrorCode::NotFound);
    // Invisible looks exactly like missing.
    let hidden = service::get(&app, Caller(Some(mia)), &key)
        .await
        .unwrap_err();
    let absent = service::get(&app, Caller(Some(mia)), "AUTH-999")
        .await
        .unwrap_err();
    assert_eq!(code(&hidden), ErrorCode::NotFound);
    assert_eq!(
        as_api_error(&hidden).unwrap().message.replace(&key, "K"),
        as_api_error(&absent)
            .unwrap()
            .message
            .replace("AUTH-999", "K")
    );

    let e = service::search(
        &app,
        root,
        SearchParams {
            cursor: Some("not-a-cursor".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);

    let e = service::search(
        &app,
        root,
        SearchParams {
            tracker: Some("bugish".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AmbiguousReference);
    let cands = &as_api_error(&e).unwrap().details.as_ref().unwrap()["candidates"];
    assert_eq!(cands.as_array().unwrap().len(), 2, "{cands}");

    let e = service::search(
        &app,
        root,
        SearchParams {
            status_category: Some("later".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    let e = service::resolve(&app, root, "x", Some("planet"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    assert_eq!(
        as_api_error(&e).unwrap().details.as_ref().unwrap()["path"],
        "kind"
    );

    let e = service::search(
        &app,
        root,
        SearchParams {
            filter: Some(serde_json::json!({"field": "Nope", "op": "eq", "value": 1})),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidFilter);

    reset(&pool).await;
}
