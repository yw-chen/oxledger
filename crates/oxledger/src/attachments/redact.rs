// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 §3.13, R-A12 (D12): an attachment's redaction, inside the
//! redaction transaction (the project row is held `FOR UPDATE`). The name
//! becomes the marker; the blob generation is purged at once
//! (`purge_reason = redacted`); every row of the project that uses the same
//! generation is marked redacted too (the same content is the same secret,
//! even where the redactor cannot see it); unused uploads of it go; the
//! events that carried the name carry the marker. The file is removed after
//! the commit ([`remove_files`]).

use crate::redaction::REDACTED;
use anyhow::Result;
use sqlx::PgConnection;
use std::path::PathBuf;
use uuid::Uuid;

/// A purged generation whose file goes after the commit.
pub struct PurgedFile {
    pub project: Uuid,
    pub blob: Uuid,
}

/// Of `ids`, the attachments not redacted by name yet, in id order. One
/// marked only because it shares a redacted content still carries its name
/// (and may be redacted by name later).
pub(crate) async fn not_named(conn: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT a.id FROM attachments a
         WHERE a.id = ANY($1)
           AND NOT EXISTS (SELECT 1 FROM attachment_redactions r WHERE r.attachment_id = a.id)
         ORDER BY a.id",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?)
}

/// Redacts attachment `id` by name, unless that is done already (`None`).
pub(crate) async fn redact(
    conn: &mut PgConnection,
    id: Uuid,
    by: Uuid,
    reason_code: &str,
    reason: &str,
) -> Result<Option<(Uuid, PurgedFile)>> {
    let row: Option<(Uuid, Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT project_id, blob_id, issue_id, wiki_page_id FROM attachments
         WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((project, blob, issue, page)) = row else {
        return Ok(None);
    };
    if not_named(&mut *conn, &[id]).await?.is_empty() {
        return Ok(None);
    }
    let rid: Uuid = sqlx::query_scalar(
        "INSERT INTO attachment_redactions (attachment_id, redacted_by, reason_code, reason)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(id)
    .bind(by)
    .bind(reason_code)
    .bind(reason)
    .fetch_one(&mut *conn)
    .await?;
    // The generation first (an upload or attach holds it FOR SHARE), then
    // its rows in id order.
    sqlx::query(
        "UPDATE attachment_blobs SET state = 'purged', purged_at = now(), purge_reason = 'redacted'
         WHERE id = $1 AND state = 'present'",
    )
    .bind(blob)
    .execute(&mut *conn)
    .await?;
    // The others with that content: marked, their names kept.
    sqlx::query(
        "UPDATE attachments SET redacted_at = now(), redaction_id = $2
         WHERE id IN (SELECT id FROM attachments
                      WHERE blob_id = $1 AND redacted_at IS NULL AND id <> $3
                      ORDER BY id FOR UPDATE)",
    )
    .bind(blob)
    .bind(rid)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    // This one: the name too (marked already when it shared an earlier
    // redacted content).
    sqlx::query(
        "UPDATE attachments SET filename = $2, redacted_at = coalesce(redacted_at, now()),
                redaction_id = coalesce(redaction_id, $3)
         WHERE id = $1",
    )
    .bind(id)
    .bind(REDACTED)
    .bind(rid)
    .execute(&mut *conn)
    .await?;
    // Uploads of it: unused ones go; used ones keep no name (nothing reads
    // them after the attach).
    sqlx::query("DELETE FROM uploads WHERE blob_id = $1 AND consumed_at IS NULL")
        .bind(blob)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE uploads SET filename = $2 WHERE blob_id = $1 AND filename <> $2")
        .bind(blob)
        .bind(REDACTED)
        .execute(&mut *conn)
        .await?;
    // The name in its container's events (R-A7 payloads, D12 scope).
    let (table, column, holder) = match (issue, page) {
        (Some(i), _) => ("issue_events", "issue_id", i),
        (None, Some(p)) => ("wiki_events", "page_id", p),
        (None, None) => unreachable!("an attachment is on an issue or a page"),
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET payload = jsonb_set(payload, '{{filename}}', to_jsonb($2::text))
         WHERE {column} = $3 AND kind IN ('attachment_added', 'attachment_removed')
           AND payload->>'attachment_id' = $1::text
           AND payload->>'filename' IS DISTINCT FROM $2"
    )))
    .bind(id)
    .bind(REDACTED)
    .bind(holder)
    .execute(&mut *conn)
    .await?;
    Ok(Some((rid, PurgedFile { project, blob })))
}

/// After the commit: the redacted generations' files (a failure is left to
/// the daily scan, R-A8).
pub async fn remove_files(files: &[PurgedFile]) {
    let Some(dir) = super::current().dir.clone() else {
        return;
    };
    for f in files {
        let path: PathBuf = super::blob_path(&dir, f.project, f.blob);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "redacted file not removed");
            }
        }
    }
}
