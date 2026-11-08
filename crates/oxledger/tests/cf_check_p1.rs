// SPDX-License-Identifier: GPL-2.0-or-later
//! Regression for contract 02 §2 P1: `issue_cf_check()` must reject a value
//! whose type check comes out NULL, not only FALSE.
//!
//! Before migration 0004 the trigger ended with `IF NOT ok`; a string field
//! written to `v_num` gave `NEW.v_text <> ''` = NULL and the row was accepted.
//!
//! 1. For every field type, a value in every column other than the type's own
//!    column is refused with check_violation (23514), through raw SQL that
//!    bypasses `custom_fields::set_value`.
//! 2. The existing-data scan of migration 0004 (its `DO` block, taken from the
//!    migration file itself) fails on a bad row written while the trigger was
//!    off, and passes once that row is gone.
//!
//! Needs PostgreSQL 18; see `common::test_pool` for which database it uses.

mod common;

use common::test_pool;
use oxledger::custom_fields;
use sqlx::PgPool;
use uuid::Uuid;

const MIGRATION_0004: &str = include_str!("../../../migrations/0004_cf_check_null_fix.sql");

/// The scan part of migration 0004: from `DO $$` up to its `END $$;`.
fn scan_block() -> &'static str {
    let start = MIGRATION_0004.find("DO $$").expect("DO block");
    let end = MIGRATION_0004[start..]
        .find("END $$;")
        .expect("end of DO block")
        + start;
    &MIGRATION_0004[start..end + "END $$;".len()]
}

async fn reset(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE issue_cf, field_options, field_trackers, field_projects, field_roles, field_defs,
                  outbox, outbox_cursors, issue_keys, issues, workflow_transitions, project_trackers,
                  member_roles, members, role_permission_trackers, role_permissions, roles, enabled_modules,
                  projects, api_keys, group_members, aliases, principals, trackers, issue_statuses CASCADE",
    )
    .execute(pool)
    .await
    .unwrap();
    custom_fields::reconcile_indexes(pool).await.unwrap();
}

/// One issue to hang values on; returns (issue id, a user id).
async fn world(pool: &PgPool) -> (Uuid, Uuid) {
    let status: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_statuses (name, category) VALUES ('New','todo') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let tracker: Uuid =
        sqlx::query_scalar("INSERT INTO trackers (name) VALUES ('Bug') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO principals (kind, login, name) VALUES ('user','u','U') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (key, name, is_public) VALUES ('P1','P1',false) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let issue: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, author_id)
         VALUES ($1,$2,$3,'s',$4) RETURNING id",
    )
    .bind(project)
    .bind(tracker)
    .bind(status)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    (issue, user)
}

async fn field(pool: &PgPool, name: &str, ftype: &str) -> i32 {
    sqlx::query_scalar("INSERT INTO field_defs (name, ftype) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(ftype)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Writes one raw value into `column` and returns the SQLSTATE of the error, if any.
async fn raw_insert(
    pool: &PgPool,
    issue: Uuid,
    field: i32,
    column: &str,
    value: &str,
) -> Option<String> {
    let sql =
        format!("INSERT INTO issue_cf (issue_id, field_id, {column}) VALUES ($1, $2, {value})");
    let res = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(issue)
        .bind(field)
        .execute(pool)
        .await;
    // Do not leave accepted rows behind for the next case.
    sqlx::query("DELETE FROM issue_cf WHERE issue_id = $1 AND field_id = $2")
        .bind(issue)
        .bind(field)
        .execute(pool)
        .await
        .unwrap();
    match res {
        Ok(_) => None,
        Err(sqlx::Error::Database(e)) => Some(e.code().unwrap_or_default().into_owned()),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[tokio::test]
async fn p1_wrong_column_is_rejected_and_scan_catches_old_rows() {
    let pool = test_pool().await;
    reset(&pool).await;
    let (issue, user) = world(&pool).await;

    let list = field(&pool, "Severity", "list").await;
    let option: Uuid = sqlx::query_scalar(
        "INSERT INTO field_options (field_id, label) VALUES ($1, 'High') RETURNING id",
    )
    .bind(list)
    .fetch_one(&pool)
    .await
    .unwrap();

    // A valid literal for each column.
    let user_lit = format!("'{user}'::uuid");
    let option_lit = format!("'{option}'::uuid");
    let samples: [(&str, &str); 5] = [
        ("v_num", "7"),
        ("v_date", "'2026-11-08'::date"),
        ("v_text", "'x'"),
        ("v_ref", "NULL"), // replaced per type below
        ("v_bool", "true"),
    ];
    let types = [
        ("int", "v_num"),
        ("float", "v_num"),
        ("date", "v_date"),
        ("string", "v_text"),
        ("link", "v_text"),
        ("text", "v_text"),
        ("list", "v_ref"),
        ("user", "v_ref"),
        ("bool", "v_bool"),
    ];
    let mut checked = 0;
    for (ftype, own) in types {
        let fid = if ftype == "list" {
            list
        } else {
            field(&pool, &format!("F-{ftype}"), ftype).await
        };
        let ref_lit = if ftype == "list" {
            &option_lit
        } else {
            &user_lit
        };
        for (column, lit) in samples {
            let lit = if column == "v_ref" {
                ref_lit.as_str()
            } else {
                lit
            };
            let state = raw_insert(&pool, issue, fid, column, lit).await;
            if column == own {
                assert_eq!(
                    state, None,
                    "{ftype}: its own column {column} must be accepted"
                );
            } else {
                assert_eq!(
                    state.as_deref(),
                    Some("23514"),
                    "{ftype}: a value in {column} must be refused with check_violation"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 9 * 4);

    // The case from the contract text, spelled out: string field, value in v_num.
    let s = field(&pool, "Customer", "string").await;
    assert_eq!(
        raw_insert(&pool, issue, s, "v_num", "1").await.as_deref(),
        Some("23514")
    );

    // Migration 0004's scan: clean data passes.
    sqlx::raw_sql(scan_block())
        .execute(&pool)
        .await
        .expect("scan passes on clean data");

    // A bad row written with the trigger off (as 0002 would have let through) makes it fail.
    sqlx::raw_sql("ALTER TABLE issue_cf DISABLE TRIGGER issue_cf_check")
        .execute(&pool)
        .await
        .unwrap();
    let insert_bad =
        sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_num) VALUES ($1, $2, 1)")
            .bind(issue)
            .bind(s)
            .execute(&pool)
            .await;
    sqlx::raw_sql("ALTER TABLE issue_cf ENABLE TRIGGER issue_cf_check")
        .execute(&pool)
        .await
        .unwrap();
    insert_bad.unwrap();
    let err = sqlx::raw_sql(scan_block())
        .execute(&pool)
        .await
        .expect_err("scan must fail on a bad row");
    let msg = err.to_string();
    assert!(msg.contains("1 issue_cf row(s)"), "{msg}");
    assert!(msg.contains(&format!("field {s}")), "{msg}");

    sqlx::query("DELETE FROM issue_cf WHERE field_id = $1")
        .bind(s)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(scan_block())
        .execute(&pool)
        .await
        .expect("scan passes again");
    reset(&pool).await;
}
