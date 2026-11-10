// SPDX-License-Identifier: GPL-2.0-or-later
//! Concepts and their labels (design 05 §2.9, §3.9, §6.13): the data
//! behind query-side alias expansion, and its administration.
//!
//! * Concepts and labels are public site data: every signed-in person
//!   reads them (`GET /admin/concepts`); people who are not administrators
//!   see active concepts and active labels only. Only administrators
//!   change them. No permission or `security_epoch` is involved.
//! * Labels are compared after NFC + casefold, always with the SQL
//!   expression `casefold(normalize(x, NFC))` — the one the unique index
//!   `aliases_concept_label_uq` uses (appendix C R18).
//! * One label belongs to one active label row among all concepts
//!   (`concept_label_taken`); `staging` labels never expand.

use crate::auth::AuthContext;
use crate::cursor::{self, Position};
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// Longest label and preferred label, in code points.
pub const MAX_LABEL: usize = 60;
/// Labels per concept, the preferred one included.
pub const MAX_LABELS: usize = 50;
/// Expanded labels per query (bounds the index query).
pub const MAX_EXPANSIONS: i64 = 32;
/// Query pieces looked up as labels (the whole query and its words).
const MAX_PIECES: usize = 16;
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 100;

/// One label of a concept.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LabelView {
    pub label: String,
    /// pref (the concept's name) | alt | hidden (a misspelling or internal
    /// term: matched and expanded like alt, listed to administrators only;
    /// `match.expanded` shows the concept's name in its place).
    pub kind: String,
    /// active | staging (staging labels are not expanded).
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConceptView {
    pub id: Uuid,
    pub pref_label: String,
    /// active | retired (a retired concept does not expand).
    pub status: String,
    /// Pass back to `PATCH /admin/concepts/{id}`; every label change bumps it.
    pub lock_version: i32,
    /// The preferred label first, then by label. People who are not
    /// administrators see active labels only.
    pub labels: Vec<LabelView>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConceptList {
    pub items: Vec<ConceptView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConceptQuery {
    /// Part of a label or of the preferred label (NFC + casefold).
    #[serde(default)]
    pub q: Option<String>,
    /// Default 50, at most 100.
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewLabel {
    pub label: String,
    /// alt (default) | hidden.
    #[serde(default)]
    pub kind: Option<String>,
    /// active (default) | staging.
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewConcept {
    /// 1–60 characters; also an active label of the concept (kind pref).
    pub pref_label: String,
    #[serde(default)]
    pub labels: Vec<NewLabel>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelPatch {
    /// active | staging.
    #[serde(default)]
    pub status: Option<String>,
    /// alt | hidden (the preferred label cannot change kind or status).
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConceptPatch {
    pub lock_version: i32,
    /// Renames the concept (its pref label).
    #[serde(default)]
    pub pref_label: Option<String>,
    /// active | retired.
    #[serde(default)]
    pub status: Option<String>,
}

// ---------------------------------------------------------------- helpers

fn invalid(path: &str, kind: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such concept")
}

/// Only administrators (people) manage concepts.
async fn admin(conn: &mut PgConnection, caller: AuthContext) -> Result<Uuid> {
    let who = crate::commands::executor(&caller)?;
    if !db::is_admin(&mut *conn, caller).await? {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "only administrators manage concepts and labels",
        )
        .with_details(json!({"reason": "permission"}))
        .into());
    }
    Ok(who.user_id)
}

/// A label as stored (trimmed, NFC) and its comparison key (NFC + casefold).
struct Norm {
    label: String,
    key: String,
}

async fn norm(conn: &mut PgConnection, path: &str, raw: &str) -> Result<Norm> {
    let t = raw.trim();
    if t.is_empty() || t.chars().any(char::is_control) {
        return Err(invalid(
            path,
            "label_invalid",
            "a label is 1–60 characters without control characters",
        ));
    }
    let (label, key): (String, String) =
        sqlx::query_as("SELECT normalize($1, NFC), casefold(normalize($1, NFC))")
            .bind(t)
            .fetch_one(&mut *conn)
            .await?;
    if label.chars().count() > MAX_LABEL {
        return Err(invalid(
            path,
            "label_invalid",
            "a label is 1–60 characters without control characters",
        ));
    }
    Ok(Norm { label, key })
}

fn label_kind(path: &str, k: Option<&str>) -> Result<&'static str> {
    match k {
        None | Some("alt") => Ok("alt"),
        Some("hidden") => Ok("hidden"),
        Some(_) => Err(invalid(path, "label_kind_invalid", "kind is alt or hidden")),
    }
}

fn label_status(path: &str, s: Option<&str>) -> Result<&'static str> {
    match s {
        None | Some("active") => Ok("active"),
        Some("staging") => Ok("staging"),
        Some(_) => Err(invalid(
            path,
            "label_status_invalid",
            "status is active or staging",
        )),
    }
}

/// The concept whose active label has this key, other than `except`.
async fn holder(
    conn: &mut PgConnection,
    key: &str,
    except: Option<Uuid>,
) -> Result<Option<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.pref_label FROM aliases a JOIN concepts c ON c.id = a.entity_id
         WHERE a.entity_kind = 'concept' AND a.status = 'active'
           AND casefold(normalize(a.label, NFC)) = $1
           AND ($2::uuid IS NULL OR a.entity_id <> $2)
         LIMIT 1",
    )
    .bind(key)
    .bind(except)
    .fetch_optional(&mut *conn)
    .await?)
}

