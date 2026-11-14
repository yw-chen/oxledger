// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.9 (T-M1-84, appendix D V2): upgrading to the AI team (0029).
//! Runs migrations 0001–0028 on a temporary database `<test db>_upg19` and
//! writes four agent principals of one person the way 02 made them:
//! - `Old Agent`: an active context, a delegation and a token;
//! - a client named with 100 characters: only closed contexts;
//! - two clients whose names share the first 56 characters: no context at
//!   all (the backfilled labels collide and the second gets `-2`).
//!
//! Then runs 0029 and checks: one backfilled external runtime per
//! principal, labels of at most 60 characters, every context, delegation and
//! token bound to its runtime, no project's `security_epoch` moved, and the
//! old token still authenticates as the same principal with the same scope.
//!
//! The temporary database is dropped. Needs PostgreSQL 18.

use oxledger::{auth, db};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;

mod common;

fn target() -> (common::TestDb, String) {
    target_named("upg19")
}

/// `<test db>_<suffix>`: each test has its own database, so they may run
/// side by side.
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

/// An 02-style agent principal of `user` for a new client called `name`.
async fn principal(pool: &PgPool, user: Uuid, name: &str, handle: &str) -> Uuid {
    let client: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_clients (name, enabled, egress_policy_ref) VALUES ($1, true, 'p') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    let agent = one(
        pool,
        "INSERT INTO principals (kind, name) VALUES ('agent', 'agent') RETURNING id",
    )
    .await;
    sqlx::query(
        "INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id, handle)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(agent)
    .bind(user)
    .bind(client)
    .bind(handle)
    .execute(pool)
    .await
    .unwrap();
    agent
}

async fn context(pool: &PgPool, agent: Uuid, session: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agent_work_contexts (agent_principal_id, fresh_context_ack, created_by_session)
         VALUES ($1, true, $2) RETURNING id",
    )
    .bind(agent)
    .bind(session)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn t_m1_84_upgrade_ai_team() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();

    db::MIGRATOR.run_to(28, &pool).await.unwrap();
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
         INSERT INTO enabled_modules VALUES ('{p}', 'issue_tracking');"
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

    // V2 case 1: an active context with a delegation and a token.
    let old = principal(&pool, user, "Old Agent", "old-agent@u").await;
    let ctx = context(&pool, old, session).await;
    let epoch_sql = format!("SELECT security_epoch FROM projects WHERE id = '{p}'");
    let epoch = int(&pool, &epoch_sql).await;
    let delegation: Uuid = sqlx::query_scalar(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id,
                                  security_epoch, proposal_ops, writable_fields, expires_at)
         VALUES ($1, $2, $3, $4, $5, ARRAY['add_note'], ARRAY[]::text[], now() + interval '7 days')
         RETURNING id",
    )
    .bind(old)
    .bind(user)
    .bind(ctx)
    .bind(p)
    .bind(epoch)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_context_source_projects (context_id, project_id, security_epoch, first_delegation_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(ctx)
    .bind(p)
    .bind(epoch)
    .bind(delegation)
    .execute(&pool)
    .await
    .unwrap();
    let (token, hash) = auth::new_agent_token();
    let credential: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_credentials (context_id, token_sha256, label, expires_at)
         VALUES ($1, $2, 'old', now() + interval '7 days') RETURNING id",
    )
    .bind(ctx)
    .bind(&hash[..])
    .fetch_one(&pool)
    .await
    .unwrap();

    // V2 case 2 and 3: a 100-character client name, only closed contexts.
    let long = principal(&pool, user, &"L".repeat(100), "long@u").await;
    for _ in 0..2 {
        let c = context(&pool, long, session).await;
        pool.execute(AssertSqlSafe(format!(
            "UPDATE agent_work_contexts SET state = 'closed', closed_at = now() WHERE id = '{c}'"
        )))
        .await
        .unwrap();
    }
    // V2 case 3 and 4: no context; two names equal in their first 56 characters.
    let prefix = "P".repeat(56);
    let first = principal(&pool, user, &format!("{prefix}AAAA"), "first@u").await;
    let second = principal(&pool, user, &format!("{prefix}BBBB"), "second@u").await;

    db::MIGRATOR.run_to(29, &pool).await.unwrap();

    assert_eq!(
        int(&pool, &epoch_sql).await,
        epoch,
        "0029 changes no permission"
    );
    let label = |agent: Uuid| {
        let pool = pool.clone();
        async move {
            let row: (
                String,
                String,
                bool,
                bool,
                Option<Uuid>,
                String,
                String,
                i32,
            ) = sqlx::query_as(
                "SELECT r.label, r.kind, r.isolation_ack, r.backfilled, r.created_by_session,
                        ap.role_key, ap.role_template, ap.template_version
                 FROM agent_principals ap JOIN agent_runtimes r ON r.id = ap.runtime_id
                 WHERE ap.agent_principal_id = $1",
            )
            .bind(agent)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.1, "external");
            assert!(row.2 && row.3, "isolation_ack and backfilled");
            assert_eq!(
                (row.5.as_str(), row.6.as_str(), row.7),
                ("personal", "personal", 1)
            );
            (row.0, row.4)
        }
    };
    assert_eq!(label(old).await, ("Old Agent".to_string(), Some(session)));
    assert_eq!(label(long).await, ("L".repeat(56), Some(session)));
    let a = label(first).await;
    let b = label(second).await;
    assert_eq!((a.1, b.1), (None, None), "no context, no session");
    let mut labels = vec![a.0, b.0];
    labels.sort();
    assert_eq!(labels, vec![prefix.clone(), format!("{prefix}-2")]);
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM agent_runtimes WHERE char_length(label) > 60"
        )
        .await,
        0
    );
    assert_eq!(int(&pool, "SELECT count(*) FROM agent_runtimes").await, 4);
    // Every context (closed ones too), delegation and token is bound to its runtime.
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM agent_work_contexts c JOIN agent_principals ap
               ON ap.agent_principal_id = c.created_for_principal_id
             WHERE c.runtime_id IS DISTINCT FROM ap.runtime_id"
        )
        .await,
        0
    );
    assert_eq!(
        int(&pool, "SELECT count(*) FROM agent_work_contexts").await,
        3
    );
    let (d_rt, d_ap): (Uuid, Uuid) =
        sqlx::query_as("SELECT runtime_id, agent_principal_id FROM delegations WHERE id = $1")
            .bind(delegation)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (c_rt, c_ap): (Uuid, Uuid) = sqlx::query_as(
        "SELECT runtime_id, agent_principal_id FROM agent_credentials WHERE id = $1",
    )
    .bind(credential)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((d_ap, c_ap), (old, old));
    assert_eq!(d_rt, c_rt);

    // The old token still works, as the same principal, with the same scope.
    // The code under test is today's: bring the schema to the latest
    // migration first (0031 added agent_credentials.last_used_at).
    db::MIGRATOR.run(&pool).await.unwrap();
    let a = auth::authenticate_bearer(&pool, &token)
        .await
        .unwrap()
        .expect("the old token authenticates");
    assert_eq!(a.principal_id(), Some(old));
    let mut conn = pool.acquire().await.unwrap();
    let scope = db::load_scope(&mut conn, a).await.unwrap();
    assert_eq!(scope.project_ids().collect::<Vec<_>>(), vec![p]);
    drop(conn);

    pool.close().await;
    recreate(&base, &name, false).await;
}

