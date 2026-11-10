// SPDX-License-Identifier: GPL-2.0-or-later
//! Custom field management, L0 (contract 05 §2.7, §3.7): create, change,
//! deactivate, delete, options, and the Q-S9 type conversion (a new field,
//! values copied in batches, the old field left for the administrator to
//! deactivate).
//!
//! Administrators only (custom fields are site-wide, as in Redmine). Every
//! change moves `schema_version` (0002/0007 triggers) and, when it changes
//! what a write must satisfy, the L0 part of the form rules version (0025);
//! visibility and scope changes bump the projects' `security_epoch` through
//! the 0010 triggers, so agent contexts that read them are quarantined (U1).
//!
//! A conversion writes the new field's values as `import` events with
//! `system_operation = cf_convert:<id>`; each batch commits its writes and
//! the resume cursor together, so a rerun after a failure never writes an
//! issue twice.

use crate::auth::{self, AuthContext, SessionPolicy};
use crate::commands::{self, SYSTEM_PRINCIPAL};
use crate::custom_fields;
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use ox_core::IssueFacts;
use ox_core::filter::{FieldType, Value};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::{PgConnection, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

/// `custom_field_conversions.state` (0025).
pub const CONVERSION_STATES: [&str; 5] =
    ["previewed", "running", "succeeded", "failed", "canceled"];

/// Issues per conversion batch.
static BATCH: AtomicUsize = AtomicUsize::new(500);

/// Tests: a smaller batch so a few issues span several batches.
#[cfg(feature = "test-support")]
pub fn set_batch_size(n: usize) {
    BATCH.store(n.max(1), Ordering::Relaxed);
}

/// Failures kept in a report (counts are exact).
const MAX_REPORTED: usize = 200;
/// Sample keys in a preview or impact.
const SAMPLE: usize = 10;

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- errors

fn invalid(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn not_found(what: &str) -> anyhow::Error {
    api(ErrorCode::NotFound, format!("no such {what}"))
}

/// Database errors a field change can run into, as API errors (the
/// database error may sit under added context).
fn map_db(e: anyhow::Error) -> anyhow::Error {
    let Some(sqlx::Error::Database(db)) = e.chain().find_map(|c| c.downcast_ref::<sqlx::Error>())
    else {
        return e;
    };
    let code = db.code().map(|c| c.to_string());
    let constraint = db.constraint().map(String::from);
    let message = db.message().to_string();
    match code.as_deref() {
        Some("23505") if constraint.as_deref() == Some("field_defs_name_uniq") => {
            invalid("name", "name_taken", "another custom field has this name")
        }
        Some("23505") if constraint.as_deref() == Some("field_options_label_uniq") => invalid(
            "label",
            "label_taken",
            "the field has an option with this label",
        ),
        Some("23503") if message.contains("is in use") => invalid(
            "option",
            "option_in_use",
            "issues use this option; deactivate it instead",
        ),
        Some("23514") if message.contains("builtin role") => invalid(
            "role_ids",
            "builtin_role",
            "built-in roles cannot be given custom field visibility",
        ),
        Some("23503") => invalid(
            "",
            "unknown_reference",
            "a tracker, project or role does not exist",
        ),
        _ => e,
    }
}

/// Only administrators (people) manage custom fields.
async fn admin(conn: &mut PgConnection, caller: AuthContext) -> Result<Uuid> {
    let who = commands::executor(&caller)?;
    if !db::is_admin(&mut *conn, caller).await? {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "only administrators manage custom fields",
        )
        .with_details(json!({"reason": "permission"}))
        .into());
    }
    Ok(who.user_id)
}

// ---------------------------------------------------------------- views

#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct OptionAdminView {
    pub id: Uuid,
    pub label: String,
    pub position: i32,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldAdminView {
    pub id: i32,
    /// `cf:<id>`, the key in command documents and forms.
    pub key: String,
    pub name: String,
    pub ftype: String,
    pub description: String,
    pub is_required: bool,
    pub is_for_all: bool,
    pub visible: bool,
    pub position: i32,
    /// pending (filters work without the index), ready, none (text), dropping.
    pub index_state: String,
    /// Deactivated: not in describe or forms and takes no new values; old
    /// values stay readable and filterable.
    pub deactivated: bool,
    pub deactivated_at: Option<DateTime<Utc>>,
    pub lock_version: i32,
    pub definition_version: i32,
    pub tracker_ids: Vec<Uuid>,
    pub project_ids: Vec<Uuid>,
    pub role_ids: Vec<Uuid>,
    pub options: Vec<OptionAdminView>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct FieldRow {
    id: i32,
    name: String,
    ftype: String,
    description: String,
    is_required: bool,
    is_for_all: bool,
    visible: bool,
    position: i32,
    index_state: String,
    deactivated_at: Option<DateTime<Utc>>,
    lock_version: i32,
    definition_version: i32,
    tracker_ids: Vec<Uuid>,
    project_ids: Vec<Uuid>,
    role_ids: Vec<Uuid>,
}

const FIELD_SQL: &str = "SELECT d.id, d.name, d.ftype, d.description, d.is_required, d.is_for_all, d.visible,
        d.position, d.index_state, d.deactivated_at, d.lock_version, d.definition_version,
        ARRAY(SELECT tracker_id FROM field_trackers WHERE field_id = d.id ORDER BY tracker_id) AS tracker_ids,
        ARRAY(SELECT project_id FROM field_projects WHERE field_id = d.id ORDER BY project_id) AS project_ids,
        ARRAY(SELECT role_id FROM field_roles WHERE field_id = d.id ORDER BY role_id) AS role_ids
 FROM field_defs d";

async fn view(conn: &mut PgConnection, r: FieldRow) -> Result<FieldAdminView> {
    let options: Vec<OptionAdminView> = sqlx::query_as(
        "SELECT id, label, position, active FROM field_options WHERE field_id = $1 ORDER BY position, label",
    )
    .bind(r.id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(FieldAdminView {
        id: r.id,
        key: format!("cf:{}", r.id),
        name: r.name,
        ftype: r.ftype,
        description: r.description,
        is_required: r.is_required,
        is_for_all: r.is_for_all,
        visible: r.visible,
        position: r.position,
        index_state: r.index_state,
        deactivated: r.deactivated_at.is_some(),
        deactivated_at: r.deactivated_at,
        lock_version: r.lock_version,
        definition_version: r.definition_version,
        tracker_ids: r.tracker_ids,
        project_ids: r.project_ids,
        role_ids: r.role_ids,
        options,
    })
}

async fn load(conn: &mut PgConnection, id: i32, lock: bool) -> Result<FieldRow> {
    let sql = format!(
        "{FIELD_SQL} WHERE d.id = $1{}",
        if lock { " FOR UPDATE OF d" } else { "" }
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| not_found("custom field"))
}

fn field_id(s: &str) -> Result<i32> {
    let s = s.strip_prefix("cf:").unwrap_or(s);
    s.parse::<i32>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| not_found("custom field"))
}

/// `GET /admin/custom-fields`.
pub async fn list(app: &App, caller: AuthContext) -> Result<Vec<FieldAdminView>> {
    let mut tx = read_tx(app).await?;
    admin(&mut tx, caller).await?;
    let rows: Vec<FieldRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{FIELD_SQL} ORDER BY d.position, d.name"
    )))
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(view(&mut tx, r).await?);
    }
    tx.commit().await?;
    Ok(out)
}

