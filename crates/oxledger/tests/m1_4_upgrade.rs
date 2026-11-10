// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.4: upgrading to time tracking (0023). Runs migrations
//! 0001–0022 on a temporary database `<test db>_upg14`, writes two projects,
//! the five built-in roles the way an installation has them (Manager,
//! Developer, Reporter, Non member, Anonymous, plus a custom role), and an
//! agent delegation, then runs 0023:
//! - every project's `security_epoch` goes up exactly once (the one
//!   statement that grants the new permissions; enabling the module does not
//!   bump it again);
//! - Manager gets the seven M1 permissions, Developer and Reporter
//!   `view_time_entries` and `log_time`, the built-in Non member and
//!   Anonymous roles and the custom role get none;
//! - every project has the time module and a time settings row;
//! - the delegation keeps its proposal ops and policies, `read_own_time`
//!   off;
//! - the work context is quarantined by the epoch change (it must be
//!   restarted).
//!
//! - every role keeps `time_entries_visibility = 'all'` (Redmine's default).
//!
//! A second test installs from scratch (`<test db>_upg14_fresh`: all
//! migrations, then `seed-demo`) and checks the built-in roles get the same.
//! The temporary databases are dropped. Needs PostgreSQL 18.

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
    (base.to_string(), format!("{name}_upg14"))
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

async fn perms(pool: &PgPool, role: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT permission FROM role_permissions WHERE role_id = $1
           AND permission IN ('view_time_entries','log_time','edit_time_entries','edit_own_time_entries',
                              'log_time_for_other_users','triage_issues','manage_public_views')
         ORDER BY permission",
    )
    .bind(role)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn visibility(pool: &PgPool, role: Uuid) -> String {
    sqlx::query_scalar("SELECT time_entries_visibility FROM roles WHERE id = $1")
        .bind(role)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// What 0023 grants by role name (M1.4-D1), for the upgrade and for a new
/// installation (`seed-demo` grants the same).
fn expected(role: &str) -> Vec<&'static str> {
    match role {
        "Manager" => vec![
            "edit_own_time_entries",
            "edit_time_entries",
            "log_time",
            "log_time_for_other_users",
            "manage_public_views",
            "triage_issues",
            "view_time_entries",
        ],
        "Developer" | "Reporter" => vec!["log_time", "view_time_entries"],
        _ => vec![],
    }
}

/// A new installation: the migrations run on an empty database (0023 has
/// no roles to grant to), then `seed-demo` creates the roles; they end up
/// with what 0023 gives existing roles of the same names.
#[tokio::test]
async fn m1_4_fresh_install_matches_migration() {
    let (base, name) = target();
    let name = format!("{name}_fresh");
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    oxledger::seed::seed_demo(&pool).await.unwrap();
    let roles: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM roles ORDER BY position")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(roles.len(), 5);
    for (id, n) in &roles {
        assert_eq!(perms(&pool, *id).await, expected(n), "{n}");
        assert_eq!(visibility(&pool, *id).await, "all", "{n}");
    }
    pool.close().await;
    recreate(&base, &name, false).await;
}

