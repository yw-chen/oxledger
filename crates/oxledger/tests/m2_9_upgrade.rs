// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.9 (§7.9, §2.1, B-37): upgrading an M1 database to M2.
//! Runs migrations up to 0030 (the M1 release) on a temporary database
//! `<test db>_upgm29`, writes what an M1 site holds (people, roles, two
//! projects, an issue with a description and a note, a watcher and an
//! unread notification, an agent delegation with a token, a linked GitHub
//! repository), then runs every later migration (0031–0042):
//!
//! * no project's `security_epoch` moved, the agent's token still
//!   authenticates and its work context still reads and writes (not
//!   `context_unavailable`);
//! * the description and the note stay `plain` and render as written;
//! * every project has one `wiki_settings` row and the wiki stays off;
//! * the notification and the repository link are still there;
//! * the search index of the M1 layout keeps serving and the blue-green
//!   rebuild replaces it;
//! * the wiki works once a project turns it on.
//!
//! The M1 search index is a `RawText` generation built after the migration
//! (this build's indexer does not read a 0030 schema); the data is written
//! with this build's functions where they still fit a 0030 schema and with
//! SQL otherwise.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use oxledger::auth;
use oxledger::changesets::{self, SubmitResult};
use oxledger::commands::{self};
use oxledger::db;
use oxledger::error::classify;
use oxledger::issue_view;
use oxledger::notifications::{self, ListQuery};
use oxledger::search::{Layout, SearchIndex};
use oxledger::service::{self, App, SearchParams};
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::json;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use std::sync::Arc;
use uuid::Uuid;
use world::*;

const ORIGIN: &str = "https://spa.test";

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

/// Drops the temporary database on every exit, a panic included (as in
/// m2_0_upgrade.rs).
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

