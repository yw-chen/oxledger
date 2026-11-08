// SPDX-License-Identifier: GPL-2.0-or-later
//! Custom field storage (decision D14): definitions, values, and the
//! per-field covering partial index `issue_cf_f<id>`.
//!
//! Schema and DDL helpers: `migrations/0002_custom_fields.sql`.
//!
//! Index lifecycle. `CREATE/DROP INDEX CONCURRENTLY` cannot run inside a
//! transaction, so it runs here on a pooled connection with the simple query
//! protocol (`sqlx::raw_sql`):
//! * [`create_field`] inserts the definition (`index_state = 'pending'`) and
//!   then builds the index before returning. It is synchronous from the
//!   caller's point of view — on a large `issue_cf` the build can take a while
//!   (it scans the table); filters work during the build, only without the index.
//! * [`drop_field`] marks the field `dropping`. That UPDATE waits for every
//!   writer holding the `FOR SHARE` lock the write trigger takes on the
//!   definition, and afterwards the trigger rejects new values. Then it deletes
//!   the values through the field's index, drops the index concurrently and
//!   deletes the definition.
//! * [`reconcile_indexes`] is the background job: it finishes whatever a crash
//!   interrupted. `oxledger serve` runs it at startup and periodically.

use crate::db::Caller;
use anyhow::{Context, Result, bail};
use ox_core::filter::{FieldType, Value, ValueKind};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

/// Column of `issue_cf` holding values of this kind.
pub fn value_column(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Num => "v_num",
        ValueKind::Date => "v_date",
        ValueKind::Text => "v_text",
        ValueKind::Ref => "v_ref",
        ValueKind::Bool => "v_bool",
    }
}

pub fn index_name(field_id: i32) -> String {
    format!("issue_cf_f{field_id}")
}

pub struct NewField<'a> {
    pub name: &'a str,
    pub ty: FieldType,
    pub description: &'a str,
    pub is_required: bool,
    /// All projects; otherwise only `projects`.
    pub is_for_all: bool,
    pub trackers: &'a [Uuid],
    pub projects: &'a [Uuid],
    /// Option labels, for `list` fields.
    pub options: &'a [&'a str],
    /// Readable by everyone who can see the issue; otherwise only by admins and
    /// callers whose effective roles in the issue's project include one of
    /// `roles` (builtin Non member / Anonymous allowed, see migration 0003).
    pub visible: bool,
    pub roles: &'a [Uuid],
}

/// Creates a field and builds its index (see module docs). Returns the field id.
pub async fn create_field(pool: &PgPool, f: NewField<'_>) -> Result<i32> {
    if (f.ty == FieldType::List) == f.options.is_empty() {
        bail!("list fields need options, other types must not have any");
    }
    let mut tx = pool.begin().await?;
    let state = if f.ty == FieldType::Text {
        "none"
    } else {
        "pending"
    };
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, description, is_required, is_for_all, visible, index_state)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(f.name)
    .bind(f.ty.as_str())
    .bind(f.description)
    .bind(f.is_required)
    .bind(f.is_for_all)
    .bind(f.visible)
    .bind(state)
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("create custom field {:?}", f.name))?;
    sqlx::query("INSERT INTO field_trackers (field_id, tracker_id) SELECT $1, unnest($2::uuid[])")
        .bind(id)
        .bind(f.trackers)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO field_projects (field_id, project_id) SELECT $1, unnest($2::uuid[])")
        .bind(id)
        .bind(f.projects)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO field_roles (field_id, role_id) SELECT $1, unnest($2::uuid[])")
        .bind(id)
        .bind(f.roles)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO field_options (field_id, label, position)
         SELECT $1, label, pos FROM unnest($2::text[]) WITH ORDINALITY AS o(label, pos)",
    )
    .bind(id)
    .bind(f.options)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    build_index(pool, id).await?;
    Ok(id)
}

async fn run_ddl(pool: &PgPool, ddl: String) -> Result<()> {
    // The DDL comes from cf_index_ddl / cf_drop_index_ddl: an integer id and fixed column names.
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl.clone()))
        .execute(pool)
        .await
        .with_context(|| format!("run {ddl}"))?;
    Ok(())
}

