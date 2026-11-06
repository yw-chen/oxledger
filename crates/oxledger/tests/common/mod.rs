// SPDX-License-Identifier: GPL-2.0-or-later
//! Shared helpers for the PostgreSQL integration tests.

use oxledger::db;
use sqlx::PgPool;

const DEFAULT_ADMIN_URL: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger";

/// Connects to the test database and runs migrations.
///
/// Uses OXLEDGER_TEST_DATABASE_URL if set, otherwise database `oxledger_test`
/// next to DATABASE_URL / the local docker default. The database is created if
/// it does not exist, so parallel work lines can each point at their own
/// (e.g. `.../oxledger_test_d14`) without setup.
pub async fn test_pool() -> PgPool {
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
    let admin = PgPool::connect(&format!("{base}/postgres"))
        .await
        .expect("PostgreSQL 18 not reachable; start the docker container from docs/playbook.md");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(name)
            .fetch_one(&admin)
            .await
            .unwrap();
    if !exists {
        // Checked above to be a plain identifier.
        let create = format!("CREATE DATABASE {name}");
        if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(create))
            .execute(&admin)
            .await
        {
            // Another test binary may have created it meanwhile.
            eprintln!("CREATE DATABASE {name}: {e}");
        }
    }
    admin.close().await;
    db::connect(&url).await.expect("connect test database")
}
