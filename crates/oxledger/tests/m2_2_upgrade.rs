// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.2 (§2.1, §2.3, B-9): upgrading to migration 0034. Runs
//! migrations up to 0033 on a temporary database `<test db>_upgm22`,
//! writes a project with saved tabs and a role, then runs 0034: no
//! project's `security_epoch` moved, every existing project got its
//! `wiki_settings` row (and a project created later gets one from the
//! trigger), saved tabs got the `wiki` tab appended with a new lock
//! version, the wiki module stayed off and no role changed.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

mod common;

/// `<test db>_<suffix>` (as in m2_0_upgrade.rs).
fn target_named(suffix: &str) -> (common::TestDb, String) {
    let db = common::test_db();
    (db.clone(), db.scratch(suffix))
}

async fn recreate(base: &common::TestDb, name: &str, create: bool) {
    let mut admin = PgConnection::connect(&base.admin_url())
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

/// Drops the temporary database on every exit, a panic included.
struct DropDb {
    base: common::TestDb,
    name: String,
}

impl Drop for DropDb {
    fn drop(&mut self) {
        let (base, name) = (self.base.clone(), self.name.clone());
        let done = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(recreate(&base, &name, false));
        })
        .join();
        if done.is_err() {
            eprintln!("could not drop the temporary database {}", self.name);
        }
    }
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

async fn text(pool: &PgPool, sql: &str) -> String {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn upgrade_0034_wiki_settings_and_tab_without_epoch() {
    let (base, name) = target_named("upgm22");
    recreate(&base, &name, true).await;
    let _drop = DropDb {
        base: base.clone(),
        name: name.clone(),
    };
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();
    db::MIGRATOR.run_to(33, &pool).await.unwrap();
    let p = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('OLD','Old') RETURNING id",
    )
    .await;
    let q = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('CUS','Custom') RETURNING id",
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO enabled_modules (project_id, module) VALUES ('{p}', 'issue_tracking');
         INSERT INTO project_tab_settings (project_id) VALUES ('{p}') ON CONFLICT DO NOTHING;
         INSERT INTO project_tab_settings (project_id) VALUES ('{q}') ON CONFLICT DO NOTHING;
         UPDATE project_tab_settings
            SET tabs = tabs || '[{{\"id\": \"c1\", \"kind\": \"view\", \"title\": \"Mine\", \"hidden\": false}}]'::jsonb,
                is_default = false, lock_version = 4
          WHERE project_id = '{q}'"
    )))
    .await
    .unwrap();
    pool.execute("INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all')")
        .await
        .unwrap();
    let epochs_before = text(
        &pool,
        "SELECT string_agg(key || ':' || security_epoch, ',' ORDER BY key) FROM projects",
    )
    .await;
    let perms_before = int(&pool, "SELECT count(*) FROM role_permissions").await;
    let lv_p = int(
        &pool,
        &format!("SELECT lock_version::bigint FROM project_tab_settings WHERE project_id = '{p}'"),
    )
    .await;
    assert_eq!(
        int(
            &pool,
            &format!(
                "SELECT jsonb_array_length(tabs)::bigint FROM project_tab_settings WHERE project_id = '{p}'"
            )
        )
        .await,
        7
    );

    db::MIGRATOR.run_to(34, &pool).await.unwrap();

    // §2.1: the upgrade moves no epoch and grants nothing.
    assert_eq!(
        text(
            &pool,
            "SELECT string_agg(key || ':' || security_epoch, ',' ORDER BY key) FROM projects",
        )
        .await,
        epochs_before
    );
    assert_eq!(
        int(&pool, "SELECT count(*) FROM role_permissions").await,
        perms_before
    );
    // L3: every existing project has its settings row.
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM projects p JOIN wiki_settings s ON s.project_id = p.id
             WHERE s.page_count = 0 AND s.start_page_id IS NULL",
        )
        .await,
        2
    );
    // B-9: the module stays off for existing projects.
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM enabled_modules WHERE module = 'wiki'"
        )
        .await,
        0
    );
    // R-T1: the tab is appended, the lock version moves on.
    for (project, len, lv) in [(p, 8, lv_p + 1), (q, 9, 5)] {
        assert_eq!(
            int(
                &pool,
                &format!(
                    "SELECT jsonb_array_length(tabs)::bigint FROM project_tab_settings WHERE project_id = '{project}'"
                )
            )
            .await,
            len
        );
        assert_eq!(
            text(
                &pool,
                &format!(
                    "SELECT tabs -> -1 ->> 'id' FROM project_tab_settings WHERE project_id = '{project}'"
                )
            )
            .await,
            "wiki"
        );
        assert_eq!(
            int(
                &pool,
                &format!(
                    "SELECT lock_version::bigint FROM project_tab_settings WHERE project_id = '{project}'"
                )
            )
            .await,
            lv
        );
    }
    // A project created after the upgrade gets its row from the trigger.
    let n = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('NEW','New') RETURNING id",
    )
    .await;
    assert_eq!(
        int(
            &pool,
            &format!("SELECT count(*) FROM wiki_settings WHERE project_id = '{n}'"),
        )
        .await,
        1
    );
    // C13: the uniqueness key folds width, case (ß too) and white space.
    assert_eq!(
        text(&pool, "SELECT wiki_title_key('Straße  Notes')").await,
        "strasse_notes"
    );
    assert_eq!(
        text(&pool, "SELECT wiki_title_key('ＡＢＣ　x')").await,
        "abc_x"
    );
    pool.close().await;
}
