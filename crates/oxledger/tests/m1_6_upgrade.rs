// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.6: upgrading to personal preferences (0026). Runs
//! migrations 0001–0025 on a temporary database `<test db>_upg16`, writes
//! a user, a project and an agent work context with a delegation, then
//! runs 0026:
//! - no project's `security_epoch` moves and no `schema_version` either
//!   (no role, permission or membership change), so the work context stays
//!   usable;
//! - `user_preferences` is empty (rows are created on first read) and its
//!   defaults are the contract's (dark, comfortable, no roles, not
//!   onboarded, lock versions 0);
//! - only users can have a row (trigger), and the CHECKs hold.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

const DEFAULT_ADMIN_URL: &str = "postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger";

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
    (base.to_string(), format!("{name}_upg16"))
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

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn m1_6_upgrade_preferences() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(25, &pool).await.unwrap();
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
    // An agent work context with a delegation (made before 0026).
    let client = one(
        &pool,
        "INSERT INTO agent_clients (name, enabled, egress_policy_ref) VALUES ('Old Agent', true, 'p') RETURNING id",
    )
    .await;
    let agent = one(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('agent','Old Agent (u)') RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id, handle) VALUES ($1, $2, $3, 'old-agent-u')")
        .bind(agent)
        .bind(user)
        .bind(client)
        .execute(&pool)
        .await
        .unwrap();
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO browser_sessions (user_id, token_sha256, csrf_secret, credential_version,
                                       idle_expires_at, absolute_expires_at)
         VALUES ($1, sha256('t'::bytea), sha256('c'::bytea), 0, now() + interval '1 hour',
                 now() + interval '1 day') RETURNING id",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    let context: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_work_contexts (agent_principal_id, fresh_context_ack, created_by_session)
         VALUES ($1, true, $2) RETURNING id",
    )
    .bind(agent)
    .bind(session)
    .fetch_one(&pool)
    .await
    .unwrap();
    let epoch_sql = format!("SELECT security_epoch FROM projects WHERE id = '{p}'");
    let epoch = int(&pool, &epoch_sql).await;
    let delegation: Uuid = sqlx::query_scalar(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id,
                                  security_epoch, proposal_ops, writable_fields, expires_at)
         VALUES ($1, $2, $3, $4, $5, ARRAY['create_issue'], ARRAY['subject'], now() + interval '7 days')
         RETURNING id",
    )
    .bind(agent)
    .bind(user)
    .bind(context)
    .bind(p)
    .bind(epoch)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_context_source_projects (context_id, project_id, security_epoch, first_delegation_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(context)
    .bind(p)
    .bind(epoch)
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();
    let schema_before = int(&pool, "SELECT schema_version FROM org_settings").await;

    db::MIGRATOR.run_to(26, &pool).await.unwrap();

    assert_eq!(
        int(&pool, &epoch_sql).await,
        epoch,
        "0026 must not bump security_epoch"
    );
    assert_eq!(
        int(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_context_source_projects s JOIN projects p ON p.id = s.project_id
                 WHERE s.context_id = '{context}' AND s.security_epoch = p.security_epoch"
            )
        )
        .await,
        1
    );
    assert_eq!(
        int(&pool, "SELECT schema_version FROM org_settings").await,
        schema_before
    );
    assert_eq!(int(&pool, "SELECT count(*) FROM user_preferences").await, 0);

    // Defaults of a new row.
    exec(
        &pool,
        &format!(
            "INSERT INTO user_preferences (user_id, home) VALUES ('{user}', '{{\"widgets\": []}}')"
        ),
    )
    .await;
    type Defaults = (Vec<String>, bool, String, String, Value, Value, i32, i32);
    let d: Defaults = sqlx::query_as(
        "SELECT interface_roles, onboarded_at IS NOT NULL, theme, density, nav_shortcuts,
                default_views, lock_version, home_lock_version FROM user_preferences",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(d.0.is_empty() && !d.1);
    assert_eq!((d.2.as_str(), d.3.as_str()), ("dark", "comfortable"));
    assert_eq!((d.4, d.5), (json!([]), json!({})));
    assert_eq!((d.6, d.7), (0, 0));
    // CHECKs and the user-only trigger.
    for bad in [
        format!(
            "UPDATE user_preferences SET interface_roles = ARRAY['boss'] WHERE user_id = '{user}'"
        ),
        format!("UPDATE user_preferences SET theme = 'neon' WHERE user_id = '{user}'"),
        format!("UPDATE user_preferences SET density = 'tiny' WHERE user_id = '{user}'"),
        format!("UPDATE user_preferences SET home = '[]' WHERE user_id = '{user}'"),
        format!("UPDATE user_preferences SET nav_shortcuts = '{{}}' WHERE user_id = '{user}'"),
        format!("UPDATE user_preferences SET default_views = '[]' WHERE user_id = '{user}'"),
        format!("INSERT INTO user_preferences (user_id, home) VALUES ('{agent}', '{{}}')"),
        format!("UPDATE user_preferences SET user_id = '{agent}' WHERE user_id = '{user}'"),
    ] {
        assert!(
            sqlx::raw_sql(AssertSqlSafe(bad.clone()))
                .execute(&pool)
                .await
                .is_err(),
            "{bad}"
        );
    }
    exec(
        &pool,
        &format!(
            "UPDATE user_preferences SET interface_roles = ARRAY['pm','engineer'] WHERE user_id = '{user}'"
        ),
    )
    .await;
    // Writing preferences moves no epoch either.
    assert_eq!(int(&pool, &epoch_sql).await, epoch);

    pool.close().await;
    recreate(&base, &name, false).await;
}
