// SPDX-License-Identifier: GPL-2.0-or-later
//! The review queue (contract 02 §15.3, §6.7 step 13a): which proposals may
//! be approved from the queue with a tick (`review_mode: "queue"`), the
//! field diff shown on each row, and the `evidence_changed` warning.
//!
//! One function decides eligibility for the list, for transaction A and for
//! step 13a in transaction B, so the three never disagree.

use crate::auth::AuthContext;
use crate::citations::{CitationStatus, Reader};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// Fields too long to judge from a one-line diff (§15.3: "短欄位").
const LONG_FIELDS: &[&str] = &["subject", "description", "is_private"];

/// One changed field as the queue shows it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffRow {
    pub target: DiffTarget,
    /// `assigned_to_id`, `status`, `cf:<id>`, …
    pub field: String,
    /// Display name.
    pub label: String,
    /// Current value, rendered (null when unset).
    pub before: Json,
    /// Proposed value, rendered.
    pub after: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffTarget {
    pub id: Uuid,
    pub key: String,
    pub subject: String,
}

/// Why a proposal cannot be approved from the queue (empty: it can).
pub fn shape_reasons(body: &Json) -> Vec<String> {
    let mut reasons = Vec::new();
    let ops = body["ops"].as_array().cloned().unwrap_or_default();
    let mut transitions = 0;
    if ops.is_empty() {
        reasons.push("no_ops".to_string());
    }
    for op in &ops {
        match op["kind"].as_str().unwrap_or_default() {
            "update_fields" => {
                if let Some(set) = op["set"].as_object() {
                    for k in set.keys() {
                        if LONG_FIELDS.contains(&k.as_str()) {
                            reasons.push(format!("long_field:{k}"));
                        }
                    }
                }
            }
            "transition" => transitions += 1,
            k => reasons.push(format!("op:{k}")),
        }
    }
    if transitions > 1 {
        reasons.push("several_transitions".to_string());
    }
    reasons
}

/// Long custom fields (`text`) are not short either.
async fn long_custom_fields(conn: &mut PgConnection, body: &Json) -> Result<Vec<String>> {
    let ids: Vec<i32> = cf_ids(body);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let long: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM field_defs WHERE id = ANY($1) AND ftype = 'text'")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    Ok(long
        .into_iter()
        .map(|i| format!("long_field:cf:{i}"))
        .collect())
}

fn cf_ids(body: &Json) -> Vec<i32> {
    let mut ids = Vec::new();
    for op in body["ops"].as_array().into_iter().flatten() {
        for k in op["set"].as_object().into_iter().flat_map(|m| m.keys()) {
            if let Some(n) = k.strip_prefix("cf:").and_then(|n| n.parse().ok()) {
                ids.push(n);
            }
        }
    }
    ids
}

/// `evidence_changed` when a citation no longer verifies for `viewer` (the
/// owner on the list and in transaction A, the executor in step 13a).
pub async fn evidence_warnings(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
) -> Result<Vec<String>> {
    let Some(items) = body["evidence"].as_array().filter(|a| !a.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut reader = Reader::new(&mut *conn, viewer).await?;
    for ev in items {
        let st = reader
            .verify_exact(
                &mut *conn,
                ev["citation_id"].as_str().unwrap_or_default(),
                ev["quote"].as_str().unwrap_or_default(),
            )
            .await?;
        if st != CitationStatus::Valid {
            return Ok(vec!["evidence_changed".to_string()]);
        }
    }
    Ok(Vec::new())
}

/// Eligibility: the shape, the stored validation, and no warning.
pub struct Eligibility {
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
}

impl Eligibility {
    pub fn queue_ok(&self) -> bool {
        self.reasons.is_empty() && self.warnings.is_empty()
    }
    pub fn review_modes(&self) -> Vec<String> {
        if self.queue_ok() {
            vec!["detail".into(), "queue".into()]
        } else {
            vec!["detail".into()]
        }
    }
}

/// `validation_status`: the stored (list, transaction A) or just simulated
/// (step 13a) status.
pub async fn eligibility(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
    validation_status: &str,
) -> Result<Eligibility> {
    let mut reasons = shape_reasons(body);
    reasons.extend(long_custom_fields(&mut *conn, body).await?);
    if validation_status != "passed" {
        reasons.push(format!("validation:{validation_status}"));
    }
    let warnings = evidence_warnings(&mut *conn, viewer, body).await?;
    Ok(Eligibility { reasons, warnings })
}

/// The refusal for a proposal that no longer qualifies (§4.4: 409).
pub fn outdated(e: &Eligibility) -> anyhow::Error {
    let mut all = e.reasons.clone();
    all.extend(e.warnings.iter().cloned());
    crate::error::ApiError::new(
        crate::error::ErrorCode::ReviewOutdated,
        "this proposal no longer qualifies for queue approval; open it and review it in full",
    )
    .with_details(json!({"reasons": all}))
    .into()
}

fn label(field: &str) -> &'static str {
    match field {
        "subject" => "Subject",
        "description" => "Description",
        "assigned_to_id" => "Assignee",
        "priority_id" => "Priority",
        "fixed_version_id" => "Target version",
        "start_date" => "Start date",
        "due_date" => "Due date",
        "estimated_hours" => "Estimated time",
        "is_private" => "Private",
        "status" => "Status",
        _ => "",
    }
}