/// Builds (or rebuilds, if a failed concurrent build left it invalid) the index
/// of one field and marks it ready. No-op for unindexed types.
pub async fn build_index(pool: &PgPool, field_id: i32) -> Result<()> {
    let ddl: Option<String> = sqlx::query_scalar("SELECT cf_index_ddl($1)")
        .bind(field_id)
        .fetch_one(pool)
        .await?;
    let Some(ddl) = ddl else { return Ok(()) };
    // A failed CREATE INDEX CONCURRENTLY leaves an INVALID index behind, and
    // IF NOT EXISTS would then silently keep it.
    let invalid: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_index x WHERE x.indexrelid = to_regclass(cf_index_name($1))
                          AND NOT x.indisvalid)",
    )
    .bind(field_id)
    .fetch_one(pool)
    .await?;
    if invalid {
        drop_index(pool, field_id).await?;
    }
    run_ddl(pool, ddl).await?;
    sqlx::query(
        "UPDATE field_defs SET index_state = 'ready' WHERE id = $1 AND index_state = 'pending'",
    )
    .bind(field_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn drop_index(pool: &PgPool, field_id: i32) -> Result<()> {
    let ddl: String = sqlx::query_scalar("SELECT cf_drop_index_ddl($1)")
        .bind(field_id)
        .fetch_one(pool)
        .await?;
    run_ddl(pool, ddl).await
}

/// Deletes a field with all its values (see module docs). Returns false if it
/// did not exist. Must not run inside a transaction (it uses CONCURRENTLY).
pub async fn drop_field(pool: &PgPool, field_id: i32) -> Result<bool> {
    // Waits for writers holding the field's FOR SHARE lock; later writers see 'dropping'.
    let ty: Option<String> = sqlx::query_scalar(
        "UPDATE field_defs SET index_state = 'dropping' WHERE id = $1 RETURNING ftype",
    )
    .bind(field_id)
    .fetch_optional(pool)
    .await?;
    let Some(ty) = ty else { return Ok(false) };
    let Some(ty) = FieldType::parse(&ty) else {
        bail!("field {field_id} has unknown type {ty}")
    };
    // Literal field id (an i32) + type condition, so the partial index serves the delete.
    let delete = format!(
        "DELETE FROM issue_cf WHERE field_id = {field_id} AND {} IS NOT NULL",
        value_column(ty.kind())
    );
    sqlx::query(sqlx::AssertSqlSafe(delete))
        .execute(pool)
        .await?;
    drop_index(pool, field_id).await?;
    sqlx::query("DELETE FROM field_defs WHERE id = $1")
        .bind(field_id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// Background job: builds pending or broken indexes, finishes interrupted
/// drops, removes orphan `issue_cf_f*` indexes. Returns how many fixes it made.
pub async fn reconcile_indexes(pool: &PgPool) -> Result<usize> {
    let mut fixed = 0;
    let dropping: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM field_defs WHERE index_state = 'dropping' ORDER BY id")
            .fetch_all(pool)
            .await?;
    for id in dropping {
        drop_field(pool, id).await?;
        fixed += 1;
    }
    let to_build: Vec<i32> = sqlx::query_scalar(
        "SELECT d.id FROM field_defs d
         LEFT JOIN pg_index x ON x.indexrelid = to_regclass(cf_index_name(d.id))
         WHERE d.index_state IN ('pending', 'ready') AND (x.indexrelid IS NULL OR NOT x.indisvalid)
         ORDER BY d.id",
    )
    .fetch_all(pool)
    .await?;
    for id in to_build {
        build_index(pool, id).await?;
        fixed += 1;
    }
    let orphans: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_index x JOIN pg_class c ON c.oid = x.indexrelid
         WHERE x.indrelid = 'issue_cf'::regclass AND c.relname ~ '^issue_cf_f[0-9]+$'
           AND NOT EXISTS (SELECT 1 FROM field_defs d
                           WHERE cf_index_name(d.id) = c.relname AND d.ftype <> 'text')",
    )
    .fetch_all(pool)
    .await?;
    for name in orphans {
        let id: i32 = name["issue_cf_f".len()..].parse()?;
        drop_index(pool, id).await?;
        fixed += 1;
    }
    Ok(fixed)
}

/// Sets (inserts or replaces) one value. The database trigger checks that the
/// value matches the field type and that list options / users exist. An empty
/// string clears the value ("" means none, as in Redmine).
pub async fn set_value<'e>(
    db: impl PgExecutor<'e>,
    issue_id: Uuid,
    field_id: i32,
    value: &Value,
) -> Result<()> {
    if matches!(value, Value::Text(t) if t.is_empty()) {
        return clear_value(db, issue_id, field_id).await;
    }
    let (mut num, mut date, mut text, mut r, mut b) = (None, None, None, None, None);
    match value {
        Value::Num(n) => num = Some(n.to_string()),
        Value::Date(d) => date = Some(*d),
        Value::Text(t) => text = Some(t.clone()),
        Value::Ref(u) => r = Some(*u),
        Value::Bool(x) => b = Some(*x),
    }
    sqlx::query(
        "INSERT INTO issue_cf (issue_id, field_id, v_num, v_date, v_text, v_ref, v_bool)
         VALUES ($1, $2, $3::numeric, $4, $5, $6, $7)
         ON CONFLICT (issue_id, field_id) DO UPDATE SET v_num = EXCLUDED.v_num,
           v_date = EXCLUDED.v_date, v_text = EXCLUDED.v_text, v_ref = EXCLUDED.v_ref,
           v_bool = EXCLUDED.v_bool",
    )
    .bind(issue_id)
    .bind(field_id)
    .bind(num)
    .bind(date)
    .bind(text)
    .bind(r)
    .bind(b)
    .execute(db)
    .await
    .with_context(|| format!("set custom field {field_id} on issue {issue_id}"))?;
    Ok(())
}

pub async fn clear_value<'e>(db: impl PgExecutor<'e>, issue_id: Uuid, field_id: i32) -> Result<()> {
    sqlx::query("DELETE FROM issue_cf WHERE issue_id = $1 AND field_id = $2")
        .bind(issue_id)
        .bind(field_id)
        .execute(db)
        .await?;
    Ok(())
}

