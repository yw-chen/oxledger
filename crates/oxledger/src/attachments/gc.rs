// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 R-A8 (T-M2-56, T-M2-57): the attachment store's clean-up.
//! Hourly: expired uploads go; a blob generation no live attachment, no
//! unexpired upload uses, whose last delete is older than the undo window,
//! is marked `purged` (committed), then its file is removed. Daily: files
//! with no present generation (an upload whose transaction never committed,
//! a file removal that failed) and stale temporary files are removed.

use super::blob_path;
use anyhow::Result;
use sqlx::{PgConnection, PgPool};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Generations purged per transaction.
const BATCH: i64 = 200;

/// A file younger than this is never an orphan: its upload may not have
/// committed yet (the rename comes before the commit).
pub const ORPHAN_GRACE: Duration = Duration::from_secs(3600);

/// A temporary file older than this is left from an upload that died.
pub const TEMP_GRACE: Duration = Duration::from_secs(24 * 3600);

/// What one hourly round did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HourlyReport {
    pub uploads_removed: u64,
    pub blobs_purged: u64,
    /// Files that could not be removed (the daily scan tries again).
    pub files_failed: u64,
}

/// The condition of a generation the GC may purge (R-A8): no attachment
/// that is neither deleted nor redacted, no unused unexpired upload (a
/// used one is its attachment's), and every
/// delete older than the undo window (an undo puts the file back).
const PURGEABLE: &str = "b.state = 'present'
    AND NOT EXISTS (SELECT 1 FROM attachments a WHERE a.blob_id = b.id
                    AND a.deleted_at IS NULL AND a.redacted_at IS NULL)
    AND NOT EXISTS (SELECT 1 FROM uploads u WHERE u.blob_id = b.id
                    AND u.consumed_at IS NULL AND u.expires_at > now())
    AND NOT EXISTS (SELECT 1 FROM attachments a WHERE a.blob_id = b.id AND a.redacted_at IS NULL
                    AND a.deleted_at > now() - make_interval(days => $1))";

/// One hourly round; `dir` is the files directory.
pub async fn hourly(pool: &PgPool, dir: &Path) -> Result<HourlyReport> {
    let mut r = HourlyReport {
        uploads_removed: sqlx::query(
            "DELETE FROM uploads WHERE consumed_at IS NULL AND expires_at < now()",
        )
        .execute(pool)
        .await?
        .rows_affected(),
        ..Default::default()
    };
    // A cursor over the ids: each generation is looked at once a round.
    let mut after = Uuid::nil();
    loop {
        let mut tx = pool.begin().await?;
        let (seen, purged) = purge_batch(&mut tx, after).await?;
        tx.commit().await?;
        for (project, blob) in &purged {
            if !remove(&blob_path(dir, *project, *blob)).await {
                r.files_failed += 1;
            }
        }
        r.blobs_purged += purged.len() as u64;
        match seen {
            Some(last) => after = last,
            None => return Ok(r),
        }
    }
}

/// One batch after `after`: candidates locked FOR UPDATE (an upload, an
/// attach or an undo putting a file back holds them FOR SHARE), then the
/// condition checked again in a second statement, whose snapshot sees what
/// those committed before the lock was granted. The last id looked at
/// (`None`: no more) and the generations purged.
async fn purge_batch(
    conn: &mut PgConnection,
    after: Uuid,
) -> Result<(Option<Uuid>, Vec<(Uuid, Uuid)>)> {
    let days = crate::undo::UNDO_DAYS as i32;
    let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT b.id FROM attachment_blobs b WHERE b.id > $2 AND {PURGEABLE}
         ORDER BY b.id LIMIT $3 FOR UPDATE OF b SKIP LOCKED"
    )))
    .bind(days)
    .bind(after)
    .bind(BATCH)
    .fetch_all(&mut *conn)
    .await?;
    let Some(last) = ids.last().copied() else {
        return Ok((None, Vec::new()));
    };
    let purged = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE attachment_blobs b SET state = 'purged', purged_at = now(), purge_reason = 'gc'
         WHERE b.id = ANY($2) AND {PURGEABLE}
         RETURNING b.project_id, b.id"
    )))
    .bind(days)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok((Some(last), purged))
}

/// Removes a file; a missing file is fine. `false` when it is still there.
async fn remove(path: &Path) -> bool {
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "attachment file not removed");
            false
        }
    }
}

/// What one daily scan did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport {
    pub orphans_removed: u64,
    pub temp_removed: u64,
    /// Entries whose name is not a generation's (left alone).
    pub unknown: u64,
    /// Entries that could not be read (left alone, logged).
    pub errors: u64,
}

