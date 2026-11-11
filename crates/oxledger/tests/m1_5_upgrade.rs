// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.5: upgrading to forms and field management (0024, 0025).
//! Runs migrations 0001–0023 on a temporary database `<test db>_upg15`,
//! writes a project, a tracker, two custom fields (one list with options)
//! and an agent work context with a delegation, then runs 0024 and 0025:
//! - no project's `security_epoch` moves (no role, permission or membership
//!   change), so the work context stays usable;
//! - existing fields get `lock_version = 0` and are active;
//! - `form_l0_version` starts at 1; the forms tables are empty;
//! - afterwards the L0 triggers move `form_l0_version` for what changes a
//!   write's requirements (required, deactivation, options) and not for a
//!   rename, and a form row moves `schema_version`.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
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
    (base.to_string(), format!("{name}_upg15"))
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
async fn m1_5_upgrade_forms() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(23, &pool).await.unwrap();
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
    let status = one(
        &pool,
        "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id",
    )
    .await;
    let tracker: Uuid = sqlx::query_scalar(
        "INSERT INTO trackers (name, position, default_status_id) VALUES ('Bug', 1, $1) RETURNING id",
    )
    .bind(status)
    .fetch_one(&pool)
    .await
    .unwrap();
    let list = int(
        &pool,
        "INSERT INTO field_defs (name, ftype) VALUES ('Severity','list') RETURNING id::bigint",
    )
    .await;
    let text = int(
        &pool,
        "INSERT INTO field_defs (name, ftype, index_state) VALUES ('Notes','text','none') RETURNING id::bigint",
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO field_options (field_id, label, position) VALUES ({list}, 'Low', 1), ({list}, 'High', 2);
             INSERT INTO field_trackers VALUES ({list}, '{tracker}'), ({text}, '{tracker}')"
        ),
    )
    .await;

    // An agent work context with a delegation (made before 0024).
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
    let epoch: i64 = sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
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

    db::MIGRATOR.run_to(25, &pool).await.unwrap();

    // No epoch move: the context is still current for the project.
    let after: i64 = sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, epoch, "0024/0025 must not bump security_epoch");
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
        schema_before,
        "creating the tables changes no schema_version"
    );
    // Existing fields: active, lock_version 0.
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM field_defs WHERE lock_version = 0 AND deactivated_at IS NULL"
        )
        .await,
        2
    );
    assert_eq!(
        int(&pool, "SELECT form_l0_version FROM org_settings").await,
        1
    );
    for t in [
        "form_defs",
        "form_rule_versions",
        "form_layout_versions",
        "form_rule_marks",
        "custom_field_conversions",
    ] {
        assert_eq!(
            int(&pool, &format!("SELECT count(*) FROM {t}")).await,
            0,
            "{t}"
        );
    }

    // The L0 counter: a rename does not move it; required, deactivation
    // and an option's activity do.
    let l0 = || int(&pool, "SELECT form_l0_version FROM org_settings");
    exec(
        &pool,
        &format!("UPDATE field_defs SET name = 'Sev' WHERE id = {list}"),
    )
    .await;
    assert_eq!(l0().await, 1);
    exec(
        &pool,
        &format!("UPDATE field_defs SET is_required = true WHERE id = {list}"),
    )
    .await;
    assert_eq!(l0().await, 2);
    exec(
        &pool,
        &format!("UPDATE field_defs SET deactivated_at = now() WHERE id = {text}"),
    )
    .await;
    assert_eq!(l0().await, 3);
    exec(
        &pool,
        &format!(
            "UPDATE field_options SET label = 'Lowest' WHERE field_id = {list} AND label = 'Low'"
        ),
    )
    .await;
    assert_eq!(l0().await, 3);
    exec(
        &pool,
        &format!(
            "UPDATE field_options SET active = false WHERE field_id = {list} AND label = 'High'"
        ),
    )
    .await;
    assert_eq!(l0().await, 4);
    // The epoch stays: none of these is a visibility change.
    let still: i64 = sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still, epoch);
    // Each other L0 change moves the counter on its own (some of these
    // also move the epoch through 0010; that is not checked here).
    let role = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Lead','all') RETURNING id",
    )
    .await;
    for (what, sql) in [
        (
            "a field role",
            format!("INSERT INTO field_roles (field_id, role_id) VALUES ({list}, '{role}')"),
        ),
        (
            "a field project",
            format!("INSERT INTO field_projects (field_id, project_id) VALUES ({list}, '{p}')"),
        ),
        (
            "is_for_all",
            format!("UPDATE field_defs SET is_for_all = NOT is_for_all WHERE id = {list}"),
        ),
        (
            "a field tracker",
            format!("DELETE FROM field_trackers WHERE field_id = {text}"),
        ),
        (
            "visibility",
            format!("UPDATE field_defs SET visible = false WHERE id = {list}"),
        ),
        (
            "an option added",
            format!(
                "INSERT INTO field_options (field_id, label, position) VALUES ({list}, 'Mid', 3)"
            ),
        ),
        (
            "an option deleted",
            format!("DELETE FROM field_options WHERE field_id = {list} AND label = 'Mid'"),
        ),
        (
            "dropping",
            format!("UPDATE field_defs SET index_state = 'dropping' WHERE id = {text}"),
        ),
    ] {
        let before = l0().await;
        exec(&pool, &sql).await;
        assert_eq!(l0().await, before + 1, "{what}");
    }
    // A form row moves describe's schema_version.
    let s0 = int(&pool, "SELECT schema_version FROM org_settings").await;
    sqlx::query("INSERT INTO form_defs (tracker_id) VALUES ($1)")
        .bind(tracker)
        .execute(&pool)
        .await
        .unwrap();
    assert!(int(&pool, "SELECT schema_version FROM org_settings").await > s0);
    // One site form per tracker (NULLS NOT DISTINCT).
    let dup = sqlx::query("INSERT INTO form_defs (tracker_id) VALUES ($1)")
        .bind(tracker)
        .execute(&pool)
        .await;
    assert!(dup.is_err(), "a second site form of the tracker");

    pool.close().await;
    recreate(&base, &name, false).await;
}
