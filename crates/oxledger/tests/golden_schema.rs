// SPDX-License-Identifier: GPL-2.0-or-later
//! Golden (docs/research/09b G7 (5), G8): the schema every migration
//! builds, read from `pg_catalog` (not `pg_dump`, whose `\restrict` key is
//! random) and sorted in Rust, one snapshot per kind of object.
//!
//! The database is `<slot DB>_schema`: its `public` schema is dropped and
//! rebuilt from the migrations on every run (no `DROP DATABASE`, so the
//! test needs no `drop-database` group), so what other tests did to their
//! slot database (dynamic custom field indexes, a left-over trigger) never
//! shows up. Needs PostgreSQL 18.

mod common;

use common::snap::{self, sorted_lines};
use sqlx::PgPool;

async fn rows(pool: &PgPool, sql: &str) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| panic!("{e}: {sql}"))
}

/// `public` rebuilt from the migrations in `<slot DB>_schema`.
async fn fresh_schema() -> PgPool {
    let db = common::test_db();
    let name = db.scratch("schema");
    common::ensure_database(&db, &name).await;
    let raw = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&db.url_for(&name))
        .await
        .unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    oxledger::db::connect_with(
        oxledger::db::pool_options().max_connections(2),
        &db.url_for(&name),
    )
    .await
    .expect("migrate the schema database")
}

#[tokio::test]
async fn schema_catalog() {
    let pool = fresh_schema().await;
    let mut snaps: Vec<(&str, String)> = Vec::new();

    // Applied migrations: version and description (checksums would only
    // repeat the files; a changed file fails `sqlx migrate` anyway).
    snaps.push((
        "migrations",
        rows(
            &pool,
            "SELECT version::text || ' ' || description FROM _sqlx_migrations
             WHERE success ORDER BY version",
        )
        .await
        .join("\n"),
    ));

    // Columns in table order: type, nullability, default, identity, generated.
    snaps.push((
        "columns",
        rows(
            &pool,
            "SELECT c.relname || '.' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod)
                    || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END
                    || CASE WHEN a.attidentity <> '' THEN ' IDENTITY ' || a.attidentity::text ELSE '' END
                    || CASE WHEN a.attgenerated <> '' THEN ' GENERATED ' || coalesce(pg_get_expr(d.adbin, d.adrelid), '')
                            WHEN d.adbin IS NOT NULL THEN ' DEFAULT ' || pg_get_expr(d.adbin, d.adrelid)
                            ELSE '' END
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
             LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
             ORDER BY c.relname, a.attnum",
        )
        .await
        .join("\n"),
    ));

    // Constraints (primary, unique, foreign, check, exclusion, not null).
    snaps.push((
        "constraints",
        sorted_lines(
            rows(
                &pool,
                "SELECT c.relname || ' ' || k.conname || ' ' || pg_get_constraintdef(k.oid)
                 FROM pg_constraint k
                 JOIN pg_class c ON c.oid = k.conrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
                 WHERE k.contype <> 'n'",
            )
            .await,
        ),
    ));

    // Indexes, as CREATE INDEX statements.
    snaps.push((
        "indexes",
        sorted_lines(
            rows(
                &pool,
                "SELECT pg_get_indexdef(i.indexrelid)
                 FROM pg_index i
                 JOIN pg_class c ON c.oid = i.indexrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'",
            )
            .await,
        ),
    ));

    // Triggers (not the internal FK ones), with their enabled state.
    snaps.push((
        "triggers",
        sorted_lines(
            rows(
                &pool,
                "SELECT pg_get_triggerdef(t.oid)
                        || CASE t.tgenabled WHEN 'O' THEN '' ELSE ' [enabled=' || t.tgenabled::text || ']' END
                 FROM pg_trigger t
                 JOIN pg_class c ON c.oid = t.tgrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
                 WHERE NOT t.tgisinternal",
            )
            .await,
        ),
    ));

    // Functions: signature, result, language, volatility, and a short hash
    // of the body (the body itself is in the migration diff).
    snaps.push((
        "functions",
        sorted_lines(
            rows(
                &pool,
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ') '
                        || CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'RETURNS ' || pg_get_function_result(p.oid) END
                        || ' ' || l.lanname || ' ' || p.provolatile::text
                        || CASE WHEN p.prosecdef THEN ' SECURITY DEFINER' ELSE '' END
                        || ' body=' || left(md5(p.prosrc), 12)
                 FROM pg_proc p
                 JOIN pg_namespace n ON n.oid = p.pronamespace AND n.nspname = 'public'
                 JOIN pg_language l ON l.oid = p.prolang",
            )
            .await,
        ),
    ));

    // Types: enums with their labels in order; sequences; views.
    let mut types = rows(
        &pool,
        "SELECT 'enum ' || t.typname || ' (' || string_agg(e.enumlabel, ', ' ORDER BY e.enumsortorder) || ')'
         FROM pg_type t
         JOIN pg_namespace n ON n.oid = t.typnamespace AND n.nspname = 'public'
         JOIN pg_enum e ON e.enumtypid = t.oid
         GROUP BY t.typname",
    )
    .await;
    types.extend(
        rows(
            &pool,
            "SELECT 'sequence ' || c.relname || ' ' || format_type(s.seqtypid, NULL)
             FROM pg_sequence s
             JOIN pg_class c ON c.oid = s.seqrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'",
        )
        .await,
    );
    types.extend(
        rows(
            &pool,
            "SELECT 'view ' || c.relname || ' kind=' || c.relkind::text
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
             WHERE c.relkind IN ('v', 'm')",
        )
        .await,
    );
    snaps.push(("types", sorted_lines(types)));

    pool.close().await;
    snap::settings("schema").bind(|| {
        for (name, body) in &snaps {
            assert!(!body.is_empty(), "{name}: nothing read");
            insta::assert_snapshot!(*name, body);
        }
    });
}
