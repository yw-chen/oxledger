// SPDX-License-Identifier: GPL-2.0-or-later
//! Helpers for tests and `oxledger seed-demo` only (contract 02 §2 P5).
//!
//! Compiled only with the `test-support` feature, so nothing in a release
//! build can reach them. They write rows directly: no permission check, no
//! workflow, no issue events. Real writes go through the command engine.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

/// Fields of an issue created by [`create_issue`].
pub struct NewIssue<'a> {
    pub project_id: Uuid,
    pub tracker_id: Uuid,
    pub status_id: Uuid,
    pub subject: &'a str,
    pub description: &'a str,
    pub author_id: Uuid,
    pub assigned_to_id: Option<Uuid>,
    pub is_private: bool,
    pub parent_id: Option<Uuid>,
}

/// Inserts an issue with the next project key (D9) and returns (id, key).
pub async fn create_issue(pool: &PgPool, n: NewIssue<'_>) -> Result<(Uuid, String)> {
    let mut tx = pool.begin().await?;
    let (pkey, number): (String, i32) = sqlx::query_as(
        "UPDATE projects SET issue_counter = issue_counter + 1 WHERE id = $1 RETURNING key, issue_counter",
    )
    .bind(n.project_id)
    .fetch_one(&mut *tx)
    .await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id,
                             assigned_to_id, is_private, parent_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
    )
    .bind(n.project_id)
    .bind(n.tracker_id)
    .bind(n.status_id)
    .bind(n.subject)
    .bind(n.description)
    .bind(n.author_id)
    .bind(n.assigned_to_id)
    .bind(n.is_private)
    .bind(n.parent_id)
    .fetch_one(&mut *tx)
    .await?;
    let key = format!("{pkey}-{number}");
    sqlx::query("INSERT INTO issue_keys (key, issue_id, project_id, number) VALUES ($1,$2,$3,$4)")
        .bind(&key)
        .bind(id)
        .bind(n.project_id)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((id, key))
}
