// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.0 (§2.1, §2.2, B-37, B-50): upgrading to migration 0032.
//! Runs migrations up to 0031 on a temporary database `<test db>_upgm20`,
//! writes projects, roles, an issue with a description and a note, then
//! runs 0032: no project's `security_epoch` moved, the new permissions went
//! to the roles §2.2 names (roles limited to their own issues get no wiki
//! reading), and existing text stays `plain` while new rows default to
//! `markdown`.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::db;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

mod common;

/// `<test db>_<suffix>` (as in m1_10_upgrade.rs).
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

/// Drops the temporary database on every exit, a panic included. `Drop`
/// cannot await inside the test's runtime, so it runs on its own thread.
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

async fn role(pool: &PgPool, name: &str, visibility: &str, perms: &[&str]) -> Uuid {
    let r: Uuid = sqlx::query_scalar(
        "INSERT INTO roles (name, issues_visibility) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(visibility)
    .fetch_one(pool)
    .await
    .unwrap();
    for p in perms {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(r)
            .bind(p)
            .execute(pool)
            .await
            .unwrap();
    }
    r
}

async fn perms_of(pool: &PgPool, r: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT permission FROM role_permissions WHERE role_id = $1 ORDER BY permission",
    )
    .bind(r)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn s(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|x| x.to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn upgrade_0032_grants_without_epoch() {
    let (base, name) = target_named("upgm20");
    recreate(&base, &name, true).await;
    // Dropped when the test ends, also when an assertion fails.
    let _drop = DropDb {
        base: base.clone(),
        name: name.clone(),
    };
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();
    db::MIGRATOR.run_to(31, &pool).await.unwrap();
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
    let dev = role(&pool, "Dev", "all", &["view_issues", "edit_issues"]).await;
    let reporter = role(&pool, "Reporter", "default", &["view_issues"]).await;
    let own = role(&pool, "Own", "own", &["view_issues", "edit_issues"]).await;
    let mgr = role(&pool, "Mgr", "all", &["view_issues", "manage_project"]).await;
    let none = role(&pool, "Nothing", "all", &["add_issues"]).await;
    let member = one(
        &pool,
        &format!(
            "INSERT INTO members (project_id, principal_id) VALUES ('{p}', '{user}') RETURNING id"
        ),
    )
    .await;
    for r in [dev, own] {
        sqlx::query("INSERT INTO member_roles (member_id, role_id) VALUES ($1, $2)")
            .bind(member)
            .bind(r)
            .execute(&pool)
            .await
            .unwrap();
    }
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id, priority_id)
         VALUES ($1,$2,$3,'old one','*not* markdown',$4,$5) RETURNING id",
    )
    .bind(p)
    .bind(bug)
    .bind(new)
    .bind(user)
    .bind(prio)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO issue_notes (issue_id, author_id, body) VALUES ($1, $2, 'old note')")
        .bind(issue)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    let epochs = "SELECT coalesce(sum(security_epoch), 0)::int8 FROM projects";
    let before = int(&pool, epochs).await;
    let roles_epoch_sql = "SELECT count(*) FROM pg_trigger WHERE tgname = 'role_permissions_epoch' AND tgenabled = 'O'";
    assert_eq!(int(&pool, roles_epoch_sql).await, 1);

    db::MIGRATOR.run_to(32, &pool).await.unwrap();

    assert_eq!(
        int(&pool, epochs).await,
        before,
        "B-37: 0032 bumps no epoch"
    );
    assert_eq!(
        int(&pool, roles_epoch_sql).await,
        1,
        "the epoch trigger is enabled again"
    );
    let wiki_read = ["view_wiki_pages", "view_wiki_edits"];
    let mut want = vec!["view_issues", "edit_issues", "edit_wiki_pages"];
    want.extend(wiki_read);
    assert_eq!(perms_of(&pool, dev).await, s(&want));
    let mut want = vec!["view_issues"];
    want.extend(wiki_read);
    assert_eq!(perms_of(&pool, reporter).await, s(&want));
    // B-50: limited to their own issues → no wiki reading.
    assert_eq!(
        perms_of(&pool, own).await,
        s(&["view_issues", "edit_issues", "edit_wiki_pages"])
    );
    let mut want = vec![
        "view_issues",
        "manage_project",
        "rename_wiki_pages",
        "delete_wiki_pages",
        "protect_wiki_pages",
        "manage_wiki",
        "import_issues",
        "import_time_entries",
    ];
    want.extend(wiki_read);
    assert_eq!(perms_of(&pool, mgr).await, s(&want));
    assert_eq!(perms_of(&pool, none).await, s(&["add_issues"]));

    // Existing text stays plain; new rows are markdown.
    let (fmt, body_fmt): (String, String) = sqlx::query_as(
        "SELECT i.description_format, n.body_format FROM issues i JOIN issue_notes n ON n.issue_id = i.id
         WHERE i.id = $1",
    )
    .bind(issue)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((fmt.as_str(), body_fmt.as_str()), ("plain", "plain"));
    let fresh: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id, priority_id)
         VALUES ($1,$2,$3,'new one',$4,$5) RETURNING id",
    )
    .bind(p)
    .bind(bug)
    .bind(new)
    .bind(user)
    .bind(prio)
    .fetch_one(&pool)
    .await
    .unwrap();
    let f: String = sqlx::query_scalar("SELECT description_format FROM issues WHERE id = $1")
        .bind(fresh)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(f, "markdown");
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM notification_preferences WHERE NOT on_mentioned"
        )
        .await,
        0
    );
    pool.close().await;
}
