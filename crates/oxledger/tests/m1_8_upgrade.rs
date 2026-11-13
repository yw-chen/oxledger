// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.8: upgrading to notifications (0028). Runs migrations
//! 0001–0027 on a temporary database `<test db>_upg18`, writes a project,
//! a person who may read it and an issue with 30 events the way an import
//! did before 0028, then runs 0028:
//! - no project's `security_epoch` moves (no permission change);
//! - the `notify` cursor exists (active) and the issue's progress row is
//!   its last event (30): T-M1-77, events before the upgrade are not sent;
//! - after the upgrade (then the later migrations, which today's dispatcher
//!   needs) a watcher gets the 31st event only, even when the cursor is
//!   rewound to the start of the outbox.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::{db, notifications};
use serde_json::json;
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
    (base.to_string(), format!("{name}_upg18"))
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

async fn event(
    pool: &PgPool,
    issue: Uuid,
    seq: i64,
    kind: &str,
    payload: serde_json::Value,
) -> Uuid {
    let sys = Uuid::parse_str("00000000-0000-7000-8000-000000000001").unwrap();
    sqlx::query_scalar(
        "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                   system_operation, payload)
         VALUES ($1, $2, $3, 'import', $4, 'system', 'm1_8_upgrade_test', $5) RETURNING id",
    )
    .bind(issue)
    .bind(seq)
    .bind(kind)
    .bind(sys)
    .bind(payload)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn m1_8_upgrade_notifications() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(27, &pool).await.unwrap();
    let new = one(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let bug = one(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('Bug', 1) RETURNING id",
    )
    .await;
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
    let role = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO role_permissions (role_id, permission) VALUES ('{role}', 'view_issues');
         INSERT INTO enabled_modules VALUES ('{p}', 'issue_tracking');
         INSERT INTO project_trackers VALUES ('{p}', '{bug}');"
    )))
    .await
    .unwrap();
    let m = one(
        &pool,
        &format!(
            "INSERT INTO members (project_id, principal_id) VALUES ('{p}', '{user}') RETURNING id"
        ),
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO member_roles VALUES ('{m}', '{role}')"
    )))
    .await
    .unwrap();
    let i = one(
        &pool,
        &format!(
            "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id)
             VALUES ('{p}', '{bug}', '{new}', 's30', '{user}') RETURNING id"
        ),
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ('OLD-1', '{i}', '{p}', 1);
         UPDATE projects SET issue_counter = 1 WHERE id = '{p}';"
    )))
    .await
    .unwrap();
    event(
        &pool,
        i,
        1,
        "created",
        json!({"fields": {"subject": "s1", "status_id": new, "is_private": false}}),
    )
    .await;
    for seq in 2..=30 {
        event(
            &pool,
            i,
            seq,
            "fields_changed",
            json!({"changes": {"subject": {"old": format!("s{}", seq - 1), "new": format!("s{seq}")}}}),
        )
        .await;
    }
    let epoch_sql = format!("SELECT security_epoch::bigint FROM projects WHERE id = '{p}'");
    let epoch: i64 = sqlx::query_scalar(AssertSqlSafe(epoch_sql.clone()))
        .fetch_one(&pool)
        .await
        .unwrap();

    db::MIGRATOR.run_to(28, &pool).await.unwrap();

    let after: i64 = sqlx::query_scalar(AssertSqlSafe(epoch_sql))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, epoch, "0028 changes no permission");
    let cursor: (String, String) = sqlx::query_as(
        "SELECT state, last_xid::text FROM outbox_cursors WHERE consumer = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursor.0, "active");
    let progress: i64 = sqlx::query_scalar(
        "SELECT last_event_seq FROM notification_issue_progress WHERE issue_id = $1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(progress, 30, "backfilled to the last event");
    // The dispatcher below is this build's: it needs this build's schema
    // (M2.1 reads issue_event_mentions and on_mentioned from 0032).
    db::MIGRATOR.run(&pool).await.unwrap();

    // A watcher after the upgrade: the 31st event only.
    sqlx::query("INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ($1, $2, 'manual')")
        .bind(i)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    // Even the outbox messages from before the upgrade (cursor at zero)
    // send nothing old.
    pool.execute("UPDATE outbox_cursors SET last_xid = '0', last_id = 0 WHERE consumer = 'notify'")
        .await
        .unwrap();
    let e31 = event(
        &pool,
        i,
        31,
        "fields_changed",
        json!({"changes": {"subject": {"old": "s30", "new": "s31"}}}),
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "UPDATE issues SET subject = 's31' WHERE id = '{i}'"
    )))
    .await
    .unwrap();
    notifications::dispatch_all(&pool).await.unwrap();
    let rows: Vec<(Uuid, Vec<Uuid>)> =
        sqlx::query_as("SELECT recipient_id, event_ids FROM notifications")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(user, vec![e31])]);

    pool.close().await;
    recreate(&base, &name, false).await;
}