/// From the independent verification: a context suspended by the breaker,
/// two delegations, and an 02 client named `oxledger-ai` whose handle looks
/// like the built-in AI's. After 0029 the suspension stays on the (now
/// shared) context, both projects stay readable, R is unchanged, and the
/// look-alike handle is left as it was (verification D5: not rewritten;
/// 0029 prints a NOTICE and upgrade-notes.md has the query).
#[tokio::test]
async fn v_upgrade_with_suspension_and_lookalike_handle() {
    let (base, name) = target_named("upg19v");
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&base.url_for(&name)).await.unwrap();
    db::MIGRATOR.run_to(28, &pool).await.unwrap();
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
    let q = one(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('TWO','Two') RETURNING id",
    )
    .await;
    let role = one(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Dev','all') RETURNING id",
    )
    .await;
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO role_permissions (role_id, permission) VALUES ('{role}', 'view_issues');
         INSERT INTO enabled_modules VALUES ('{p}', 'issue_tracking'), ('{q}', 'issue_tracking');"
    )))
    .await
    .unwrap();
    for pr in [p, q] {
        let m = one(
            &pool,
            &format!(
                "INSERT INTO members (project_id, principal_id) VALUES ('{pr}', '{user}') RETURNING id"
            ),
        )
        .await;
        pool.execute(AssertSqlSafe(format!(
            "INSERT INTO member_roles VALUES ('{m}', '{role}')"
        )))
        .await
        .unwrap();
    }
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
    let old = principal(&pool, user, "Old Agent", "old-agent@u").await;
    let look = principal(&pool, user, "oxledger-ai", "oxledger-ai@u").await;
    let ctx = context(&pool, old, session).await;
    for pr in [p, q] {
        let e = int(
            &pool,
            &format!("SELECT security_epoch FROM projects WHERE id = '{pr}'"),
        )
        .await;
        let d: Uuid = sqlx::query_scalar(
            "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id,
                                      security_epoch, proposal_ops, writable_fields, expires_at)
             VALUES ($1, $2, $3, $4, $5, ARRAY['add_note'], ARRAY[]::text[], now() + interval '7 days')
             RETURNING id",
        )
        .bind(old)
        .bind(user)
        .bind(ctx)
        .bind(pr)
        .bind(e)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_context_source_projects (context_id, project_id, security_epoch, first_delegation_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(ctx)
        .bind(pr)
        .bind(e)
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.execute(AssertSqlSafe(format!(
        "INSERT INTO agent_context_write_controls (context_id, suspended_at, suspension_reason)
         VALUES ('{ctx}', now(), 'audience_blocked')
         ON CONFLICT (context_id) DO UPDATE SET suspended_at = now(), suspension_reason = 'audience_blocked'"
    )))
    .await
    .unwrap();
    let (token, hash) = auth::new_agent_token();
    sqlx::query(
        "INSERT INTO agent_credentials (context_id, token_sha256, label, expires_at)
         VALUES ($1, $2, 'old', now() + interval '7 days')",
    )
    .bind(ctx)
    .bind(&hash[..])
    .execute(&pool)
    .await
    .unwrap();

    db::MIGRATOR.run_to(29, &pool).await.unwrap();

    assert_eq!(
        int(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_context_write_controls
                 WHERE context_id = '{ctx}' AND suspended_at IS NOT NULL"
            )
        )
        .await,
        1
    );
    // Today's code needs today's schema (0031: last_used_at). 0030 refuses
    // an ordinary client named oxledger-ai until it is renamed
    // (upgrade-notes.md); the principal's handle is not touched.
    sqlx::query("UPDATE agent_clients SET name = 'oxledger-ai (02)' WHERE name = 'oxledger-ai'")
        .execute(&pool)
        .await
        .unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    let a = auth::authenticate_bearer(&pool, &token)
        .await
        .unwrap()
        .expect("old token");
    let mut conn = pool.acquire().await.unwrap();
    let mut scope: Vec<Uuid> = db::load_scope(&mut conn, a)
        .await
        .unwrap()
        .project_ids()
        .collect();
    scope.sort();
    let mut want = vec![p, q];
    want.sort();
    assert_eq!(scope, want);
    drop(conn);
    assert_eq!(
        int(
            &pool,
            &format!(
                "SELECT count(*) FROM agent_context_source_projects WHERE context_id = '{ctx}'"
            )
        )
        .await,
        2
    );
    let h: String =
        sqlx::query_scalar("SELECT handle FROM agent_principals WHERE agent_principal_id = $1")
            .bind(look)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(h, "oxledger-ai@u", "02 data is not rewritten");
    // The administrator's query from upgrade-notes.md finds it.
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM agent_principals ap JOIN agent_runtimes r ON r.id = ap.runtime_id
             WHERE r.kind = 'external' AND ap.handle LIKE 'oxledger-ai@%'"
        )
        .await,
        1
    );
    pool.close().await;
    recreate(&base, &name, false).await;
}