/// The option id of a list field by label (case-insensitive).
pub async fn option_id(pool: &PgPool, field_id: i32, label: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM field_options WHERE field_id = $1 AND lower(label) = lower($2)",
    )
    .bind(field_id)
    .bind(label)
    .fetch_optional(pool)
    .await?)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FieldRow {
    pub id: i32,
    pub name: String,
    pub ftype: String,
    pub visible: bool,
    pub is_for_all: bool,
    pub trackers: Vec<Uuid>,
    pub projects: Vec<Uuid>,
}

impl FieldRow {
    pub fn field_type(&self) -> Result<FieldType> {
        FieldType::parse(&self.ftype)
            .with_context(|| format!("field {} has unknown type {}", self.id, self.ftype))
    }
}

/// All fields (except those being deleted) with their applicability, by position and name.
pub async fn list_fields(pool: &PgPool) -> Result<Vec<FieldRow>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.name, d.ftype, d.visible, d.is_for_all,
                ARRAY(SELECT tracker_id FROM field_trackers t WHERE t.field_id = d.id ORDER BY tracker_id) AS trackers,
                ARRAY(SELECT project_id FROM field_projects p WHERE p.field_id = d.id ORDER BY project_id) AS projects
         FROM field_defs d WHERE d.index_state <> 'dropping' ORDER BY d.position, d.name",
    )
    .fetch_all(pool)
    .await?)
}

/// For fields with `visible = false`: the projects where the caller's
/// effective roles include one listed in `field_roles`. Effective roles are
/// the same as `db::role_ids_for` (non-admin): member roles where `principals`
/// (the user and their groups) are members; the builtin Non member role on
/// public projects they are not a member of; Anonymous on public projects when
/// not signed in. `get` uses the same rule, so filtering and display agree
/// (Redmine differs here: it filters by member roles only).
pub async fn readable_projects(
    conn: &mut sqlx::PgConnection,
    caller: Caller,
    principals: &[Uuid],
) -> Result<std::collections::HashMap<i32, Vec<Uuid>>> {
    let builtin = if caller.0.is_some() {
        "non_member"
    } else {
        "anonymous"
    };
    let rows: Vec<(i32, Vec<Uuid>)> = sqlx::query_as(
        "WITH mine AS (
             SELECT m.project_id, mr.role_id
             FROM members m JOIN member_roles mr ON mr.member_id = m.id
             WHERE m.principal_id = ANY($1)
         ), eff AS (
             SELECT project_id, role_id FROM mine
             UNION
             SELECT p.id, r.id FROM projects p JOIN roles r ON r.builtin = $2
             WHERE p.is_public
               AND NOT EXISTS (SELECT 1 FROM members m
                               WHERE m.project_id = p.id AND m.principal_id = ANY($1))
         )
         SELECT fr.field_id, array_agg(DISTINCT eff.project_id ORDER BY eff.project_id)
         FROM field_roles fr JOIN eff ON eff.role_id = fr.role_id
         GROUP BY fr.field_id",
    )
    .bind(principals)
    .bind(builtin)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().collect())
}