fn taken(label: &str, holder: Option<(Uuid, String)>) -> anyhow::Error {
    let (id, pref) = holder.unzip();
    let msg = match &pref {
        Some(p) => format!("〈{label}〉已經是〈{p}〉的標籤"),
        None => format!("〈{label}〉已經是另一個概念的標籤"),
    };
    ApiError::new(ErrorCode::ConceptLabelTaken, msg)
        .with_details(json!({"label": label, "concept_id": id, "pref_label": pref}))
        .into()
}

/// Fails with `concept_label_taken` when another concept holds `n` actively.
async fn ensure_free(conn: &mut PgConnection, n: &Norm, except: Option<Uuid>) -> Result<()> {
    match holder(conn, &n.key, except).await? {
        None => Ok(()),
        Some(h) => Err(taken(&n.label, Some(h))),
    }
}

/// A lost race on the unique index (the pre-check passed in a concurrent
/// transaction): `concept_label_taken` without naming the holder.
fn map_unique(e: sqlx::Error, label: &str) -> anyhow::Error {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => taken(label, None),
        _ => e.into(),
    }
}

#[derive(sqlx::FromRow)]
struct ConceptRow {
    id: Uuid,
    pref_label: String,
    status: String,
    lock_version: i32,
    created_at: DateTime<Utc>,
}

async fn labels_of(
    conn: &mut PgConnection,
    ids: &[Uuid],
    all: bool,
) -> Result<HashMap<Uuid, Vec<LabelView>>> {
    // People who are not administrators see active labels that are not
    // hidden (a hidden label is a misspelling or an internal term). The
    // preferred label is the concept's own `pref_label`, always first.
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT entity_id, label, label_kind, status FROM (
             SELECT a.entity_id, a.label, a.label_kind, a.status FROM aliases a
             WHERE a.entity_kind = 'concept' AND a.entity_id = ANY($1)
               AND ($2 OR (a.status = 'active' AND a.label_kind <> 'hidden'))
               AND a.label_kind <> 'pref'
             UNION ALL
             SELECT c.id, c.pref_label, 'pref', 'active' FROM concepts c WHERE c.id = ANY($1)) l
         ORDER BY entity_id, (label_kind = 'pref') DESC, label",
    )
    .bind(ids)
    .bind(all)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<Uuid, Vec<LabelView>> = HashMap::new();
    for (id, label, kind, status) in rows {
        out.entry(id).or_default().push(LabelView {
            label,
            kind,
            status,
        });
    }
    Ok(out)
}

