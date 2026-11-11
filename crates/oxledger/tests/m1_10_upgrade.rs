// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.10 (§5, migration 0030): upgrading to the built-in AI chat.
//! Runs migrations up to 0029 on a temporary database `<test db>_upg110`,
//! writes a project and a member and then runs 0030: no project's
//! `security_epoch` moved, the built-in client `oxledger-ai` exists
//! disabled, the AI is off with no provider, and the changeset origin
//! CHECK names `builtin_ai` next to `external_agent`.
//!
//! A second database `<test db>_upg110c` has an ordinary 02 client named
//! `oxledger-ai`: 0030 refuses with a message naming upgrade-notes.md and
//! changes nothing; once the client is renamed, 0030 goes through.
//!
//! The temporary databases are dropped. Needs PostgreSQL 18.

use oxledger::db;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger";

/// `<test db>_<suffix>` (as in m1_9_upgrade.rs).
fn target_named(suffix: &str) -> (String, String) {
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
    (base.to_string(), format!("{name}_{suffix}"))
}

async fn recreate(base: &str, name: &str, create: bool) {
    let mut admin = PgConnection::connect(&format!("{base}/postgres"))
        .await
        .expect("PostgreSQL 18 not reachable");
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

async fn int(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn upgrade_0030_moves_no_epoch() {
    let (base, name) = target_named("upg110");
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
    db::MIGRATOR.run_to(29, &pool).await.unwrap();
    let user = one(
        &pool,
        "INSERT INTO principals (kind, name, login) VALUES ('user','u','u') RETURNING id",
    )
    .await;
    let p = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OLD','Old') RETURNING id",
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO members (project_id, principal_id) VALUES ('{p}', '{user}')"
    )))
    .await
    .unwrap();
    let epochs = "SELECT coalesce(sum(security_epoch), 0)::int8 FROM projects";
    let before = int(&pool, epochs).await;
    let clients = int(&pool, "SELECT count(*) FROM agent_clients").await;

    db::MIGRATOR.run_to(30, &pool).await.unwrap();

    assert_eq!(
        int(&pool, epochs).await,
        before,
        "0030 changes no permission"
    );
    assert_eq!(
        int(&pool, "SELECT count(*) FROM agent_clients").await,
        clients + 1
    );
    let (enabled, builtin): (bool, bool) =
        sqlx::query_as("SELECT enabled, builtin FROM agent_clients WHERE name = 'oxledger-ai'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!enabled && builtin);
    let (on, provider): (bool, Option<Uuid>) =
        sqlx::query_as("SELECT enabled, active_provider_id FROM ai_settings")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!on && provider.is_none(), "off by default (P1)");
    assert_eq!(int(&pool, "SELECT count(*) FROM ai_providers").await, 0);
    // The origin CHECK accepts builtin_ai now and still refuses others.
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = 'changesets'::regclass AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%origin%'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(defs.iter().any(|d| d.contains("builtin_ai")), "{defs:?}");
    assert!(
        !defs.iter().any(|d| d.contains("'external_agent'")
            && !d.contains("builtin_ai")
            && d.contains("ANY")),
        "{defs:?}"
    );
    pool.close().await;
    recreate(&base, &name, false).await;
}

#[tokio::test]
async fn upgrade_0030_refuses_an_ordinary_client_named_oxledger_ai() {
    let (base, name) = target_named("upg110c");
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
    db::MIGRATOR.run_to(29, &pool).await.unwrap();
    pool.execute(
        "DELETE FROM agent_clients WHERE name = 'oxledger-ai';
         INSERT INTO agent_clients (name, enabled, egress_policy_ref) VALUES ('oxledger-ai', true, 'p')",
    )
    .await
    .unwrap();
    let e = db::MIGRATOR.run_to(30, &pool).await.unwrap_err();
    assert!(format!("{e:#}").contains("upgrade-notes.md"), "{e:#}");
    // A failed run can leave the migrator's advisory lock on a pooled
    // connection until it idles out; a server would exit here. Reconnect.
    pool.close().await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM pg_tables WHERE tablename = 'ai_settings'"
        )
        .await,
        0,
        "nothing of 0030 stays"
    );
    pool.execute("UPDATE agent_clients SET name = 'oxledger-ai (02)' WHERE name = 'oxledger-ai'")
        .await
        .unwrap();
    db::MIGRATOR.run_to(30, &pool).await.unwrap();
    assert_eq!(
        int(&pool, "SELECT count(*) FROM agent_clients WHERE name = 'oxledger-ai' AND builtin AND NOT enabled").await,
        1
    );
    pool.close().await;
    recreate(&base, &name, false).await;
}
