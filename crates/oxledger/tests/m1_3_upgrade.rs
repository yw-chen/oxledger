// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.3: upgrading to Triage. Runs migrations 0001–0021 on a
//! temporary database `<test db>_upg13`, writes a project with an issue and
//! an agent delegation (five proposal ops, their command policies), then
//! runs 0022: the project gets a disabled Triage settings row, the existing
//! issue does not enter the inbox, the delegation does not gain
//! `triage_decide` (neither as a proposal op nor as a command policy), no
//! role gets `triage_issues`, and no `security_epoch` changes. The
//! temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

mod common;

/// (this test's server, temporary database name).
fn target() -> (common::TestDb, String) {
    let db = common::test_db();
    (db.clone(), db.scratch("upg13"))
}

async fn recreate(base: &common::TestDb, name: &str, create: bool) {
    let mut admin = PgConnection::connect(&base.admin_url())
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

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn m1_3_upgrade_triage() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();

    db::MIGRATOR.run_to(21, &pool).await.unwrap();
    let new = one(
        &pool,
        "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id",
    )
    .await;
    let bug = one(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('Bug', 1) RETURNING id",
    )
    .await;
    let prio = one(&pool, "SELECT id FROM issue_priorities WHERE is_default").await;
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
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id, priority_id)
         VALUES ($1,$2,$3,'old one',$4,$5) RETURNING id",
    )
    .bind(p)
    .bind(bug)
    .bind(new)
    .bind(user)
    .bind(prio)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ('OLD-1',$1,$2,1)",
    )
    .bind(issue)
    .bind(p)
    .execute(&pool)
    .await
    .unwrap();

    // An agent with a delegation made before 0022.
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
    let delegation: Uuid = sqlx::query_scalar(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id,
                                  security_epoch, proposal_ops, writable_fields, expires_at)
         VALUES ($1, $2, $3, $4, 0,
                 ARRAY['create_issue','update_fields','transition','add_note','duplicate_of'],
                 ARRAY['subject'], now() + interval '7 days') RETURNING id",
    )
    .bind(agent)
    .bind(user)
    .bind(context)
    .bind(p)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delegation_command_policies (delegation_id, command_kind)
         SELECT $1, unnest(ARRAY['create_issue','update_fields','transition','add_note','duplicate_of'])",
    )
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();

    let epochs_before: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let grants_before = count(&pool, "SELECT count(*) FROM role_permissions").await;

    db::MIGRATOR.run_to(22, &pool).await.unwrap();

    // Settings: one row per project, Triage off, the default rule.
    let (enabled, rule, lv): (bool, String, i32) = sqlx::query_as(
        "SELECT enabled, entry_rule, lock_version FROM project_triage_settings WHERE project_id = $1",
    )
    .bind(p)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!enabled);
    assert_eq!(rule, "creator_without_triage");
    assert_eq!(lv, 0);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM project_triage_settings").await,
        count(&pool, "SELECT count(*) FROM projects").await
    );
    // The existing issue is outside Triage.
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_triage").await, 0);
    // The delegation is not extended.
    let ops: Vec<String> = sqlx::query_scalar("SELECT proposal_ops FROM delegations WHERE id = $1")
        .bind(delegation)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!ops.contains(&"triage_decide".to_string()));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM delegation_command_policies WHERE command_kind = 'triage_decide'"
        )
        .await,
        0
    );
    // ... but may be, now.
    sqlx::query(
        "INSERT INTO delegation_command_policies (delegation_id, command_kind) VALUES ($1, 'triage_decide')",
    )
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();
    // No role gets triage_issues here (M1.4 grants it to Manager).
    assert_eq!(
        count(&pool, "SELECT count(*) FROM role_permissions").await,
        grants_before
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM role_permissions WHERE permission = 'triage_issues'"
        )
        .await,
        0
    );
    // A project created later gets its row too.
    let fresh = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('FRESH','Fresh') RETURNING id",
    )
    .await;
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM project_triage_settings WHERE project_id = '{fresh}' AND NOT enabled"
            )
        )
        .await,
        1
    );
    // The upgrade changes no security epoch.
    let epochs_after: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects WHERE key <> 'FRESH' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(epochs_after, epochs_before, "no epoch bump");

    pool.close().await;
    recreate(&base, &name, false).await;
}