async fn view_of(conn: &mut PgConnection, id: Uuid, all: bool) -> Result<ConceptView> {
    let r: Option<ConceptRow> = sqlx::query_as(
        "SELECT id, pref_label, status, lock_version, created_at FROM concepts
         WHERE id = $1 AND ($2 OR status = 'active')",
    )
    .bind(id)
    .bind(all)
    .fetch_optional(&mut *conn)
    .await?;
    let r = r.ok_or_else(not_found)?;
    let mut labels = labels_of(conn, &[id], all).await?;
    Ok(ConceptView {
        id: r.id,
        pref_label: r.pref_label,
        status: r.status,
        lock_version: r.lock_version,
        labels: labels.remove(&id).unwrap_or_default(),
        created_at: r.created_at,
    })
}

fn concept_id(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw.trim()).map_err(|_| not_found())
}

/// Locks the concept row for a change; `not_found` when missing.
async fn lock_concept(conn: &mut PgConnection, id: Uuid) -> Result<ConceptRow> {
    let r: Option<ConceptRow> = sqlx::query_as(
        "SELECT id, pref_label, status, lock_version, created_at FROM concepts
         WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    r.ok_or_else(not_found)
}

async fn bump(conn: &mut PgConnection, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE concepts SET lock_version = lock_version + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- reads

/// `GET /admin/concepts?q=&limit=&cursor=`: every signed-in person;
/// administrators also see retired concepts and staging labels.
pub async fn list(app: &App, caller: AuthContext, q: &ConceptQuery) -> Result<ConceptList> {
    // Clamped like the other list endpoints' `limit` (not an error).
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let text =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
    let mut tx = read_tx(app).await?;
    let all = db::is_admin(&mut tx, caller).await?;
    let hash = cursor::query_hash(&json!({"concepts": text, "all": all}));
    let after: Option<(String, Uuid)> = match q.cursor.as_deref() {
        None => None,
        Some(c) => match cursor::decode(&app.pool, c, caller.principal_id(), &hash).await? {
            Position::Sort { k, i } if k.len() == 1 => Some((k[0].clone().unwrap_or_default(), i)),
            _ => return Err(cursor::mismatch()),
        },
    };
    let rows: Vec<(Uuid, String, String, i32, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT c.id, c.pref_label, c.status, c.lock_version, c.created_at,
                casefold(normalize(c.pref_label, NFC)) AS sk
         FROM concepts c
         WHERE ($1 OR c.status = 'active')
           AND ($2::text IS NULL
                OR strpos(casefold(normalize(c.pref_label, NFC)), casefold(normalize($2, NFC))) > 0
                OR EXISTS (SELECT 1 FROM aliases a
                           WHERE a.entity_kind = 'concept' AND a.entity_id = c.id
                             AND ($1 OR (a.status = 'active' AND a.label_kind <> 'hidden'))
                             AND strpos(casefold(normalize(a.label, NFC)),
                                        casefold(normalize($2, NFC))) > 0))
           AND ($3::text IS NULL OR (casefold(normalize(c.pref_label, NFC)), c.id) > ($3, $4))
         ORDER BY sk, c.id
         LIMIT $5",
    )
    .bind(all)
    .bind(&text)
    .bind(after.as_ref().map(|a| a.0.clone()))
    .bind(after.as_ref().map(|a| a.1).unwrap_or_default())
    .bind(limit as i64 + 1)
    .fetch_all(&mut *tx)
    .await?;
    let more = rows.len() > limit as usize;
    let rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let mut labels = labels_of(&mut tx, &ids, all).await?;
    tx.commit().await?;
    let next_cursor = match (more, rows.last()) {
        (true, Some(last)) => Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &hash,
                Position::Sort {
                    k: vec![Some(last.5.clone())],
                    i: last.0,
                },
            )
            .await?,
        ),
        _ => None,
    };
    Ok(ConceptList {
        items: rows
            .into_iter()
            .map(
                |(id, pref_label, status, lock_version, created_at, _)| ConceptView {
                    id,
                    pref_label,
                    status,
                    lock_version,
                    labels: labels.remove(&id).unwrap_or_default(),
                    created_at,
                },
            )
            .collect(),
        next_cursor,
    })
}