async fn text(pool: &PgPool, sql: &str) -> String {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

const EPOCHS: &str =
    "SELECT string_agg(key || ':' || security_epoch, ',' ORDER BY key) FROM projects";

#[tokio::test]
async fn upgrade_from_the_m1_release() {
    let base = common::test_db();
    let name = base.scratch("upgm29");
    recreate(&base, &name, true).await;
    let _drop = DropDb {
        base: base.clone(),
        name: name.clone(),
    };
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();
    db::MIGRATOR.run_to(30, &pool).await.unwrap();

    // ---- an M1 site
    let w = World::build(&pool).await;
    let (issue, key) = create_issue(
        &pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "wombat before the upgrade",
            description: "*not* markdown, AUTH-1 <b>as typed</b>",
            author_id: w.alice,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO issue_notes (issue_id, author_id, body) VALUES ($1, $2, '# old note')",
    )
    .bind(issue)
    .bind(w.alice)
    .execute(&pool)
    .await
    .unwrap();
    // bob watches it and has one unread notification of its creation.
    sqlx::query("UPDATE issues SET last_event_seq = 1 WHERE id = $1")
        .bind(issue)
        .execute(&pool)
        .await
        .unwrap();
    let created: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                   system_operation, payload)
         VALUES ($1, 1, 'created', 'import', $2, 'system', 'm1_site', $3) RETURNING id",
    )
    .bind(issue)
    .bind(w.alice)
    .bind(json!({"fields": {"subject": "wombat before the upgrade", "is_private": false}}))
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ($1, $2, 'manual')")
        .bind(issue)
        .bind(w.bob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO notifications (recipient_id, issue_id, project_id, event_ids, reasons,
                                    event_reasons, first_at, last_at)
         VALUES ($1, $2, $3, ARRAY[$4::uuid], ARRAY['watching'], ARRAY['watching'], now(), now())",
    )
    .bind(w.bob)
    .bind(issue)
    .bind(w.web)
    .bind(created)
    .execute(&pool)
    .await
    .unwrap();
    // An agent of alice's in WEB, with a token.
    let d = w.delegate(w.web).await.unwrap();
    let credential = oxledger::agents::issue_credential(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.context_id,
        "m1",
        None,
    )
    .await
    .unwrap();
    // A GitHub repository linked to WEB.
    exec(
        &pool,
        &format!(
            "WITH i AS (INSERT INTO github_installations (gh_installation_id, account_login, state)
                        VALUES (7, 'acme', 'active') RETURNING id),
                  r AS (INSERT INTO github_repos (gh_repo_id, installation_id, full_name, is_private,
                                                  linkable, state)
                        SELECT 9, id, 'acme/app', true, true, 'accessible' FROM i RETURNING id)
             INSERT INTO project_repo_links (project_id, repo_id, linked_by, sync_state)
             SELECT '{}', id, '{}', 'ok' FROM r",
            w.web, w.alice
        ),
    )
    .await;
    let epochs = text(&pool, EPOCHS).await;
    let perms_before = count(&pool, "SELECT count(*) FROM role_permissions").await;

    // ---- the upgrade
    db::MIGRATOR.run(&pool).await.unwrap();
    assert_eq!(
        count(&pool, "SELECT max(version) FROM _sqlx_migrations").await,
        42
    );

    // §2.1 (B-37): no epoch moved, though 0032 granted the new permissions.
    assert_eq!(text(&pool, EPOCHS).await, epochs);
    assert!(count(&pool, "SELECT count(*) FROM role_permissions").await > perms_before);
    let dev: Vec<String> = sqlx::query_scalar(
        "SELECT permission FROM role_permissions WHERE role_id = $1 AND permission LIKE '%wiki%'
         ORDER BY permission",
    )
    .bind(w.dev)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        dev,
        ["edit_wiki_pages", "view_wiki_edits", "view_wiki_pages"]
    );

    // Existing text stays plain and shows as written.
    assert_eq!(
        text(
            &pool,
            &format!(
                "SELECT i.description_format || ',' || n.body_format FROM issues i
                 JOIN issue_notes n ON n.issue_id = i.id WHERE i.id = '{issue}'"
            )
        )
        .await,
        "plain,plain"
    );
    let v = issue_view::spa_view(
        &w.app,
        w.alice_ctx,
        &key,
        Some(vec!["recent_notes".into()]),
        Some(10),
        None,
    )
    .await
    .unwrap();
    let v = serde_json::to_string(&v).unwrap();
    assert!(v.contains("*not* markdown"), "{v}");
    assert!(v.contains("&lt;b&gt;as typed&lt;/b&gt;"), "{v}");
    assert!(v.contains("# old note"), "{v}");
    assert!(!v.contains("<em>") && !v.contains("<h1"), "{v}");

    // One wiki_settings row per project; the wiki stays off (B-9).
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM projects p JOIN wiki_settings s ON s.project_id = p.id"
        )
        .await,
        count(&pool, "SELECT count(*) FROM projects").await
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM enabled_modules WHERE module = 'wiki'"
        )
        .await,
        0
    );

    // The notification and the repository link are still there.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let inbox = notifications::list(&w.app, bob, &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(inbox.items.len(), 1);
    assert_eq!(inbox.items[0].issue.as_ref().unwrap().key, key);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM project_repo_links l JOIN github_repos r ON r.id = l.repo_id
             WHERE l.unlinked_at IS NULL AND r.default_branch IS NULL"
        )
        .await,
        1
    );

    // The M1 search index serves, then the blue-green rebuild replaces it.
    let index = SearchIndex::open(None).unwrap();
    index.set_first_build(Some(Layout::RawText));
    let app = Arc::new(App {
        pool: pool.clone(),
        index,
        wiki: oxledger::wiki_search::WikiIndex::open(None).unwrap(),
    });
    app.index.sync_all(&pool).await.unwrap();
    assert_eq!(app.index.view().unwrap().layout(), Layout::RawText);
    assert!(app.index.needs_upgrade());
    let find = |app: Arc<App>, who| async move {
        service::search(
            &app,
            who,
            SearchParams {
                q: Some("wombat".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.key)
        .collect::<Vec<_>>()
    };
    assert_eq!(
        find(app.clone(), w.alice_ctx).await,
        std::slice::from_ref(&key)
    );
    app.index.rebuild(&pool).await.unwrap();
    assert!(!app.index.needs_upgrade());
    assert_eq!(app.index.view().unwrap().layout(), Layout::Current);
    assert_eq!(
        find(app.clone(), w.alice_ctx).await,
        std::slice::from_ref(&key)
    );
    app.wiki.sync_all(&pool).await.unwrap();

    // The M1 token still authenticates; its context reads and writes.
    let agent = auth::authenticate_bearer(&pool, &credential.token)
        .await
        .unwrap()
        .expect("the M1 token");
    assert_eq!(find(app.clone(), agent).await, std::slice::from_ref(&key));
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&pool)
        .await
        .unwrap();
    let r = changesets::command_submit(
        &app,
        agent,
        Uuid::now_v7(),
        &note_doc(issue, lv, "after the upgrade"),
        false,
        ORIGIN,
    )
    .await;
    match r {
        Ok(SubmitResult::Applied { .. }) | Ok(SubmitResult::ReviewRequired { .. }) => {}
        Err(e) => panic!(
            "the M1 context after the upgrade: {:?}: {e:#}",
            classify(&e).map(|a| (a.code, a.details))
        ),
    }
    assert_eq!(text(&pool, EPOCHS).await, epochs);

    // M2 on the old project: the wiki, once it is turned on.
    exec(
        &pool,
        &format!(
            "INSERT INTO enabled_modules (project_id, module) VALUES ('{}', 'wiki')",
            w.web
        ),
    )
    .await;
    let r = commands::submit(
        &app,
        w.alice_ctx,
        Uuid::now_v7(),
        &json!({"command_schema_version": 1, "ops": [
            {"op_id": op(), "kind": "wiki_put", "target": {"project": "WEB", "page": "Home"},
             "body": "# after the upgrade"}]}),
    )
    .await
    .unwrap();
    assert_eq!(r.wiki.len(), 1);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT page_count::bigint FROM wiki_settings WHERE project_id = '{}'",
                w.web
            )
        )
        .await,
        1
    );
    pool.close().await;
}