#[tokio::test]
async fn m1_4_upgrade_time() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(22, &pool).await.unwrap();
    let user = one(
        &pool,
        "INSERT INTO principals (kind, name, login) VALUES ('user','u','u') RETURNING id",
    )
    .await;
    let p1 = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OLD','Old') RETURNING id",
    )
    .await;
    let p2 = one(
        &pool,
        "INSERT INTO projects (key, name, is_public) VALUES ('PUB','Public', true) RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects")
        .execute(&pool)
        .await
        .unwrap();
    let manager = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Manager','all') RETURNING id",
    )
    .await;
    let developer = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Developer','default') RETURNING id",
    )
    .await;
    let reporter = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','default') RETURNING id",
    )
    .await;
    let non_member = one(&pool, "INSERT INTO roles (name, issues_visibility, builtin) VALUES ('Non member','default','non_member') RETURNING id").await;
    let anonymous = one(&pool, "INSERT INTO roles (name, issues_visibility, builtin) VALUES ('Anonymous','default','anonymous') RETURNING id").await;
    let custom = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('QA','default') RETURNING id",
    )
    .await;
    // An installation that already gave Manager manage_public_views (M1.2).
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission) VALUES ($1, 'manage_public_views')",
    )
    .bind(manager)
    .execute(&pool)
    .await
    .unwrap();
    let m: Uuid = sqlx::query_scalar(
        "INSERT INTO members (project_id, principal_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(p1)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
        .bind(m)
        .bind(manager)
        .execute(&pool)
        .await
        .unwrap();

    // An agent with a delegation made before 0023.
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
    let epoch_p1: i64 = sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(p1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let delegation: Uuid = sqlx::query_scalar(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id,
                                  security_epoch, proposal_ops, writable_fields, expires_at)
         VALUES ($1, $2, $3, $4, $5, ARRAY['create_issue','add_note','triage_decide'],
                 ARRAY['subject'], now() + interval '7 days') RETURNING id",
    )
    .bind(agent)
    .bind(user)
    .bind(context)
    .bind(p1)
    .bind(epoch_p1)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delegation_command_policies (delegation_id, command_kind)
         SELECT $1, unnest(ARRAY['create_issue','add_note','triage_decide'])",
    )
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_context_source_projects (context_id, project_id, security_epoch, first_delegation_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(context)
    .bind(p1)
    .bind(epoch_p1)
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();

    let before: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT id, security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();

    db::MIGRATOR.run(&pool).await.unwrap();

    // Exactly one bump per project.
    let after: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT id, security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    for ((id, b), (_, a)) in before.iter().zip(&after) {
        assert_eq!(
            *a,
            b + 1,
            "project {id}: epoch {b} → {a}, expected one bump"
        );
    }

    // Redmine's defaults (lib/redmine/default_data/loader.rb) plus the two
    // oxledger permissions for Manager.
    assert_eq!(perms(&pool, manager).await, expected("Manager"));
    assert_eq!(perms(&pool, developer).await, expected("Developer"));
    assert_eq!(perms(&pool, reporter).await, expected("Reporter"));
    for r in [non_member, anonymous, custom] {
        assert!(perms(&pool, r).await.is_empty());
    }
    // Redmine's default: every role sees all time entries of its projects.
    for r in [manager, developer, reporter, non_member, anonymous, custom] {
        assert_eq!(visibility(&pool, r).await, "all");
    }

    // The module and the settings rows.
    for p in [p1, p2] {
        let on: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM enabled_modules WHERE project_id = $1 AND module = 'time_tracking')",
        )
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(on);
        let lv: i32 = sqlx::query_scalar(
            "SELECT lock_version FROM project_time_settings WHERE project_id = $1",
        )
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lv, 0);
    }
    // A project created afterwards gets its settings row too.
    let p3 = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('NEW','New') RETURNING id",
    )
    .await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM project_time_settings WHERE project_id = $1")
            .bind(p3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);

    // The delegation is unchanged: no time commands, read_own_time off.
    let (ops, read_own): (Vec<String>, bool) =
        sqlx::query_as("SELECT proposal_ops, read_own_time FROM delegations WHERE id = $1")
            .bind(delegation)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ops, vec!["create_issue", "add_note", "triage_decide"]);
    assert!(!read_own);
    let policies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delegation_command_policies WHERE delegation_id = $1",
    )
    .bind(delegation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(policies, 3);
    // The work context reads from a project whose epoch moved: quarantined.
    let stale: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agent_context_source_projects r JOIN projects p ON p.id = r.project_id
                        WHERE r.context_id = $1 AND p.security_epoch <> r.security_epoch)",
    )
    .bind(context)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stale, "agent work contexts must be restarted after 0023");
    // The default activities.
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM time_entry_activities ORDER BY position")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(names, vec!["開發", "設計", "測試", "會議", "其他"]);

    pool.close().await;
    recreate(&base, &name, false).await;
}