/// `GET /admin/concepts/{id}`.
pub async fn get(app: &App, caller: AuthContext, id: &str) -> Result<ConceptView> {
    let id = concept_id(id)?;
    let mut tx = read_tx(app).await?;
    let all = db::is_admin(&mut tx, caller).await?;
    let v = view_of(&mut tx, id, all).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- writes

/// `POST /admin/concepts {pref_label, labels:[{label, kind?, status?}]}` (201).
pub async fn create(app: &App, caller: AuthContext, input: &NewConcept) -> Result<ConceptView> {
    let mut tx = app.pool.begin().await?;
    let who = admin(&mut tx, caller).await?;
    if input.labels.len() + 1 > MAX_LABELS {
        return Err(invalid(
            "labels",
            "too_many_labels",
            "a concept has at most 50 labels",
        ));
    }
    let pref = norm(&mut tx, "pref_label", &input.pref_label).await?;
    let mut rows: Vec<(Norm, &'static str, &'static str)> = vec![(pref, "pref", "active")];
    for (i, l) in input.labels.iter().enumerate() {
        let p = format!("labels[{i}]");
        let n = norm(&mut tx, &format!("{p}.label"), &l.label).await?;
        let kind = label_kind(&format!("{p}.kind"), l.kind.as_deref())?;
        let status = label_status(&format!("{p}.status"), l.status.as_deref())?;
        if rows.iter().any(|(m, _, _)| m.key == n.key) {
            return Err(invalid(
                &format!("{p}.label"),
                "duplicate_label",
                "the same label twice (after NFC and case folding)",
            ));
        }
        rows.push((n, kind, status));
    }
    for (n, _, status) in &rows {
        if *status == "active" {
            ensure_free(&mut tx, n, None).await?;
        }
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO concepts (pref_label, created_by) VALUES ($1, $2) RETURNING id",
    )
    .bind(&rows[0].0.label)
    .bind(who)
    .fetch_one(&mut *tx)
    .await?;
    for (n, kind, status) in &rows {
        sqlx::query(
            "INSERT INTO aliases (entity_kind, entity_id, label, label_kind, status, created_by)
             VALUES ('concept', $1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(&n.label)
        .bind(kind)
        .bind(status)
        .bind(who)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_unique(e, &n.label))?;
    }
    let v = view_of(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(v)
}

/// `POST /admin/concepts/{id}/labels {label, kind?, status?}` (201).
pub async fn add_label(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &NewLabel,
) -> Result<ConceptView> {
    let id = concept_id(id)?;
    let mut tx = app.pool.begin().await?;
    let who = admin(&mut tx, caller).await?;
    lock_concept(&mut tx, id).await?;
    let n = norm(&mut tx, "label", &input.label).await?;
    let kind = label_kind("kind", input.kind.as_deref())?;
    let status = label_status("status", input.status.as_deref())?;
    let (count, same): (i64, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(bool_or(casefold(normalize(label, NFC)) = $2), false)
         FROM aliases WHERE entity_kind = 'concept' AND entity_id = $1",
    )
    .bind(id)
    .bind(&n.key)
    .fetch_one(&mut *tx)
    .await?;
    if same {
        return Err(invalid(
            "label",
            "label_exists",
            "this concept already has the label (after NFC and case folding)",
        ));
    }
    if count as usize >= MAX_LABELS {
        return Err(invalid(
            "label",
            "too_many_labels",
            "a concept has at most 50 labels",
        ));
    }
    if status == "active" {
        ensure_free(&mut tx, &n, Some(id)).await?;
    }
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, label_kind, status, created_by)
         VALUES ('concept', $1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(&n.label)
    .bind(kind)
    .bind(status)
    .bind(who)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_unique(e, &n.label))?;
    bump(&mut tx, id).await?;
    let v = view_of(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(v)
}

/// `PATCH /admin/concepts/{id}/labels/{label} {status?, kind?}`; the label
/// in the path is matched after NFC + casefold.
pub async fn patch_label(
    app: &App,
    caller: AuthContext,
    id: &str,
    label: &str,
    input: &LabelPatch,
) -> Result<ConceptView> {
    let id = concept_id(id)?;
    let mut tx = app.pool.begin().await?;
    admin(&mut tx, caller).await?;
    lock_concept(&mut tx, id).await?;
    let key: String = sqlx::query_scalar("SELECT casefold(normalize($1, NFC))")
        .bind(label.trim())
        .fetch_one(&mut *tx)
        .await?;
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT label, label_kind, status FROM aliases
         WHERE entity_kind = 'concept' AND entity_id = $1 AND casefold(normalize(label, NFC)) = $2",
    )
    .bind(id)
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((stored, old_kind, old_status)) = row else {
        return Err(api(ErrorCode::NotFound, "the concept has no such label"));
    };
    let kind = match input.kind.as_deref() {
        None => old_kind.clone(),
        Some(k) => label_kind("kind", Some(k))?.to_string(),
    };
    let status = match input.status.as_deref() {
        None => old_status.clone(),
        Some(s) => label_status("status", Some(s))?.to_string(),
    };
    if old_kind == "pref" && (kind != "pref" || status != "active") {
        return Err(invalid(
            "label",
            "pref_label_fixed",
            "the preferred label stays active; rename the concept instead",
        ));
    }
    if status == "active" && old_status != "active" {
        let n = Norm {
            label: stored.clone(),
            key: key.clone(),
        };
        ensure_free(&mut tx, &n, Some(id)).await?;
    }
    if kind != old_kind || status != old_status {
        sqlx::query(
            "UPDATE aliases SET label_kind = $3, status = $4
             WHERE entity_kind = 'concept' AND entity_id = $1 AND label = $2",
        )
        .bind(id)
        .bind(&stored)
        .bind(&kind)
        .bind(&status)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_unique(e, &stored))?;
        bump(&mut tx, id).await?;
    }
    let v = view_of(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(v)
}

/// `PATCH /admin/concepts/{id} {lock_version, pref_label?, status?}`.
pub async fn patch(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &ConceptPatch,
) -> Result<ConceptView> {
    let id = concept_id(id)?;
    let mut tx = app.pool.begin().await?;
    let who = admin(&mut tx, caller).await?;
    let before = lock_concept(&mut tx, id).await?;
    if before.lock_version != input.lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the concept changed since you loaded it; reload it",
        )
        .with_details(json!({"current_lock_version": before.lock_version}))
        .into());
    }
    let mut changed = false;
    if let Some(s) = input.status.as_deref() {
        if !matches!(s, "active" | "retired") {
            return Err(invalid(
                "status",
                "concept_status_invalid",
                "status is active or retired",
            ));
        }
        if s != before.status {
            sqlx::query("UPDATE concepts SET status = $2 WHERE id = $1")
                .bind(id)
                .bind(s)
                .execute(&mut *tx)
                .await?;
            changed = true;
        }
    }
    if let Some(p) = input.pref_label.as_deref() {
        let n = norm(&mut tx, "pref_label", p).await?;
        if n.label != before.pref_label {
            let other: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM aliases
                                WHERE entity_kind = 'concept' AND entity_id = $1 AND label_kind <> 'pref'
                                  AND casefold(normalize(label, NFC)) = $2)",
            )
            .bind(id)
            .bind(&n.key)
            .fetch_one(&mut *tx)
            .await?;
            if other {
                return Err(invalid(
                    "pref_label",
                    "label_exists",
                    "this concept already has the label (after NFC and case folding)",
                ));
            }
            ensure_free(&mut tx, &n, Some(id)).await?;
            let renamed = sqlx::query(
                "UPDATE aliases SET label = $2, status = 'active'
                 WHERE entity_kind = 'concept' AND entity_id = $1 AND label_kind = 'pref'",
            )
            .bind(id)
            .bind(&n.label)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_unique(e, &n.label))?
            .rows_affected();
            // A concept without its pref row (data from before 0027 fixed
            // it): the new name gets one, so it expands and is unique.
            if renamed == 0 {
                sqlx::query(
                    "INSERT INTO aliases (entity_kind, entity_id, label, label_kind, status, created_by)
                     VALUES ('concept', $1, $2, 'pref', 'active', $3)",
                )
                .bind(id)
                .bind(&n.label)
                .bind(who)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_unique(e, &n.label))?;
            }
            sqlx::query("UPDATE concepts SET pref_label = $2 WHERE id = $1")
                .bind(id)
                .bind(&n.label)
                .execute(&mut *tx)
                .await?;
            changed = true;
        }
    }
    if changed {
        bump(&mut tx, id).await?;
    }
    let v = view_of(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- expansion

/// One expanded label (`match.expanded`, §3.9): the query piece that
/// equals a label, the concept (its preferred label) and the other label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Expansion {
    /// The query piece as typed.
    pub from: String,
    /// The concept's preferred label.
    pub via: String,
    /// The label added to the query; for a hidden label (a misspelling
    /// or internal term) the concept's preferred label instead.
    pub label: String,
    /// The added label is hidden (`label` then shows `via`).
    #[serde(skip)]
    #[schemars(skip)]
    pub hidden: bool,
    /// The label actually added to the query (matching only).
    #[serde(skip)]
    #[schemars(skip)]
    pub added: String,
}