/// The daily scan (T-M2-57): every file `dir/<project>/<xx>/<blob>` (the
/// ids in simple form, `xx` the blob's first two characters) with no
/// present generation of that project and older than `grace` is removed;
/// `.tmp` files older than `temp_grace` too. A project at a time; an entry
/// that cannot be read is skipped.
pub async fn orphan_scan(
    pool: &PgPool,
    dir: &Path,
    grace: Duration,
    temp_grace: Duration,
) -> Result<ScanReport> {
    let root = dir.to_path_buf();
    let (projects, temps, mut r) = tokio::task::spawn_blocking(move || top(&root)).await?;
    let now = SystemTime::now();
    let old = |m: SystemTime, g: Duration| now.duration_since(m).is_ok_and(|age| age >= g);
    for (path, m) in temps {
        if old(m, temp_grace) && remove(&path).await {
            r.temp_removed += 1;
        }
    }
    for (project, path) in projects {
        let (files, unknown, errors) =
            tokio::task::spawn_blocking(move || project_files(&path)).await?;
        r.unknown += unknown;
        r.errors += errors;
        for chunk in files.chunks(1000) {
            let blobs: Vec<Uuid> = chunk.iter().map(|f| f.blob).collect();
            let present: HashSet<Uuid> = sqlx::query_scalar(
                "SELECT id FROM attachment_blobs
                 WHERE project_id = $1 AND id = ANY($2) AND state = 'present'",
            )
            .bind(project)
            .bind(&blobs)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
            for f in chunk {
                if present.contains(&f.blob) || !old(f.modified, grace) {
                    continue;
                }
                if remove(&f.path).await {
                    r.orphans_removed += 1;
                }
            }
        }
    }
    Ok(r)
}

struct Found {
    path: PathBuf,
    blob: Uuid,
    modified: SystemTime,
}

/// A file name that is a uuid in simple form (32 lowercase hex digits).
fn simple_id(name: &str) -> Option<Uuid> {
    let ok = name.len() == 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| Uuid::parse_str(name).ok()).flatten()
}

fn note_error(path: &Path, e: &std::io::Error, errors: &mut u64) {
    // Gone in between (an upload's temporary file renamed, a file removed
    // after a redaction): nothing to do.
    if e.kind() != std::io::ErrorKind::NotFound {
        tracing::warn!(path = %path.display(), error = %e, "attachment scan: entry not read");
        *errors += 1;
    }
}

type Top = (Vec<(Uuid, PathBuf)>, Vec<(PathBuf, SystemTime)>, ScanReport);

/// The project directories and the temporary files.
fn top(root: &Path) -> Top {
    let mut r = ScanReport::default();
    let mut projects = Vec::new();
    let mut temps = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            note_error(root, &e, &mut r.errors);
            return (projects, temps, r);
        }
    };
    for p in entries.flatten() {
        let name = p.file_name().to_string_lossy().into_owned();
        let is_dir = p.file_type().is_ok_and(|t| t.is_dir());
        if name == ".tmp" && is_dir {
            match std::fs::read_dir(p.path()) {
                Ok(list) => {
                    for t in list.flatten() {
                        if !t.file_type().is_ok_and(|t| t.is_file()) {
                            continue;
                        }
                        match t.metadata().and_then(|m| m.modified()) {
                            Ok(m) => temps.push((t.path(), m)),
                            Err(e) => note_error(&t.path(), &e, &mut r.errors),
                        }
                    }
                }
                Err(e) => note_error(&p.path(), &e, &mut r.errors),
            }
            continue;
        }
        match simple_id(&name) {
            Some(project) if is_dir => projects.push((project, p.path())),
            _ => r.unknown += 1,
        }
    }
    (projects, temps, r)
}

/// One project's generation files: (files, unknown, errors).
fn project_files(dir: &Path) -> (Vec<Found>, u64, u64) {
    let mut files = Vec::new();
    let (mut unknown, mut errors) = (0, 0);
    let shards = match std::fs::read_dir(dir) {
        Ok(s) => s,
        Err(e) => {
            note_error(dir, &e, &mut errors);
            return (files, unknown, errors);
        }
    };
    for shard in shards.flatten() {
        let sname = shard.file_name().to_string_lossy().into_owned();
        if !shard.file_type().is_ok_and(|t| t.is_dir()) || sname.len() != 2 {
            unknown += 1;
            continue;
        }
        let list = match std::fs::read_dir(shard.path()) {
            Ok(l) => l,
            Err(e) => {
                note_error(&shard.path(), &e, &mut errors);
                continue;
            }
        };
        for f in list.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            let blob = simple_id(&fname).filter(|_| fname.starts_with(&sname));
            match blob {
                Some(blob) if f.file_type().is_ok_and(|t| t.is_file()) => {
                    match f.metadata().and_then(|m| m.modified()) {
                        Ok(modified) => files.push(Found {
                            path: f.path(),
                            blob,
                            modified,
                        }),
                        Err(e) => note_error(&f.path(), &e, &mut errors),
                    }
                }
                _ => unknown += 1,
            }
        }
    }
    (files, unknown, errors)
}
