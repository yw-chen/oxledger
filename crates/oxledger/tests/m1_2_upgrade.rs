// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.2: upgrading project tab settings. Runs migrations
//! 0001–0020 on a temporary database `<test db>_upg12`, writes a project
//! with the default tabs and one with fifteen (five built-ins and ten
//! custom, the old maximum), then runs 0021: both get the built-ins `board`
//! and `triage` (hidden) at the end, the full one has seventeen, the CHECK
//! is 7–17, lock_version moves on, and no `security_epoch` changes. The
//! temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger";

/// (server base URL, temporary database name).
fn target() -> (String, String) {
    let url = match std::env::var("OXLEDGER_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            let admin = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_ADMIN_URL.into());
            format!("{}/oxledger_test", admin.rsplit_once('/').unwrap().0)
        }
    };
    let (base, rest) = url.rsplit_once('/').expect("database URL has a path");
    let name = rest.split('?').next().unwrap();
    assert!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "unexpected test database name {name:?}"
    );
    (base.to_string(), format!("{name}_upg12"))
}

async fn recreate(base: &str, name: &str, create: bool) {
    let mut admin = PgConnection::connect(&format!("{base}/postgres"))
        .await
        .expect("PostgreSQL 18 not reachable");
    // `name` is checked to be a plain identifier in `target`.
    admin
        .execute(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
        )))
        .await
        .unwrap();
    if create {
        admin
            .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .await
            .unwrap();
    }
    admin.close().await.unwrap();
}

async fn one(pool: &PgPool, sql: &str) -> Uuid {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn settings(pool: &PgPool, p: Uuid) -> (Value, bool, i32) {
    sqlx::query_as(
        "SELECT tabs, is_default, lock_version FROM project_tab_settings WHERE project_id = $1",
    )
    .bind(p)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn ids(tabs: &Value) -> Vec<String> {
    tabs.as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn m1_2_upgrade_project_tabs() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(20, &pool).await.unwrap();
    let plain = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('PLAIN','Plain') RETURNING id",
    )
    .await;
    let full = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('FULL','Full') RETURNING id",
    )
    .await;
    let (before, is_default, lv) = settings(&pool, plain).await;
    assert_eq!(before.as_array().unwrap().len(), 5);
    assert!(is_default);
    let mut tabs = before.as_array().unwrap().clone();
    for n in 0..10 {
        tabs.push(json!({"id": format!("l_{n:010}"), "kind": "link",
                         "title": format!("link {n}"), "hidden": false,
                         "url": format!("https://x.test/{n}")}));
    }
    sqlx::query(
        "UPDATE project_tab_settings SET tabs = $2, is_default = false, lock_version = 3,
                default_tab = 'issues' WHERE project_id = $1",
    )
    .bind(full)
    .bind(Value::Array(tabs.clone()))
    .execute(&pool)
    .await
    .unwrap();
    let epochs_before: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();

    db::MIGRATOR.run_to(21, &pool).await.unwrap();

    let builtins = [
        "timeline",
        "milestones",
        "issues",
        "dev",
        "plan",
        "board",
        "triage",
    ];
    let (t, is_default, lv2) = settings(&pool, plain).await;
    assert_eq!(ids(&t), builtins);
    assert!(is_default);
    assert_eq!(lv2, lv + 1);
    assert_eq!(t[5]["title"], "看板");
    assert_eq!(t[6]["hidden"], true);
    assert_eq!(t[5]["hidden"], false);

    let (t, is_default, lv2) = settings(&pool, full).await;
    let got = ids(&t);
    assert_eq!(got.len(), 17);
    assert_eq!(got[..5], builtins[..5]);
    assert_eq!(got[5..15], ids(&Value::Array(tabs[5..].to_vec()))[..]);
    assert_eq!(got[15..], ["board", "triage"]);
    assert!(!is_default);
    assert_eq!(lv2, 4);
    let default_tab: String =
        sqlx::query_scalar("SELECT default_tab FROM project_tab_settings WHERE project_id = $1")
            .bind(full)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(default_tab, "issues");

    // A new project gets the seven; the CHECK is 7–17.
    let fresh = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('FRESH','Fresh') RETURNING id",
    )
    .await;
    let (t, _, lv3) = settings(&pool, fresh).await;
    assert_eq!(ids(&t), builtins);
    assert_eq!(lv3, 0);
    let too_few =
        sqlx::query("UPDATE project_tab_settings SET tabs = tabs - 6 - 5 WHERE project_id = $1")
            .bind(fresh)
            .execute(&pool)
            .await;
    assert!(too_few.is_err());
    let mut eighteen = got_tabs(&pool, full).await;
    eighteen.push(
        json!({"id": "l_9999999999", "kind": "link", "title": "x", "hidden": false,
                         "url": "https://x.test/x"}),
    );
    let too_many = sqlx::query("UPDATE project_tab_settings SET tabs = $2 WHERE project_id = $1")
        .bind(full)
        .bind(Value::Array(eighteen))
        .execute(&pool)
        .await;
    assert!(too_many.is_err());

    // The upgrade changes no security epoch.
    let epochs_after: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects WHERE key <> 'FRESH' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(epochs_after, epochs_before, "no epoch bump");
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_tables WHERE tablename IN
           ('view_defs', 'view_def_events', 'view_def_event_redactions')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tables, 3);

    pool.close().await;
    recreate(&base, &name, false).await;
}

async fn got_tabs(pool: &PgPool, p: Uuid) -> Vec<Value> {
    settings(pool, p).await.0.as_array().unwrap().clone()
}