async fn principal(conn: &mut PgConnection, id: &Json) -> Result<Json> {
    let Some(id) = id.as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
        return Ok(Json::Null);
    };
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as("SELECT name, login, kind FROM principals WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.map_or(
        Json::Null,
        |(name, login, kind)| json!({"id": id, "name": name, "login": login, "kind": kind}),
    ))
}

async fn named(conn: &mut PgConnection, table: &str, id: &Json) -> Result<Json> {
    let Some(id) = id.as_str().and_then(|s| s.parse::<Uuid>().ok()) else {
        return Ok(Json::Null);
    };
    let sql = match table {
        "priority" => "SELECT name FROM issue_priorities WHERE id = $1",
        "version" => "SELECT name FROM versions WHERE id = $1",
        _ => "SELECT name FROM issue_statuses WHERE id = $1",
    };
    let name: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(name.map_or(Json::Null, |n| json!({"id": id, "name": n})))
}

async fn render(conn: &mut PgConnection, field: &str, v: &Json) -> Result<Json> {
    Ok(match field {
        "assigned_to_id" => principal(conn, v).await?,
        "priority_id" => named(conn, "priority", v).await?,
        "fixed_version_id" => named(conn, "version", v).await?,
        "status" => named(conn, "status", v).await?,
        f if f.starts_with("cf:") => {
            let id: i32 = f[3..].parse().unwrap_or_default();
            let ftype: Option<String> =
                sqlx::query_scalar("SELECT ftype FROM field_defs WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *conn)
                    .await?;
            match ftype.as_deref() {
                Some("user") => principal(conn, v).await?,
                Some("list") => match v.as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                    Some(o) => {
                        let l: Option<String> =
                            sqlx::query_scalar("SELECT label FROM field_options WHERE id = $1")
                                .bind(o)
                                .fetch_optional(&mut *conn)
                                .await?;
                        l.map_or(Json::Null, |l| json!({"id": o, "label": l}))
                    }
                    None => v.clone(),
                },
                _ => v.clone(),
            }
        }
        _ => v.clone(),
    })
}