// ---------------------------------------------------------------- required impact (B-11)

/// Issues the field applies to that have no value: what making it required
/// would leave failing (only the caller's visible issues are counted).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RequiredImpact {
    pub violating: i64,
    pub sample_keys: Vec<String>,
    pub scanned: i64,
    pub complete: bool,
}

async fn required_impact(
    conn: &mut PgConnection,
    caller: AuthContext,
    id: i32,
) -> Result<RequiredImpact> {
    type Row = (Uuid, String, Uuid, Uuid, bool, Uuid, Option<Uuid>, bool);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id,
                EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = $1)
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         JOIN field_defs d ON d.id = $1
         WHERE i.archived_at IS NULL
           AND EXISTS (SELECT 1 FROM field_trackers ft WHERE ft.field_id = d.id AND ft.tracker_id = i.tracker_id)
           AND (d.is_for_all OR EXISTS (SELECT 1 FROM field_projects fp WHERE fp.field_id = d.id AND fp.project_id = i.project_id))
         ORDER BY i.id LIMIT $2",
    )
    .bind(id)
    .bind(crate::forms::SCAN_LIMIT + 1)
    .fetch_all(&mut *conn)
    .await?;
    let complete = rows.len() as i64 <= crate::forms::SCAN_LIMIT;
    let scope = db::load_scope(&mut *conn, caller).await?;
    let mut violating = 0;
    let mut sample_keys = Vec::new();
    let mut scanned = 0;
    for (_, key, p, t, private, author, assignee, has) in
        rows.into_iter().take(crate::forms::SCAN_LIMIT as usize)
    {
        scanned += 1;
        if has {
            continue;
        }
        let facts = IssueFacts {
            project_id: p,
            tracker_id: t,
            is_private: private,
            author_id: author,
            assigned_to_id: assignee,
        };
        if scope.can_see(&facts) {
            violating += 1;
            if sample_keys.len() < SAMPLE {
                sample_keys.push(key);
            }
        }
    }
    Ok(RequiredImpact {
        violating,
        sample_keys,
        scanned,
        complete,
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FieldDetail {
    pub field: FieldAdminView,
    /// `?preview_required=true`, or a PATCH that made the field required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_impact: Option<RequiredImpact>,
    /// PATCH: what the change does not block but the administrator should
    /// know (`restricted_condition_field`: published form rules name the
    /// field in conditions; people who cannot read it are no longer held
    /// to them, agents are blocked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Json>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FieldQuery {
    /// true: add `required_impact` (what `is_required = true` would leave failing).
    #[serde(default)]
    pub preview_required: Option<bool>,
}

/// `GET /admin/custom-fields/{id}`.
pub async fn get(app: &App, caller: AuthContext, id: &str, q: &FieldQuery) -> Result<FieldDetail> {
    let id = field_id(id)?;
    let mut tx = read_tx(app).await?;
    admin(&mut tx, caller).await?;
    let r = load(&mut tx, id, false).await?;
    let impact = if q.preview_required == Some(true) {
        Some(required_impact(&mut tx, caller, id).await?)
    } else {
        None
    };
    let field = view(&mut tx, r).await?;
    tx.commit().await?;
    Ok(FieldDetail {
        field,
        required_impact: impact,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- create

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionLabel {
    pub label: String,
}

/// `POST /admin/custom-fields` (and the new field of a conversion).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewFieldInput {
    pub name: String,
    /// int, float, date, string, text, list, user, bool, link.
    pub ftype: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_required: Option<bool>,
    #[serde(default)]
    pub is_for_all: Option<bool>,
    #[serde(default)]
    pub project_ids: Option<Vec<Uuid>>,
    pub tracker_ids: Vec<Uuid>,
    /// Default true. false: only administrators and holders of `role_ids`.
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub role_ids: Option<Vec<Uuid>>,
    /// list fields: 1–200 options.
    #[serde(default)]
    pub options: Option<Vec<OptionLabel>>,
}

fn check_name(path: &str, name: &str) -> Result<()> {
    let n = name.trim().chars().count();
    if n == 0 || n > 255 {
        return Err(invalid(path, "name_invalid", "a name is 1–255 characters"));
    }
    Ok(())
}

fn check_label(path: &str, label: &str) -> Result<()> {
    let n = label.trim().chars().count();
    if n == 0 || n > 255 {
        return Err(invalid(
            path,
            "label_invalid",
            "a label is 1–255 characters",
        ));
    }
    Ok(())
}

fn check_new(input: &NewFieldInput, prefix: &str) -> Result<FieldType> {
    check_name(&format!("{prefix}name"), &input.name)?;
    let Some(ty) = FieldType::parse(&input.ftype) else {
        return Err(ApiError::new(ErrorCode::InvalidValue, "unknown field type")
            .with_details(
                json!({"path": format!("{prefix}ftype"), "kind": "ftype_invalid",
                                 "allowed": FieldType::ALL.map(FieldType::as_str)}),
            )
            .into());
    };
    if input.tracker_ids.is_empty() {
        return Err(invalid(
            &format!("{prefix}tracker_ids"),
            "trackers_required",
            "a field applies to at least one tracker",
        ));
    }
    let options = input.options.as_deref().unwrap_or(&[]);
    if (ty == FieldType::List) == options.is_empty() || options.len() > 200 {
        return Err(invalid(
            &format!("{prefix}options"),
            "options_invalid",
            "list fields take 1–200 options; other types none",
        ));
    }
    for (i, o) in options.iter().enumerate() {
        check_label(&format!("{prefix}options[{i}].label"), &o.label)?;
    }
    if input.description.as_ref().is_some_and(|d| d.len() > 10_000) {
        return Err(invalid(
            &format!("{prefix}description"),
            "description_invalid",
            "a description is at most 10000 bytes",
        ));
    }
    Ok(ty)
}

async fn create_from(pool: &PgPool, input: &NewFieldInput, ty: FieldType) -> Result<i32> {
    let labels: Vec<&str> = input
        .options
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|o| o.label.trim())
        .collect();
    custom_fields::create_field(
        pool,
        custom_fields::NewField {
            name: input.name.trim(),
            ty,
            description: input.description.as_deref().unwrap_or(""),
            is_required: input.is_required.unwrap_or(false),
            is_for_all: input.is_for_all.unwrap_or(false),
            trackers: &input.tracker_ids,
            projects: input.project_ids.as_deref().unwrap_or(&[]),
            options: &labels,
            visible: input.visible.unwrap_or(true),
            roles: input.role_ids.as_deref().unwrap_or(&[]),
        },
    )
    .await
    .map_err(map_db)
}

/// `POST /admin/custom-fields`: 201; the index is built before returning
/// (`index_state` may still say pending if that failed: filters work).
pub async fn create(
    app: &App,
    caller: AuthContext,
    input: &NewFieldInput,
) -> Result<FieldAdminView> {
    {
        let mut c = app.pool.acquire().await?;
        admin(&mut c, caller).await?;
    }
    let ty = check_new(input, "")?;
    let id = create_from(&app.pool, input, ty).await?;
    let mut tx = read_tx(app).await?;
    let r = load(&mut tx, id, false).await?;
    let v = view(&mut tx, r).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- patch

/// What `PATCH /admin/custom-fields/{id}` may change.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldSet {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_required: Option<bool>,
    #[serde(default)]
    pub is_for_all: Option<bool>,
    #[serde(default)]
    pub project_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub tracker_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub role_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub deactivated: Option<bool>,
    /// Never changes: `unsupported_operation` (`details.suggested = "conversion"`).
    #[serde(default)]
    pub ftype: Option<Json>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldPatch {
    pub lock_version: i32,
    pub set: FieldSet,
}

fn conflict(current: i32) -> anyhow::Error {
    ApiError::new(
        ErrorCode::Conflict,
        "the field changed since you loaded it; reload it",
    )
    .with_details(json!({"current_lock_version": current}))
    .into()
}

/// `PATCH /admin/custom-fields/{id}`.
pub async fn patch(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &FieldPatch,
) -> Result<FieldDetail> {
    let id = field_id(id)?;
    let s = &input.set;
    if s.ftype.is_some() {
        return Err(ApiError::new(
            ErrorCode::UnsupportedOperation,
            "a field's type cannot change; convert it into a new field",
        )
        .with_details(json!({"path": "set.ftype", "suggested": "conversion"}))
        .into());
    }
    if let Some(n) = &s.name {
        check_name("set.name", n)?;
    }
    if s.tracker_ids.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid(
            "set.tracker_ids",
            "trackers_required",
            "a field applies to at least one tracker",
        ));
    }
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    let before = load(&mut tx, id, true).await?;
    if before.index_state == "dropping" {
        return Err(not_found("custom field"));
    }
    if before.lock_version != input.lock_version {
        return Err(conflict(before.lock_version));
    }
    let r = async {
        sqlx::query(
            "UPDATE field_defs SET
                name = COALESCE($2, name), description = COALESCE($3, description),
                is_required = COALESCE($4, is_required), is_for_all = COALESCE($5, is_for_all),
                visible = COALESCE($6, visible), position = COALESCE($7, position),
                deactivated_at = CASE WHEN $8::boolean IS NULL THEN deactivated_at
                                      WHEN $8 THEN COALESCE(deactivated_at, now()) ELSE NULL END,
                lock_version = lock_version + 1
             WHERE id = $1",
        )
        .bind(id)
        .bind(s.name.as_ref().map(|n| n.trim().to_string()))
        .bind(&s.description)
        .bind(s.is_required)
        .bind(s.is_for_all)
        .bind(s.visible)
        .bind(s.position)
        .bind(s.deactivated)
        .execute(&mut *tx)
        .await?;
        // Sets are replaced only when they differ (a no-op must not bump
        // the epoch through the 0010 triggers).
        if let Some(t) = &s.tracker_ids {
            let mut t = t.clone();
            t.sort();
            t.dedup();
            if t != before.tracker_ids {
                sqlx::query("DELETE FROM field_trackers WHERE field_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO field_trackers (field_id, tracker_id) SELECT $1, unnest($2::uuid[])")
                    .bind(id)
                    .bind(&t)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if let Some(p) = &s.project_ids {
            let mut p = p.clone();
            p.sort();
            p.dedup();
            if p != before.project_ids {
                sqlx::query("DELETE FROM field_projects WHERE field_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO field_projects (field_id, project_id) SELECT $1, unnest($2::uuid[])")
                    .bind(id)
                    .bind(&p)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if let Some(rs) = &s.role_ids {
            let mut rs = rs.clone();
            rs.sort();
            rs.dedup();
            if rs != before.role_ids {
                sqlx::query("DELETE FROM field_roles WHERE field_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO field_roles (field_id, role_id) SELECT $1, unnest($2::uuid[])")
                    .bind(id)
                    .bind(&rs)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        anyhow::Ok(())
    }
    .await;
    r.map_err(map_db)?;
    let impact = if s.is_required == Some(true) && !before.is_required {
        Some(required_impact(&mut tx, caller, id).await?)
    } else {
        None
    };
    let after = load(&mut tx, id, false).await?;
    let mut warnings = Vec::new();
    if before.visible && !after.visible {
        for (form, rule_ids) in crate::forms::rules_conditioned_on(&mut tx, id).await? {
            warnings.push(
                json!({"kind": "restricted_condition_field", "field": format!("cf:{id}"),
                                 "form_id": form, "rule_ids": rule_ids}),
            );
        }
    }
    let field = view(&mut tx, after).await?;
    tx.commit().await?;
    Ok(FieldDetail {
        field,
        required_impact: impact,
        warnings,
    })
}

// ---------------------------------------------------------------- options

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOption {
    pub label: String,
    /// Default: after the last option.
    #[serde(default)]
    pub position: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionPatch {
    /// The field's lock_version.
    pub lock_version: i32,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    /// false: kept on issues that have it, not offered for new values.
    #[serde(default)]
    pub active: Option<bool>,
}

async fn list_field(conn: &mut PgConnection, id: i32) -> Result<FieldRow> {
    let r = load(&mut *conn, id, true).await?;
    if r.ftype != "list" {
        return Err(invalid("", "not_a_list", "only list fields have options"));
    }
    if r.index_state == "dropping" {
        return Err(not_found("custom field"));
    }
    Ok(r)
}

async fn bump(conn: &mut PgConnection, id: i32) -> Result<()> {
    sqlx::query("UPDATE field_defs SET lock_version = lock_version + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `POST /admin/custom-fields/{id}/options`.
pub async fn add_option(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &NewOption,
) -> Result<FieldAdminView> {
    let id = field_id(id)?;
    check_label("label", &input.label)?;
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    list_field(&mut tx, id).await?;
    sqlx::query(
        "INSERT INTO field_options (field_id, label, position)
         VALUES ($1, $2, COALESCE($3, (SELECT COALESCE(max(position), 0) + 1 FROM field_options WHERE field_id = $1)))",
    )
    .bind(id)
    .bind(input.label.trim())
    .bind(input.position)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_db(e.into()))?;
    bump(&mut tx, id).await?;
    let r = load(&mut tx, id, false).await?;
    let v = view(&mut tx, r).await?;
    tx.commit().await?;
    Ok(v)
}

/// `PATCH /admin/custom-fields/{id}/options/{oid}`.
pub async fn patch_option(
    app: &App,
    caller: AuthContext,
    id: &str,
    option: Uuid,
    input: &OptionPatch,
) -> Result<FieldAdminView> {
    let id = field_id(id)?;
    if let Some(l) = &input.label {
        check_label("label", l)?;
    }
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    let f = list_field(&mut tx, id).await?;
    if f.lock_version != input.lock_version {
        return Err(conflict(f.lock_version));
    }
    let n = sqlx::query(
        "UPDATE field_options SET label = COALESCE($3, label), position = COALESCE($4, position),
                active = COALESCE($5, active)
         WHERE id = $2 AND field_id = $1",
    )
    .bind(id)
    .bind(option)
    .bind(input.label.as_ref().map(|l| l.trim().to_string()))
    .bind(input.position)
    .bind(input.active)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_db(e.into()))?
    .rows_affected();
    if n == 0 {
        return Err(not_found("option"));
    }
    bump(&mut tx, id).await?;
    let r = load(&mut tx, id, false).await?;
    let v = view(&mut tx, r).await?;
    tx.commit().await?;
    Ok(v)
}

/// `DELETE /admin/custom-fields/{id}/options/{oid}?lock_version=`: only an
/// option no issue uses (`invalid_value`, `details.kind = "option_in_use"`).
pub async fn delete_option(
    app: &App,
    caller: AuthContext,
    id: &str,
    option: Uuid,
    lock_version: i32,
) -> Result<FieldAdminView> {
    let id = field_id(id)?;
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    let f = list_field(&mut tx, id).await?;
    if f.lock_version != lock_version {
        return Err(conflict(f.lock_version));
    }
    let n = sqlx::query("DELETE FROM field_options WHERE id = $2 AND field_id = $1")
        .bind(id)
        .bind(option)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_db(e.into()))?
        .rows_affected();
    if n == 0 {
        return Err(not_found("option"));
    }
    bump(&mut tx, id).await?;
    let r = load(&mut tx, id, false).await?;
    let v = view(&mut tx, r).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- delete

/// `DELETE /admin/custom-fields/{id}?lock_version=` (step-up): marks the
/// field `dropping` now (new values are refused from here on) and deletes
/// values, index and definition in the background (also finished by the
/// periodic index job after a restart). 202.
pub async fn delete(
    app: &App,
    policy: &SessionPolicy,
    caller: AuthContext,
    id: &str,
    lock_version: i32,
) -> Result<FieldAdminView> {
    let id = field_id(id)?;
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    auth::require_step_up(&mut tx, policy, &caller).await?;
    let f = load(&mut tx, id, true).await?;
    if f.lock_version != lock_version {
        return Err(conflict(f.lock_version));
    }
    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM custom_field_conversions
                        WHERE (from_field_id = $1 OR to_field_id = $1) AND state = 'running')",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if running {
        return Err(
            ApiError::new(ErrorCode::Conflict, "a conversion of this field is running")
                .with_details(json!({"kind": "conversion_running"}))
                .into(),
        );
    }
    sqlx::query(
        "UPDATE field_defs SET index_state = 'dropping', lock_version = lock_version + 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let r = load(&mut tx, id, false).await?;
    let v = view(&mut tx, r).await?;
    tx.commit().await?;
    let pool = app.pool.clone();
    tokio::spawn(async move {
        if let Err(e) = custom_fields::drop_field(&pool, id).await {
            tracing::warn!("dropping custom field {id}: {e:#} (the index job retries)");
        }
    });
    Ok(v)
}

// ---------------------------------------------------------------- conversions (Q-S9)

/// `POST /admin/custom-fields/{id}/conversions`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversionInput {
    /// The new field (created now; values are copied by `start`).
    pub to: NewFieldInput,
    /// list → list: `{"options": {"<old option id>": "<new label>"}}`;
    /// string/link/text → list: `{"values": {"<text>": "<new label>"}}`;
    /// string/link/text → int/float/date/bool and the textual targets: `{}`
    /// (parsed; list → string uses the option label).
    #[serde(default)]
    pub mapping: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversionFailures {
    pub count: i64,
    /// Visible issues only.
    pub sample_issue_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversionPreview {
    pub conversion_id: Uuid,
    pub to_field_id: i32,
    /// Values that convert.
    pub convertible: i64,
    pub failures: ConversionFailures,
    pub mapping_echo: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversionView {
    pub id: Uuid,
    pub from_field_id: i32,
    pub to_field_id: i32,
    /// previewed, running, succeeded, failed, canceled.
    pub state: String,
    pub batches_done: i32,
    /// `{converted, failed, skipped, failures: [{issue_id, key, reason}]}`:
    /// reason codes (`unmapped`, `unparseable`, `too_long`), never values.
    pub report: Option<Json>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// One conversion rule, decided at preview from the two types and the mapping.
#[derive(Debug, Clone)]
enum Rule {
    /// Old option id → new option id.
    Options(BTreeMap<Uuid, Uuid>),
    /// Trimmed text → new option id.
    Values(BTreeMap<String, Uuid>),
    /// Text parsed into the new type.
    Parse(FieldType),
    /// int → float.
    Widen,
    /// To string/text/link: the old value as text (list: its label).
    Text(FieldType, BTreeMap<Uuid, String>),
}

fn unsupported(from: FieldType, to: FieldType) -> anyhow::Error {
    ApiError::new(
        ErrorCode::UnsupportedOperation,
        format!("no conversion from {} to {}", from.as_str(), to.as_str()),
    )
    .with_details(json!({"path": "to.ftype"}))
    .into()
}

fn mapping_err(msg: impl Into<String>) -> anyhow::Error {
    invalid("mapping", "mapping_invalid", msg)
}

/// The rule for (from, to) and the mapping; `labels` are the new field's
/// options (label → id), `old_labels` the old field's (id → label).
fn rule_for(
    from: FieldType,
    to: FieldType,
    mapping: &Json,
    labels: &BTreeMap<String, Uuid>,
    old_labels: &BTreeMap<Uuid, String>,
) -> Result<Rule> {
    use FieldType as T;
    let label_id = |l: &Json| -> Result<Uuid> {
        let l = l
            .as_str()
            .ok_or_else(|| mapping_err("mapping targets are option labels"))?;
        labels
            .get(&l.trim().to_lowercase())
            .copied()
            .ok_or_else(|| mapping_err(format!("{l:?} is not an option of the new field")))
    };
    let textual = |t| matches!(t, T::String | T::Text | T::Link);
    match (from, to) {
        (T::List, T::List) => {
            let Some(Json::Object(m)) = mapping.get("options") else {
                return Err(mapping_err(
                    "list → list needs {\"options\": {old option id: new label}}",
                ));
            };
            let mut out = BTreeMap::new();
            for (k, v) in m {
                let old = Uuid::parse_str(k)
                    .ok()
                    .filter(|u| old_labels.contains_key(u))
                    .ok_or_else(|| mapping_err(format!("{k} is not an option of the old field")))?;
                out.insert(old, label_id(v)?);
            }
            Ok(Rule::Options(out))
        }
        (f, T::List) if textual(f) => {
            let Some(Json::Object(m)) = mapping.get("values") else {
                return Err(mapping_err(
                    "text → list needs {\"values\": {text: new label}}",
                ));
            };
            let mut out = BTreeMap::new();
            for (k, v) in m {
                out.insert(k.trim().to_string(), label_id(v)?);
            }
            Ok(Rule::Values(out))
        }
        (f, T::Int | T::Float | T::Date | T::Bool) if textual(f) => Ok(Rule::Parse(to)),
        (T::Int, T::Float) => Ok(Rule::Widen),
        (T::Int | T::Float | T::Date | T::List | T::String | T::Link, t) if textual(t) => {
            Ok(Rule::Text(to, old_labels.clone()))
        }
        (T::Text, T::String | T::Link) => Ok(Rule::Text(to, old_labels.clone())),
        _ => Err(unsupported(from, to)),
    }
}

/// An old value as stored (the typed `issue_cf` columns).
#[derive(Debug, Clone, sqlx::FromRow)]
struct OldValue {
    issue_id: Uuid,
    v_num: Option<String>,
    v_date: Option<NaiveDate>,
    v_text: Option<String>,
    v_ref: Option<Uuid>,
    v_bool: Option<bool>,
}

/// The new value, or why not (a reason code).
fn convert(rule: &Rule, v: &OldValue) -> std::result::Result<Value, &'static str> {
    match rule {
        Rule::Options(m) => v
            .v_ref
            .and_then(|r| m.get(&r))
            .map(|n| Value::Ref(*n))
            .ok_or("unmapped"),
        Rule::Values(m) => v
            .v_text
            .as_deref()
            .and_then(|t| m.get(t.trim()))
            .map(|n| Value::Ref(*n))
            .ok_or("unmapped"),
        Rule::Parse(t) => {
            let s = v.v_text.as_deref().unwrap_or("").trim();
            match t {
                FieldType::Int | FieldType::Float => {
                    match commands::parse_cf("", &json!({ t.as_str(): s })) {
                        Ok((_, c)) if !c.is_null() => c[t.as_str()]
                            .as_str()
                            .and_then(|x| x.parse::<f64>().ok())
                            .map(Value::Num)
                            .ok_or("unparseable"),
                        _ => Err("unparseable"),
                    }
                }
                FieldType::Date => s
                    .parse::<NaiveDate>()
                    .map(Value::Date)
                    .map_err(|_| "unparseable"),
                _ => match s.to_lowercase().as_str() {
                    "true" | "1" | "yes" => Ok(Value::Bool(true)),
                    "false" | "0" | "no" => Ok(Value::Bool(false)),
                    _ => Err("unparseable"),
                },
            }
        }
        Rule::Widen => v
            .v_num
            .as_deref()
            .and_then(|n| n.parse::<f64>().ok())
            .map(Value::Num)
            .ok_or("unparseable"),
        Rule::Text(to, labels) => {
            let text = if let Some(t) = &v.v_text {
                t.clone()
            } else if let Some(n) = &v.v_num {
                n.clone()
            } else if let Some(d) = v.v_date {
                d.to_string()
            } else if let Some(r) = v.v_ref {
                labels.get(&r).cloned().ok_or("unmapped")?
            } else if let Some(b) = v.v_bool {
                b.to_string()
            } else {
                return Err("unparseable");
            };
            if *to != FieldType::Text && text.len() > 2000 {
                return Err("too_long");
            }
            Ok(Value::Text(text))
        }
    }
}

/// The canonical command-form JSON of a value of type `t` (event payloads).
fn canonical(t: FieldType, v: &Value) -> Json {
    match v {
        Value::Num(n) => {
            let s = if t == FieldType::Int {
                format!("{}", *n as i64)
            } else {
                commands::parse_cf("", &json!({"float": n.to_string()}))
                    .ok()
                    .and_then(|(_, c)| c["float"].as_str().map(String::from))
                    .unwrap_or_else(|| n.to_string())
            };
            json!({ t.as_str(): s })
        }
        Value::Date(d) => json!({"date": d.to_string()}),
        Value::Text(s) => json!({ t.as_str(): s }),
        Value::Ref(u) => json!({ t.as_str(): u }),
        Value::Bool(b) => json!({"bool": b}),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ConvRow {
    id: Uuid,
    from_field_id: i32,
    to_field_id: i32,
    mapping: Json,
    state: String,
    batches_done: i32,
    cursor_issue_id: Option<Uuid>,
    report: Option<Json>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

const CONV_COLS: &str =
    "id, from_field_id, to_field_id, mapping, state, batches_done, cursor_issue_id,
                         report, created_at, started_at, finished_at";

fn conv_view(r: &ConvRow) -> ConversionView {
    ConversionView {
        id: r.id,
        from_field_id: r.from_field_id,
        to_field_id: r.to_field_id,
        state: r.state.clone(),
        batches_done: r.batches_done,
        report: r.report.clone(),
        created_at: r.created_at,
        started_at: r.started_at,
        finished_at: r.finished_at,
    }
}

async fn options_of(conn: &mut PgConnection, field: i32) -> Result<Vec<(Uuid, String)>> {
    Ok(
        sqlx::query_as("SELECT id, label FROM field_options WHERE field_id = $1")
            .bind(field)
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// The rule of a stored conversion.
async fn rule_of(conn: &mut PgConnection, c: &ConvRow) -> Result<Rule> {
    let types: Vec<(i32, String)> =
        sqlx::query_as("SELECT id, ftype FROM field_defs WHERE id = ANY($1)")
            .bind([c.from_field_id, c.to_field_id])
            .fetch_all(&mut *conn)
            .await?;
    let ty = |id: i32| {
        types
            .iter()
            .find(|t| t.0 == id)
            .and_then(|t| FieldType::parse(&t.1))
            .ok_or_else(|| not_found("custom field"))
    };
    let (from, to) = (ty(c.from_field_id)?, ty(c.to_field_id)?);
    let labels: BTreeMap<String, Uuid> = options_of(&mut *conn, c.to_field_id)
        .await?
        .into_iter()
        .map(|(id, l)| (l.to_lowercase(), id))
        .collect();
    let old: BTreeMap<Uuid, String> = options_of(&mut *conn, c.from_field_id)
        .await?
        .into_iter()
        .collect();
    rule_for(from, to, &c.mapping, &labels, &old)
}

const OLD_SQL: &str =
    "SELECT issue_id, v_num::text AS v_num, v_date, v_text, v_ref, v_bool FROM issue_cf";

/// `POST /admin/custom-fields/{id}/conversions`: creates the new field and
/// reports what would convert. Nothing is copied yet.
pub async fn preview_conversion(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &ConversionInput,
) -> Result<ConversionPreview> {
    let from_id = field_id(id)?;
    let user = {
        let mut c = app.pool.acquire().await?;
        admin(&mut c, caller).await?
    };
    let to_ty = {
        let mut tx = read_tx(app).await?;
        let from = load(&mut tx, from_id, false).await?;
        if from.index_state == "dropping" {
            return Err(not_found("custom field"));
        }
        let to_ty = check_new(&input.to, "to.")?;
        let from_ty = FieldType::parse(&from.ftype).ok_or_else(|| not_found("custom field"))?;
        // The mapping is checked against the new options before the field exists.
        let labels: BTreeMap<String, Uuid> = input
            .to
            .options
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|o| (o.label.trim().to_lowercase(), Uuid::nil()))
            .collect();
        let old: BTreeMap<Uuid, String> = options_of(&mut tx, from_id).await?.into_iter().collect();
        rule_for(from_ty, to_ty, &input.mapping, &labels, &old)?;
        tx.commit().await?;
        to_ty
    };
    let to_id = create_from(&app.pool, &input.to, to_ty).await?;
    let mut tx = app.pool.begin().await?;
    let conv: ConvRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO custom_field_conversions (from_field_id, to_field_id, mapping, state, created_by)
         VALUES ($1, $2, $3, 'previewed', $4) RETURNING {CONV_COLS}"
    )))
    .bind(from_id)
    .bind(to_id)
    .bind(&input.mapping)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    let rule = rule_of(&mut tx, &conv).await?;
    let olds: Vec<OldValue> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{OLD_SQL} WHERE field_id = {from_id} ORDER BY issue_id"
    )))
    .fetch_all(&mut *tx)
    .await?;
    let mut convertible = 0;
    let mut failing = Vec::new();
    for o in &olds {
        match convert(&rule, o) {
            Ok(_) => convertible += 1,
            Err(_) => failing.push(o.issue_id),
        }
    }
    let (_, sample) = visible_keys(&mut tx, caller, &failing).await?;
    tx.commit().await?;
    Ok(ConversionPreview {
        conversion_id: conv.id,
        to_field_id: to_id,
        convertible,
        failures: ConversionFailures {
            count: failing.len() as i64,
            sample_issue_keys: sample,
        },
        mapping_echo: input.mapping.clone(),
    })
}

async fn visible_keys(
    conn: &mut PgConnection,
    caller: AuthContext,
    ids: &[Uuid],
) -> Result<(i64, Vec<String>)> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let rows: Vec<(String, Uuid, Uuid, bool, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT k.key, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = ANY($1) ORDER BY i.id",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut n = 0;
    let mut keys = Vec::new();
    for (key, p, t, private, author, assignee) in rows {
        let f = IssueFacts {
            project_id: p,
            tracker_id: t,
            is_private: private,
            author_id: author,
            assigned_to_id: assignee,
        };
        if scope.can_see(&f) {
            n += 1;
            if keys.len() < SAMPLE {
                keys.push(key);
            }
        }
    }
    Ok((n, keys))
}

async fn load_conv(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<ConvRow> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CONV_COLS} FROM custom_field_conversions WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| not_found("conversion"))
}

/// `GET /admin/conversions/{id}`.
pub async fn get_conversion(app: &App, caller: AuthContext, id: Uuid) -> Result<ConversionView> {
    let mut tx = read_tx(app).await?;
    admin(&mut tx, caller).await?;
    let c = load_conv(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(conv_view(&c))
}

/// `POST /admin/conversions/{id}/start` (step-up): from `previewed`, or
/// again from `failed` (it resumes at its cursor). The copy runs in the
/// background; `GET` shows progress.
pub async fn start_conversion(
    app: &App,
    policy: &SessionPolicy,
    caller: AuthContext,
    id: Uuid,
) -> Result<ConversionView> {
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    auth::require_step_up(&mut tx, policy, &caller).await?;
    let c = load_conv(&mut tx, id, true).await?;
    if c.state != "previewed" && c.state != "failed" {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            format!("the conversion is {}", c.state),
        )
        .with_details(json!({"state": c.state}))
        .into());
    }
    // Both fields must be live: not being dropped, not deactivated.
    for (which, fid) in [("from", c.from_field_id), ("to", c.to_field_id)] {
        let f = load(&mut tx, fid, true).await?;
        if f.index_state == "dropping" || f.deactivated_at.is_some() {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("the {which} field is deactivated or being deleted"),
            )
            .with_details(json!({"field": format!("cf:{fid}"), "kind": "field_inactive"}))
            .into());
        }
    }
    sqlx::query(
        "UPDATE custom_field_conversions SET state = 'running', started_at = COALESCE(started_at, now()),
                finished_at = NULL WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let c = load_conv(&mut tx, id, false).await?;
    tx.commit().await?;
    spawn_run(app.pool.clone(), id);
    Ok(conv_view(&c))
}

/// Conversions this process is running (so the periodic job does not start
/// a second runner for one still going).
static ACTIVE: std::sync::Mutex<BTreeSet<Uuid>> = std::sync::Mutex::new(BTreeSet::new());

/// Runs a conversion in its own task; an error stays with it (logged, the
/// conversion `failed`). `false` when this process already runs it.
fn spawn_run(pool: PgPool, id: Uuid) -> bool {
    if !ACTIVE.lock().map(|mut a| a.insert(id)).unwrap_or(false) {
        return false;
    }
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_conversion(&pool, id).await {
                tracing::warn!("custom field conversion {id}: {e:#}");
            }
            if let Ok(mut a) = ACTIVE.lock() {
                a.remove(&id);
            }
            // Started again while this runner was finishing (its spawn saw
            // this one still active): run it, unless another runner has.
            let running: bool = sqlx::query_scalar(
                "SELECT state = 'running' FROM custom_field_conversions WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
            if !running || !ACTIVE.lock().map(|mut a| a.insert(id)).unwrap_or(false) {
                break;
            }
        }
    });
    true
}

/// Runs a `running` conversion to the end, batch by batch. A batch locks
/// the conversion row, converts the issues after the cursor, and commits
/// them with the new cursor; an error marks it `failed` (start resumes).
pub async fn run_conversion(pool: &PgPool, id: Uuid) -> Result<()> {
    loop {
        match batch(pool, id).await {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(e) => {
                sqlx::query(
                    "UPDATE custom_field_conversions SET state = 'failed', finished_at = now()
                     WHERE id = $1 AND state = 'running'",
                )
                .bind(id)
                .execute(pool)
                .await?;
                return Err(e);
            }
        }
    }
}

/// Picks up conversions left `running` (a restart): the periodic field job.
/// Each runs in its own task, so a long or failing one neither holds up the
/// index job nor the other conversions. Returns how many were started.
pub async fn resume_conversions(pool: &PgPool) -> Result<usize> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM custom_field_conversions WHERE state = 'running' ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(ids
        .into_iter()
        .filter(|id| spawn_run(pool.clone(), *id))
        .count())
}

/// One batch; `Ok(false)` when the conversion is done (or not running).
async fn batch(pool: &PgPool, id: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let c = load_conv(&mut tx, id, true).await?;
    if c.state != "running" {
        return Ok(false);
    }
    let rule = rule_of(&mut tx, &c).await?;
    let to_ty: String = sqlx::query_scalar("SELECT ftype FROM field_defs WHERE id = $1")
        .bind(c.to_field_id)
        .fetch_one(&mut *tx)
        .await?;
    let to_ty = FieldType::parse(&to_ty).ok_or_else(|| not_found("custom field"))?;
    let size = BATCH.load(Ordering::Relaxed) as i64;
    // The batch's issues first, locked in id order (as writers lock them),
    // then their old values: a write committed meanwhile is either seen or
    // waits for this batch.
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT issue_id FROM issue_cf WHERE field_id = $3 AND ($1::uuid IS NULL OR issue_id > $1)
         ORDER BY issue_id LIMIT $2",
    )
    .bind(c.cursor_issue_id)
    .bind(size)
    .bind(c.from_field_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("SELECT id FROM issues WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    let olds: Vec<OldValue> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{OLD_SQL} WHERE field_id = {} AND issue_id = ANY($1) ORDER BY issue_id",
        c.from_field_id
    )))
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    let mut report = c.report.clone().unwrap_or_else(
        || json!({"converted": 0, "failed": 0, "skipped": 0, "not_applicable": 0, "failures": []}),
    );
    if ids.is_empty() {
        sqlx::query(
            "UPDATE custom_field_conversions SET state = 'succeeded', finished_at = now(), report = $2
             WHERE id = $1",
        )
        .bind(id)
        .bind(&report)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(false);
    }
    if c.batches_done > 0 {
        fault("cf_convert.batch").await?;
    }
    let op = format!("cf_convert:{id}");
    let key = format!("cf:{}", c.to_field_id);
    let last = ids.last().copied().or(c.cursor_issue_id);
    for o in &olds {
        let row: Option<(i64, bool, String, bool)> = sqlx::query_as(
            "SELECT i.last_event_seq,
                    EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = $2),
                    (SELECT key FROM issue_keys k WHERE k.issue_id = i.id AND k.is_current),
                    EXISTS (SELECT 1 FROM field_defs d
                            JOIN field_trackers ft ON ft.field_id = d.id AND ft.tracker_id = i.tracker_id
                            WHERE d.id = $2
                              AND (d.is_for_all OR EXISTS (SELECT 1 FROM field_projects fp
                                                           WHERE fp.field_id = d.id
                                                             AND fp.project_id = i.project_id)))
             FROM issues i WHERE i.id = $1",
        )
        .bind(o.issue_id)
        .bind(c.to_field_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((seq, has, issue_key, applies)) = row else {
            continue;
        };
        if has {
            report["skipped"] = json!(report["skipped"].as_i64().unwrap_or(0) + 1);
            continue;
        }
        if !applies {
            // The new field does not apply to this issue: nothing is written.
            report["not_applicable"] = json!(report["not_applicable"].as_i64().unwrap_or(0) + 1);
            continue;
        }
        let v = match convert(&rule, o) {
            Ok(v) => v,
            Err(reason) => {
                report["failed"] = json!(report["failed"].as_i64().unwrap_or(0) + 1);
                if let Some(Json::Array(f)) = report.get_mut("failures")
                    && f.len() < MAX_REPORTED
                {
                    f.push(json!({"issue_id": o.issue_id, "key": issue_key, "reason": reason}));
                }
                continue;
            }
        };
        custom_fields::set_value(&mut *tx, o.issue_id, c.to_field_id, &v).await?;
        let payload = json!({"changes": {key.clone(): {"old": null, "new": canonical(to_ty, &v)}}});
        let event_id: Uuid = sqlx::query_scalar(
            "WITH ev AS (
               INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind,
                                         system_operation, payload)
               VALUES ($1, $2, 'fields_changed', 'import', $3, 'system', $4, $5)
               RETURNING id, issue_id, kind, payload, at
             ), projected AS (
               INSERT INTO issue_field_changes (event_id, issue_id, field_key, old_ref, new_ref, at)
               SELECT ev.id, ev.issue_id, r.field_key, r.old_ref, r.new_ref, ev.at
               FROM ev, issue_field_change_rows(ev.kind, ev.payload) r
             )
             SELECT id FROM ev",
        )
        .bind(o.issue_id)
        .bind(seq + 1)
        .bind(SYSTEM_PRINCIPAL)
        .bind(&op)
        .bind(&payload)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO issue_field_origins (issue_id, field_key, origin_execution_id, origin_event_id)
             VALUES ($1, $2, NULL, $3)
             ON CONFLICT (issue_id, field_key) DO UPDATE
               SET origin_execution_id = NULL, origin_event_id = EXCLUDED.origin_event_id",
        )
        .bind(o.issue_id)
        .bind(&key)
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE issues SET lock_version = lock_version + 1, last_event_seq = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(o.issue_id)
        .bind(seq + 1)
        .execute(&mut *tx)
        .await?;
        report["converted"] = json!(report["converted"].as_i64().unwrap_or(0) + 1);
    }
    sqlx::query(
        "UPDATE custom_field_conversions SET cursor_issue_id = $2, batches_done = batches_done + 1,
                report = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(last)
    .bind(&report)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
