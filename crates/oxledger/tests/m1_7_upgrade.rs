// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.7: upgrading to concepts (0027). Runs migrations 0001–0026
//! on a temporary database `<test db>_upg17`, writes a project and concept
//! labels the way a seed or import did before 0027 (no concept rows, and
//! labels that collide after NFC + casefold — R18: across concepts, within
//! one concept, by case and by NFD), then runs 0027:
//! - it does not fail on the collisions: one label per normalized label
//!   stays active (a `pref` label first, then the oldest concept, then the
//!   smallest label) and the others become `staging`;
//! - every concept id gets a `concepts` row (pref label from its `pref`
//!   label, else the smallest; made by the system principal);
//! - the partial unique index then refuses a new active collision, while a
//!   staging duplicate is accepted;
//! - no project's `security_epoch` moves (no permission change).
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
    (base.to_string(), format!("{name}_upg17"))
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

async fn int(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn label(pool: &PgPool, concept: Uuid, label: &str, kind: &str) {
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, label_kind) VALUES ('concept', $1, $2, $3)",
    )
    .bind(concept)
    .bind(label)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
}

async fn status(pool: &PgPool, concept: Uuid, label: &str) -> String {
    sqlx::query_scalar(
        "SELECT status FROM aliases WHERE entity_kind = 'concept' AND entity_id = $1 AND label = $2",
    )
    .bind(concept)
    .bind(label)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn m1_7_upgrade_concepts() {
    let (base, name) = target();
    recreate(&base, &name, true).await;
    let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();

    db::MIGRATOR.run_to(26, &pool).await.unwrap();
    let p: Uuid =
        sqlx::query_scalar("INSERT INTO projects (key, name) VALUES ('OLD','Old') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let epoch_sql = format!("SELECT security_epoch FROM projects WHERE id = '{p}'");
    let epoch = int(&pool, &epoch_sql).await;
    // uuidv7 order: c1 is the oldest concept.
    let (c1, c2, c3, c4) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    // c1: 登入 (pref), SSO, Login and login (the same label within c1).
    label(&pool, c1, "登入", "pref").await;
    label(&pool, c1, "SSO", "alt").await;
    label(&pool, c1, "Login", "alt").await;
    label(&pool, c1, "login", "alt").await;
    // c2 (no pref label): sso (taken by c1), café in NFD, LOGIN.
    label(&pool, c2, "sso", "alt").await;
    label(&pool, c2, "cafe\u{301}", "alt").await;
    label(&pool, c2, "LOGIN", "hidden").await;
    // c3: CAFÉ (NFC) as its pref label: a pref label wins over c2's alt.
    label(&pool, c3, "CAFÉ", "pref").await;
    // c4: its pref label café loses to c3's (older); an alt label takes
    // its place as the name (alt before hidden).
    label(&pool, c4, "café", "pref").await;
    label(&pool, c4, "zeta", "alt").await;
    label(&pool, c4, "eta", "hidden").await;
    // A non-concept alias is not touched.
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label) VALUES ('project', $1, 'sso')",
    )
    .bind(p)
    .execute(&pool)
    .await
    .unwrap();

    db::MIGRATOR.run_to(27, &pool).await.unwrap();

    // Concept rows: the name is the active pref label. c2 lost every
    // label to older concepts: retired, labels kept as staging (D1).
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, pref_label, status FROM concepts ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (c1, "登入".to_string(), "active".to_string()),
            (c2, "LOGIN".to_string(), "retired".to_string()),
            (c3, "CAFÉ".to_string(), "active".to_string()),
            (c4, "zeta".to_string(), "active".to_string()),
        ]
    );
    let system = int(
        &pool,
        "SELECT count(*) FROM concepts WHERE created_by = '00000000-0000-7000-8000-000000000001'",
    )
    .await;
    assert_eq!(system, 4);
    // Every active concept has exactly one pref label: active, its name.
    let bad = int(
        &pool,
        "SELECT count(*) FROM concepts c
         WHERE c.status = 'active'
           AND (SELECT count(*) FROM aliases a
                WHERE a.entity_kind = 'concept' AND a.entity_id = c.id AND a.label_kind = 'pref'
                  AND a.status = 'active' AND a.label = c.pref_label) <> 1",
    )
    .await;
    assert_eq!(bad, 0, "an active concept without its active pref label");
    let prefs = int(
        &pool,
        "SELECT count(*) FROM aliases WHERE entity_kind = 'concept' AND label_kind = 'pref'",
    )
    .await;
    assert_eq!(prefs, 3, "no staging or second pref label is left");
    assert_eq!(status(&pool, c4, "café").await, "staging");
    assert_eq!(status(&pool, c4, "zeta").await, "active");
    assert_eq!(status(&pool, c4, "eta").await, "active");
    // R18: one active label per normalized label.
    assert_eq!(status(&pool, c1, "登入").await, "active");
    assert_eq!(status(&pool, c1, "SSO").await, "active");
    assert_eq!(status(&pool, c1, "Login").await, "active");
    assert_eq!(status(&pool, c1, "login").await, "staging", "same concept");
    assert_eq!(
        status(&pool, c2, "sso").await,
        "staging",
        "older concept wins"
    );
    assert_eq!(status(&pool, c2, "LOGIN").await, "staging");
    assert_eq!(
        status(&pool, c2, "cafe\u{301}").await,
        "staging",
        "a pref label wins"
    );
    assert_eq!(status(&pool, c3, "CAFÉ").await, "active");
    assert_eq!(
        int(
            &pool,
            "SELECT count(*) FROM aliases WHERE entity_kind = 'project' AND status = 'active'"
        )
        .await,
        1
    );
    // The unique index now holds: an active collision is refused, a
    // staging duplicate is accepted.
    let e = sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, status) VALUES ('concept', $1, 'sSo', 'active')",
    )
    .bind(c3)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(format!("{e}").contains("aliases_concept_label_uq"), "{e}");
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, status) VALUES ('concept', $1, 'sSo', 'staging')",
    )
    .bind(c3)
    .execute(&pool)
    .await
    .unwrap();
    // No permission change: the epoch stays.
    assert_eq!(int(&pool, &epoch_sql).await, epoch);

    pool.close().await;
    recreate(&base, &name, false).await;
}
