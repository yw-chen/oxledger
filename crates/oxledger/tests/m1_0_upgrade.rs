// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.0: upgrading a database that has data. Runs migrations
//! 0001–0018 (the migration files themselves, through `db::MIGRATOR`) on a
//! temporary database `<test db>_upg`, writes pre-M1 rows, then runs the rest
//! (0019, 0020) and checks the `issue_field_changes` backfill (a redacted
//! event is not backfilled) and the NOT VALID branch of
//! `role_permissions_tracker_scope`. The temporary database is dropped.
//! Needs PostgreSQL 18; see `common::test_pool`.

use oxledger::db;
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
    (base.to_string(), format!("{name}_upg"))
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

#[tokio::test]
async fn m1_0_upgrade_with_data() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    // ------------------------------------------------ the schema before M1
    db::MIGRATOR.run_to(18, &pool).await.unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 18);

    let sys = Uuid::parse_str("00000000-0000-7000-8000-000000000001").unwrap();
    let new = one(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('New','todo',1) RETURNING id").await;
    let doing = one(&pool, "INSERT INTO issue_statuses (name, category, position) VALUES ('Doing','doing',2) RETURNING id").await;
    let bug = one(
        &pool,
        "INSERT INTO trackers (name, position) VALUES ('Bug', 1) RETURNING id",
    )
    .await;
    let prio = one(&pool, "SELECT id FROM issue_priorities WHERE is_default").await;
    let prio2 = one(
        &pool,
        "INSERT INTO issue_priorities (name, position) VALUES ('M1 upgrade test', 99) RETURNING id",
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
    // A role whose non-issue permission is limited to a tracker (legal before 0019).
    let role = one(
        &pool,
        "INSERT INTO roles (name) VALUES ('Legacy') RETURNING id",
    )
    .await;
    sqlx::query("INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ($1, 'manage_versions', false)")
        .bind(role)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO role_permission_trackers VALUES ($1, 'manage_versions', $2)")
        .bind(role)
        .bind(bug)
        .execute(&pool)
        .await
        .unwrap();

    let issue = |subject: &'static str, n: i32| {
        let pool = pool.clone();
        async move {
            let i: Uuid = sqlx::query_scalar(
                "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id, priority_id)
                 VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
            )
            .bind(p)
            .bind(bug)
            .bind(new)
            .bind(subject)
            .bind(user)
            .bind(prio)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ($1,$2,$3,$4)",
            )
            .bind(format!("OLD-{n}"))
            .bind(i)
            .bind(p)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
            i
        }
    };
    let i1 = issue("one", 1).await;
    let i2 = issue("two", 2).await;
    let event = |i: Uuid, seq: i64, kind: &'static str, payload: serde_json::Value| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                           system_operation, payload)
                 VALUES ($1, $2, $3, 'import', $4, 'system', 'm1_upgrade_test', $5) RETURNING id",
            )
            .bind(i)
            .bind(seq)
            .bind(kind)
            .bind(sys)
            .bind(payload)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    use serde_json::json;
    let c1 = event(
        i1,
        1,
        "created",
        json!({"fields": {"subject": "one", "status_id": new, "priority_id": prio,
                          "assigned_to_id": user, "is_private": false, "cf:3": {"int": "7"}}}),
    )
    .await;
    let f1 = event(
        i1,
        2,
        "fields_changed",
        json!({"changes": {"subject": {"old": "one", "new": "uno"},
                           "assigned_to_id": {"old": user, "new": null},
                           "priority_id": {"old": prio, "new": prio2},
                           "cf:3": {"old": {"int": "7"}, "new": {"int": "8"}}}}),
    )
    .await;
    let s1 = event(i1, 3, "status_changed", json!({"old": new, "new": doing})).await;
    let d1 = event(
        i1,
        4,
        "duplicate_added",
        json!({"role": "source", "other_issue_id": i2}),
    )
    .await;
    let r1 = event(
        i1,
        5,
        "duplicate_removed",
        json!({"role": "source", "other_issue_id": i2, "relation_id": Uuid::now_v7()}),
    )
    .await;
    // A subject-only change: no projection rows.
    let f_subject = event(
        i2,
        1,
        "fields_changed",
        json!({"changes": {"subject": {"old": "two", "new": "dos"}}}),
    )
    .await;
    // A redacted status change: not backfilled.
    let s2 = event(i2, 2, "status_changed", json!({"old": new, "new": doing})).await;
    let red: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_event_redactions (event_id, redacted_by, reason_code, reason)
         VALUES ($1, $2, 'other', 'test') RETURNING id",
    )
    .bind(s2)
    .bind(sys)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE issue_events SET payload = to_jsonb('[已遮蔽]'::text), redaction_id = $2 WHERE id = $1")
        .bind(s2)
        .bind(red)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET last_event_seq = 5 WHERE id = $1")
        .bind(i1)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE issues SET last_event_seq = 2 WHERE id = $1")
        .bind(i2)
        .execute(&pool)
        .await
        .unwrap();
    let epochs_before: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();

    // ------------------------------------------------ 0019, 0020 (the files)
    db::MIGRATOR.run(&pool).await.unwrap();
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version > 18 ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(applied.starts_with(&[19, 20]), "{applied:?}");

    // The backfill.
    type Row = (Uuid, Uuid, String, Option<Uuid>, Option<Uuid>, bool);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.event_id, c.issue_id, c.field_key, c.old_ref, c.new_ref, c.at = e.at
         FROM issue_field_changes c JOIN issue_events e ON e.id = c.event_id
         ORDER BY e.issue_id = $1 DESC, e.seq, c.field_key",
    )
    .bind(i1)
    .fetch_all(&pool)
    .await
    .unwrap();
    let r = |e: Uuid, i: Uuid, k: &str, o: Option<Uuid>, n: Option<Uuid>| -> Row {
        (e, i, k.to_string(), o, n, true)
    };
    assert_eq!(
        rows,
        [
            r(c1, i1, "assigned_to_id", None, Some(user)),
            r(c1, i1, "priority_id", None, Some(prio)),
            r(c1, i1, "status_id", None, Some(new)),
            r(f1, i1, "assigned_to_id", Some(user), None),
            r(f1, i1, "priority_id", Some(prio), Some(prio2)),
            r(s1, i1, "status_id", Some(new), Some(doing)),
        ]
    );
    for e in [d1, r1, f_subject, s2] {
        assert!(rows.iter().all(|x| x.0 != e), "{e} has projection rows");
    }

    // The tracker scope stays NOT VALID; the legacy row is kept as it was.
    let (valid,): (bool,) = sqlx::query_as(
        "SELECT convalidated FROM pg_constraint WHERE conname = 'role_permissions_tracker_scope'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !valid,
        "existing tracker-limited manage_versions: left NOT VALID"
    );
    let kept: (bool, i64) = sqlx::query_as(
        "SELECT rp.all_trackers,
                (SELECT count(*) FROM role_permission_trackers t
                 WHERE t.role_id = rp.role_id AND t.permission = rp.permission)
         FROM role_permissions rp WHERE rp.role_id = $1 AND rp.permission = 'manage_versions'",
    )
    .bind(role)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, (false, 1));
    // New rows are still checked.
    let refused = sqlx::query(
        "INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ($1, 'log_time', false)",
    )
    .bind(role)
    .execute(&pool)
    .await;
    assert!(refused.is_err());
    // No epoch bump from the upgrade.
    let epochs_after: Vec<i64> =
        sqlx::query_scalar("SELECT security_epoch FROM projects ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(epochs_after, epochs_before);
    let hours: Option<String> =
        sqlx::query_scalar("SELECT estimated_hours::text FROM issues WHERE id = $1")
            .bind(i1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hours, None);

    pool.close().await;
    recreate(&base, &name, false).await;
}