/// The diff of a proposal whose targets `viewer` can all read; `None`
/// otherwise (the row then shows no content).
pub async fn diff(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
    keys: &std::collections::BTreeMap<Uuid, String>,
) -> Result<Vec<DiffRow>> {
    let mut rows = Vec::new();
    for op in body["ops"].as_array().into_iter().flatten() {
        let Some(id) = op["target"]["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
        else {
            continue;
        };
        let Some(key) = keys.get(&id) else { continue };
        type Cur = (
            String,
            String,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
            Option<chrono::NaiveDate>,
            Option<chrono::NaiveDate>,
            bool,
            Uuid,
            Uuid,
            Uuid,
            Uuid,
            Option<String>,
        );
        let cur: Option<Cur> = sqlx::query_as(
            "SELECT subject, description, assigned_to_id, priority_id, fixed_version_id,
                    start_date, due_date, is_private, status_id, tracker_id, project_id, id,
                    trim_scale(estimated_hours)::text
             FROM issues WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(c) = cur else { continue };
        let target = DiffTarget {
            id,
            key: key.clone(),
            subject: c.0.clone(),
        };
        let mut push = |field: String, label: String, before: Json, after: Json| {
            rows.push(DiffRow {
                target: target.clone(),
                field,
                label,
                before,
                after,
            });
        };
        match op["kind"].as_str().unwrap_or_default() {
            "update_fields" => {
                for (k, v) in op["set"].as_object().into_iter().flatten() {
                    let before_raw = match k.as_str() {
                        "subject" => json!(c.0),
                        "description" => json!(c.1),
                        "assigned_to_id" => json!(c.2),
                        "priority_id" => json!(c.3),
                        "fixed_version_id" => json!(c.4),
                        "start_date" => json!(c.5),
                        "due_date" => json!(c.6),
                        "is_private" => json!(c.7),
                        "estimated_hours" => json!(c.12),
                        _ => Json::Null,
                    };
                    let (lbl, before) = if let Some(n) = k.strip_prefix("cf:") {
                        let name: Option<String> =
                            sqlx::query_scalar("SELECT name FROM field_defs WHERE id = $1")
                                .bind(n.parse::<i32>().unwrap_or_default())
                                .fetch_optional(&mut *conn)
                                .await?;
                        let name = name.unwrap_or_else(|| k.clone());
                        let roles = crate::db::role_ids_for(&mut *conn, viewer, c.10).await?;
                        let vals = crate::service::custom_field_values(
                            &mut *conn,
                            viewer,
                            &roles,
                            crate::service::CfTarget {
                                issue_id: id,
                                tracker_id: c.9,
                                project_id: c.10,
                            },
                            Some(std::slice::from_ref(&name)),
                        )
                        .await?;
                        let before = vals
                            .into_iter()
                            .next()
                            .and_then(|v| v.value)
                            .unwrap_or(Json::Null);
                        (name, before)
                    } else {
                        (
                            label(k).to_string(),
                            render(&mut *conn, k, &before_raw).await?,
                        )
                    };
                    let after = render(&mut *conn, k, v).await?;
                    push(k.clone(), lbl, before, after);
                }
            }
            "transition" => {
                let before = render(&mut *conn, "status", &json!(c.8)).await?;
                let after = render(&mut *conn, "status", &op["to_status_id"]).await?;
                push("status".into(), label("status").into(), before, after);
            }
            "add_note" => push(
                "note".into(),
                "Note".into(),
                Json::Null,
                json!({"is_private": op["is_private"]}),
            ),
            _ => {}
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(ops: Json) -> Json {
        json!({"ops": ops})
    }

    #[test]
    fn shape() {
        let t = json!({"type": "issue", "id": Uuid::nil()});
        let ok = doc(json!([
            {"kind": "update_fields", "target": t, "set": {"assigned_to_id": null, "priority_id": null}},
            {"kind": "transition", "target": t, "to_status_id": Uuid::nil()}
        ]));
        assert!(shape_reasons(&ok).is_empty());
        for (bad, why) in [
            (
                json!([{"kind": "add_note", "target": t, "body": "x"}]),
                "op:add_note",
            ),
            (
                json!([{"kind": "update_fields", "target": t, "set": {"description": "x"}}]),
                "long_field:description",
            ),
            (
                json!([{"kind": "update_fields", "target": t, "set": {"subject": "x"}}]),
                "long_field:subject",
            ),
            (
                json!([{"kind": "update_fields", "target": t, "set": {"is_private": true}}]),
                "long_field:is_private",
            ),
            (json!([{"kind": "create_issue"}]), "op:create_issue"),
            (json!([{"kind": "duplicate_of"}]), "op:duplicate_of"),
            (json!([{"kind": "create_version"}]), "op:create_version"),
            (
                json!([{"kind": "transition"}, {"kind": "transition"}]),
                "several_transitions",
            ),
        ] {
            assert_eq!(
                shape_reasons(&doc(bad.clone())),
                vec![why.to_string()],
                "{bad}"
            );
        }
    }
}
