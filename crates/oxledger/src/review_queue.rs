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
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Fields too long to judge from a one-line diff (§15.3: "短欄位").
const LONG_FIELDS: &[&str] = &["subject", "description", "is_private"];
/// Contract 05 §4.3: a `log_time` may be ticked when its comments are at
/// most this many characters (or absent).
pub const TIME_COMMENTS_TICKABLE: usize = 80;

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
    /// G13: the value when the agent proposed this version, rendered like
    /// `before` (null when unset). Meaningful only with `has_original`.
    pub original: Json,
    /// False when no value was recorded for this field: an owner's
    /// revision, a proposal stored before 0031, redacted content, a field
    /// the proposing agent could not read then or you cannot read now, or a
    /// row that is not a field (note, triage, time).
    pub has_original: bool,
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
            // Contract 05 §4.3: accepting (without a note) and snoozing
            // may be ticked; declining, duplicates and unsnooze may not.
            "triage_decide" => match op["decision"].as_str().unwrap_or_default() {
                "accept" | "snooze" => {}
                d => reasons.push(format!("triage:{d}")),
            },
            // Contract 05 §4.3: short or no comments; update_time_entry
            // is opened one by one.
            "log_time" => {
                let n = op["fields"]["comments"]
                    .as_str()
                    .map_or(0, |c| c.chars().count());
                if n > TIME_COMMENTS_TICKABLE {
                    reasons.push("long_field:comments".to_string());
                }
            }
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
    stored_rules_version: Option<&str>,
) -> Result<Eligibility> {
    eligibility_for(
        conn,
        viewer,
        body,
        validation_status,
        stored_rules_version,
        None,
    )
    .await
}