/// The pieces of `q` that may equal a label: the whole query, and each
/// whitespace-separated word when there are several, trimmed and
/// de-duplicated, in that order.
pub fn pieces(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let whole = q.trim();
    if whole.is_empty() {
        return out;
    }
    out.push(whole.to_string());
    for w in whole.split_whitespace() {
        if out.len() >= MAX_PIECES {
            break;
        }
        if !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// The labels to add to `q` (design 05 §3.9): a piece of `q` (after NFC +
/// casefold) equal to an active label of an active concept adds that
/// concept's other active labels. Staging labels and retired concepts
/// never take part. Labels that equal a piece of `q` are not added.
pub async fn expansions(conn: &mut PgConnection, q: &str) -> Result<Vec<Expansion>> {
    let p = pieces(q);
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "WITH cand AS (
             SELECT DISTINCT ON (k) raw, k, n FROM (
                 SELECT x AS raw, casefold(normalize(x, NFC)) AS k, n
                 FROM unnest($1::text[]) WITH ORDINALITY AS u(x, n)) s
             ORDER BY k, n),
         hit AS (
             SELECT DISTINCT ON (a.entity_id) c.raw, a.entity_id
             FROM cand c
             JOIN aliases a ON a.entity_kind = 'concept' AND a.status = 'active'
                           AND casefold(normalize(a.label, NFC)) = c.k
             JOIN concepts k ON k.id = a.entity_id AND k.status = 'active'
             ORDER BY a.entity_id, c.n)
         SELECT h.raw, k.pref_label, o.label, o.label_kind = 'hidden'
         FROM hit h
         JOIN concepts k ON k.id = h.entity_id
         JOIN aliases o ON o.entity_kind = 'concept' AND o.entity_id = h.entity_id
                       AND o.status = 'active'
         WHERE casefold(normalize(o.label, NFC)) NOT IN (SELECT k FROM cand)
         ORDER BY h.raw, k.pref_label, o.label
         LIMIT $2",
    )
    .bind(&p)
    .bind(MAX_EXPANSIONS)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(from, via, added, hidden)| Expansion {
            label: if hidden { via.clone() } else { added.clone() },
            from,
            via,
            hidden,
            added,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::pieces;

    #[test]
    fn pieces_are_the_query_and_its_words() {
        assert_eq!(pieces("  登入  "), vec!["登入"]);
        assert_eq!(pieces("登入 失敗"), vec!["登入 失敗", "登入", "失敗"]);
        assert_eq!(pieces("a a b"), vec!["a a b", "a", "b"]);
        assert!(pieces("   ").is_empty());
        let many: String = (0..40).map(|i| format!("w{i} ")).collect();
        assert_eq!(pieces(&many).len(), 16);
    }
}