/// [`eligibility`] of a stored ChangeSet: one the agent chain limit sent
/// for review (`chain_review`) never qualifies (design 05 §3.12.4).
pub async fn eligibility_for(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
    validation_status: &str,
    stored_rules_version: Option<&str>,
    chain_review: Option<&str>,
) -> Result<Eligibility> {
    let mut reasons = shape_reasons(body);
    // Design 05 §3.12.4: sent by the agent chain limit → read it in full.
    // The reason carries no counts (appendix D V21).
    if let Some(c) = chain_review {
        reasons.push(format!("agent_chain:{c}"));
    }
    reasons.extend(long_custom_fields(&mut *conn, body).await?);
    if validation_status != "passed" {
        reasons.push(format!("validation:{validation_status}"));
    }
    // Contract 05 §3.6.3 (appendix C R14): the form rules changed since the
    // version was stored.
    if crate::forms::rules_changed(&mut *conn, body, stored_rules_version).await? {
        reasons.push("rules_changed".into());
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
        "activity" => "SELECT name FROM time_entry_activities WHERE id = $1",
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
            // A stored proposal carries custom values typed, `{"<type>": v}`
            // (commands.rs `parse_cf`); show them like the current value.
            let typed = v
                .as_object()
                .filter(|m| m.len() == 1)
                .and_then(|m| m.values().next());
            let v = typed.unwrap_or(v);
            match ftype.as_deref() {
                Some("int") => v
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .map_or_else(|| v.clone(), |n| json!(n)),
                Some("float") => v
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map_or_else(|| v.clone(), |n| json!(n)),
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

/// An existing issue's built-in fields, as the diff and the baseline read
/// them.
#[derive(sqlx::FromRow)]
struct Cur {
    subject: String,
    description: String,
    assigned_to_id: Option<Uuid>,
    priority_id: Option<Uuid>,
    fixed_version_id: Option<Uuid>,
    start_date: Option<chrono::NaiveDate>,
    due_date: Option<chrono::NaiveDate>,
    is_private: bool,
    status_id: Uuid,
    tracker_id: Uuid,
    project_id: Uuid,
    author_id: Uuid,
    estimated_hours: Option<String>,
    lock_version: i32,
}

async fn current(conn: &mut PgConnection, id: Uuid) -> Result<Option<Cur>> {
    Ok(sqlx::query_as(
        "SELECT subject, description, assigned_to_id, priority_id, fixed_version_id,
                start_date, due_date, is_private, status_id, tracker_id, project_id, author_id,
                trim_scale(estimated_hours)::text AS estimated_hours, lock_version
         FROM issues WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

impl Cur {
    /// A built-in field's stored value (`status` = `status_id`); null for
    /// anything else.
    fn raw(&self, field: &str) -> Json {
        match field {
            "subject" => json!(self.subject),
            "description" => json!(self.description),
            "assigned_to_id" => json!(self.assigned_to_id),
            "priority_id" => json!(self.priority_id),
            "fixed_version_id" => json!(self.fixed_version_id),
            "start_date" => json!(self.start_date),
            "due_date" => json!(self.due_date),
            "is_private" => json!(self.is_private),
            "estimated_hours" => json!(self.estimated_hours),
            "status" => json!(self.status_id),
            _ => Json::Null,
        }
    }

    fn cf_target(&self, issue_id: Uuid) -> crate::service::CfTarget {
        crate::service::CfTarget {
            issue_id,
            tracker_id: self.tracker_id,
            project_id: self.project_id,
        }
    }
}

/// The custom field `cf:<n>`'s name.
async fn cf_name(conn: &mut PgConnection, key: &str) -> Result<Option<String>> {
    let Some(n) = key.strip_prefix("cf:").and_then(|n| n.parse::<i32>().ok()) else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT name FROM field_defs WHERE id = $1")
            .bind(n)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// The fields a document changes on each existing issue: the `set` keys of
/// `update_fields` and `status` for a `transition`.
fn changed_fields(body: &Json) -> BTreeMap<Uuid, BTreeSet<String>> {
    let mut out: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for op in body["ops"].as_array().into_iter().flatten() {
        let Some(id) = op["target"]["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
        else {
            continue;
        };
        match op["kind"].as_str().unwrap_or_default() {
            "update_fields" => out.entry(id).or_default().extend(
                op["set"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.clone()),
            ),
            "transition" => {
                out.entry(id).or_default().insert("status".into());
            }
            _ => {}
        }
    }
    out
}

/// G13: the values the fields a proposal changes hold right now on each
/// existing target, as `reader` (the proposing agent) can read them:
/// `{"<issue id>": {"lock_version": n, "fields": {"<field>": value}}}`.
/// Built-in fields hold their stored value (ids, dates, `status` = the
/// status id), custom fields the value `get` shows. A target the reader
/// cannot see, and a custom field it cannot read (agents: `visible` only),
/// are left out (contract 02 §3.5: not readable = not there).
pub async fn baseline(conn: &mut PgConnection, reader: AuthContext, body: &Json) -> Result<Json> {
    let want = changed_fields(body);
    let mut out = serde_json::Map::new();
    if want.is_empty() {
        return Ok(Json::Object(out));
    }
    let scope = crate::db::load_scope(&mut *conn, reader).await?;
    for (id, fields) in want {
        let Some(c) = current(&mut *conn, id).await? else {
            continue;
        };
        if !scope.can_see(&ox_core::IssueFacts {
            project_id: c.project_id,
            tracker_id: c.tracker_id,
            is_private: c.is_private,
            author_id: c.author_id,
            assigned_to_id: c.assigned_to_id,
        }) {
            continue;
        }
        let mut values = serde_json::Map::new();
        let mut cf_keys: Vec<(String, String)> = Vec::new();
        for f in fields {
            if f.starts_with("cf:") {
                if let Some(name) = cf_name(&mut *conn, &f).await? {
                    cf_keys.push((f, name));
                }
            } else if label(&f).is_empty() {
                continue;
            } else {
                values.insert(f.clone(), c.raw(&f));
            }
        }
        if !cf_keys.is_empty() {
            let names: Vec<String> = cf_keys.iter().map(|(_, n)| n.clone()).collect();
            let roles = crate::db::role_ids_for(&mut *conn, reader, c.project_id).await?;
            let vals = crate::service::custom_field_values(
                &mut *conn,
                reader,
                &roles,
                c.cf_target(id),
                Some(&names),
            )
            .await?;
            for v in vals {
                if let Some((key, _)) = cf_keys.iter().find(|(_, n)| *n == v.name) {
                    values.insert(key.clone(), v.value.unwrap_or(Json::Null));
                }
            }
        }
        out.insert(
            id.to_string(),
            json!({"lock_version": c.lock_version, "fields": values}),
        );
    }
    Ok(Json::Object(out))
}

/// The diff of a proposal whose targets `viewer` can all read; `None`
/// otherwise (the row then shows no content).
pub async fn diff(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
    keys: &BTreeMap<Uuid, String>,
) -> Result<Vec<DiffRow>> {
    diff_with_baseline(conn, viewer, body, keys, None).await
}

/// [`diff`] with the version's [`baseline`] as each row's `original`
/// (the queue list and `GET /changesets/{id}` both use this, G12/G13).
/// A recorded custom field value shows only while `viewer` can read that
/// field on the issue now.
pub async fn diff_with_baseline(
    conn: &mut PgConnection,
    viewer: AuthContext,
    body: &Json,
    keys: &BTreeMap<Uuid, String>,
    baseline: Option<&Json>,
) -> Result<Vec<DiffRow>> {
    let mut rows = Vec::new();
    for op in body["ops"].as_array().into_iter().flatten() {
        if op["kind"] == "log_time" {
            if let Some(r) = time_row(&mut *conn, viewer, op, keys).await? {
                rows.push(r);
            }
            continue;
        }
        let Some(id) = op["target"]["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok())
        else {
            continue;
        };
        let Some(key) = keys.get(&id) else { continue };
        let Some(c) = current(&mut *conn, id).await? else {
            continue;
        };
        let recorded = baseline
            .and_then(|b| b.get(id.to_string()))
            .and_then(|t| t["fields"].as_object());
        let target = DiffTarget {
            id,
            key: key.clone(),
            subject: c.subject.clone(),
        };
        let mut push =
            |field: String, label: String, before: Json, after: Json, orig: Option<Json>| {
                rows.push(DiffRow {
                    target: target.clone(),
                    field,
                    label,
                    before,
                    after,
                    has_original: orig.is_some(),
                    original: orig.unwrap_or(Json::Null),
                });
            };
        match op["kind"].as_str().unwrap_or_default() {
            "update_fields" => {
                for (k, v) in op["set"].as_object().into_iter().flatten() {
                    let was = recorded.and_then(|m| m.get(k));
                    let (lbl, before, original) = if k.starts_with("cf:") {
                        let name = cf_name(&mut *conn, k).await?.unwrap_or_else(|| k.clone());
                        let roles =
                            crate::db::role_ids_for(&mut *conn, viewer, c.project_id).await?;
                        let vals = crate::service::custom_field_values(
                            &mut *conn,
                            viewer,
                            &roles,
                            c.cf_target(id),
                            Some(std::slice::from_ref(&name)),
                        )
                        .await?;
                        // Not readable by the viewer now: no original either.
                        let readable = !vals.is_empty();
                        let before = vals
                            .into_iter()
                            .next()
                            .and_then(|v| v.value)
                            .unwrap_or(Json::Null);
                        (name, before, was.filter(|_| readable).cloned())
                    } else {
                        let original = match was {
                            Some(w) => Some(render(&mut *conn, k, w).await?),
                            None => None,
                        };
                        (
                            label(k).to_string(),
                            render(&mut *conn, k, &c.raw(k)).await?,
                            original,
                        )
                    };
                    let after = render(&mut *conn, k, v).await?;
                    push(k.clone(), lbl, before, after, original);
                }
            }
            "transition" => {
                let before = render(&mut *conn, "status", &c.raw("status")).await?;
                let after = render(&mut *conn, "status", &op["to_status_id"]).await?;
                let original = match recorded.and_then(|m| m.get("status")) {
                    Some(w) => Some(render(&mut *conn, "status", w).await?),
                    None => None,
                };
                push(
                    "status".into(),
                    label("status").into(),
                    before,
                    after,
                    original,
                );
            }
            "add_note" => push(
                "note".into(),
                "Note".into(),
                Json::Null,
                json!({"is_private": op["is_private"]}),
                None,
            ),
            "triage_decide" => {
                let state: Option<String> =
                    sqlx::query_scalar("SELECT state FROM issue_triage WHERE issue_id = $1")
                        .bind(id)
                        .fetch_optional(&mut *conn)
                        .await?;
                let mut after = json!({"decision": op["decision"]});
                if let Some(u) = op.get("snooze_until") {
                    after["snooze_until"] = u.clone();
                }
                if let Some(d) = op["duplicate_of"]["id"].as_str() {
                    after["duplicate_of"] = json!(d);
                }
                push(
                    "triage".into(),
                    "Triage".into(),
                    json!(state.unwrap_or_else(|| "none".into())),
                    after,
                    None,
                );
            }
            _ => {}
        }
    }
    Ok(rows)
}

/// A `log_time` on an issue the viewer can see (contract 05 §6: the queue
/// shows time proposals as rows): hours, date, activity and comments. A
/// `log_time` without an issue has no row (the proposal title says it).
async fn time_row(
    conn: &mut PgConnection,
    viewer: AuthContext,
    op: &Json,
    keys: &std::collections::BTreeMap<Uuid, String>,
) -> Result<Option<DiffRow>> {
    let id = match op["issue"]["id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    {
        Some(id) => Some(id),
        // P3 gap G47: proposals stored before the fix kept log_time's issue
        // as a key (normalize_refs did not convert it). Resolve it as the
        // command engine does (current or old key); visibility is checked
        // below either way.
        None => match op["issue"]["key"].as_str() {
            Some(k) => {
                sqlx::query_scalar(
                    "SELECT k.issue_id FROM issue_keys k JOIN issues i ON i.id = k.issue_id
                     WHERE k.key = $1 AND i.archived_at IS NULL",
                )
                .bind(k.trim().to_uppercase())
                .fetch_optional(&mut *conn)
                .await?
            }
            None => None,
        },
    };
    let Some(id) = id else {
        return Ok(None);
    };
    type Row = (String, String, Uuid, Uuid, bool, Uuid, Option<Uuid>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT k.key, i.subject, i.project_id, i.tracker_id, i.is_private, i.author_id, i.assigned_to_id
         FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.id = $1 AND i.archived_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    if !keys.contains_key(&id) {
        let scope = crate::db::load_scope(&mut *conn, viewer).await?;
        if !scope.can_see(&ox_core::IssueFacts {
            project_id: r.2,
            tracker_id: r.3,
            is_private: r.4,
            author_id: r.5,
            assigned_to_id: r.6,
        }) {
            return Ok(None);
        }
    }
    let f = &op["fields"];
    let activity = match f["activity_id"].as_str() {
        Some(_) => named(&mut *conn, "activity", &f["activity_id"]).await?,
        None => Json::Null,
    };
    Ok(Some(DiffRow {
        target: DiffTarget {
            id,
            key: r.0,
            subject: r.1,
        },
        field: "time".into(),
        label: "Time".into(),
        before: Json::Null,
        after: json!({"hours": f["hours"], "spent_on": f["spent_on"], "activity": activity,
                      "comments": f["comments"], "user_id": f["user_id"]}),
        original: Json::Null,
        has_original: false,
    }))
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
        for d in ["accept", "snooze"] {
            let ok = doc(json!([{"kind": "triage_decide", "target": t, "decision": d}]));
            assert!(shape_reasons(&ok).is_empty(), "{d}");
        }
        let eighty = "工".repeat(TIME_COMMENTS_TICKABLE);
        for f in [
            json!({"hours": "1"}),
            json!({"hours": "1", "comments": eighty}),
        ] {
            let ok = doc(json!([{"kind": "log_time", "fields": f}]));
            assert!(shape_reasons(&ok).is_empty(), "{f}");
        }
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
                json!([{"kind": "log_time", "fields": {"comments": "工".repeat(TIME_COMMENTS_TICKABLE + 1)}}]),
                "long_field:comments",
            ),
            (
                json!([{"kind": "update_time_entry"}]),
                "op:update_time_entry",
            ),
            (
                json!([{"kind": "triage_decide", "decision": "decline"}]),
                "triage:decline",
            ),
            (
                json!([{"kind": "triage_decide", "decision": "duplicate"}]),
                "triage:duplicate",
            ),
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
