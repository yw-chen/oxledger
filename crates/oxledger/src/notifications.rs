// SPDX-License-Identifier: GPL-2.0-or-later
//! Watchers and the in-app notification centre (design 05 §2.10, §3.11,
//! §4.6, §6.15–§6.17; M1.8, A2, K9). No email (M2).
//!
//! - **Dispatch** ([`dispatch_round`]): the outbox consumer `notify` reads
//!   `topic = 'issue'` messages; for every issue with events after its
//!   `notification_issue_progress` row it works out the candidate recipients
//!   of each event (watching, assigned, author, triage_decided,
//!   agent_on_behalf), drops the actor (unless `on_self`), muted projects,
//!   people who are not active users and every agent, and then checks
//!   **each recipient with their own permissions now**: the issue must be
//!   visible to them and the event must have something they can read — the
//!   same [`issue_view::event_view`] that renders issue history (private
//!   notes, restricted fields, the other end of a duplicate). Redacted
//!   events are not sent. Survivors join the recipient's unread row of the
//!   issue under `FOR UPDATE` (appendix D V8). One transaction per issue
//!   writes the notifications, the automatic watchers and the progress row;
//!   the outbox cursor moves after the batch. A message seen twice finds the
//!   progress row already past its events, so nothing is sent twice.
//! - **Reading** ([`list`], [`unread_count`]): every row is checked again on
//!   every read with the reader's permissions at that moment; a row with no
//!   visible event is left out without a trace, so revoked access,
//!   privatised issues, restricted fields and hidden notes disappear on the
//!   next read. Rows hold references only; the text comes from
//!   `issue_events` and `issue_notes` each time (a D12 redaction shows as a
//!   redaction marker).
//! - **Watching** ([`watch`]): only yourself, only issues you can see (an
//!   invisible issue answers exactly like a missing one); `watcher_count`
//!   counts active users who can see the issue now, at most 100 (B-46).
//! - Agents get none of this (§4.6): there is no tool, and agent principals
//!   are never recipients or watchers (DB triggers too).

use crate::auth::AuthContext;
use crate::citations::Reader;
use crate::commands;
use crate::cursor::{self, Position};
use crate::db;
use crate::error::{ApiError, ErrorCode, api, classify};
use crate::issue_view::{self, EventCache, EventRow, IssueRef};
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ox_core::IssueFacts;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Postgres};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The outbox consumer (`outbox_cursors.consumer`).
pub const CONSUMER: &str = "notify";
/// `notifications.reasons` (0028 CHECK; `mentioned` from 0032, dispatched
/// from M2.1, design 06 §3.4).
pub const REASONS: [&str; 7] = [
    "watching",
    "assigned",
    "author",
    "agent_on_behalf",
    "triage_decided",
    "ai_budget",
    "mentioned",
];
/// `notifications.kind` (0028 CHECK).
pub const KINDS: [&str; 2] = ["issue", "ai_budget"];
/// `issue_watchers.source` (0028 CHECK).
pub const WATCH_SOURCES: [&str; 4] = ["manual", "auto_author", "auto_assignee", "auto_note"];
/// Event ids kept per notification row (0028 CHECK); older ones are dropped.
pub const MAX_EVENTS: usize = 50;
/// The bell shows `99+` above this (§3.11.3).
pub const UNREAD_CAP: u32 = 99;
/// `watcher_count` counts at most this many (B-46).
pub const WATCHER_CAP: u32 = 100;
/// Retention (B-25): read rows this many days after being read, unread rows
/// this many days after their last event arrived.
pub const READ_RETENTION_DAYS: i32 = 30;
pub const UNREAD_RETENTION_DAYS: i32 = 90;
/// Outbox messages per dispatch round.
const BATCH: i64 = 200;
/// Issues dispatched in parallel within a round, unless
/// `OXLEDGER_NOTIFY_WORKERS` (1–16) says otherwise. Each worker holds one
/// pool connection while it works.
const DEFAULT_WORKERS: usize = 4;
/// Consecutive failures of one issue after which its pending events are
/// skipped (logged, `notify_skipped_issues_total`), so one broken issue
/// cannot hold back every other notification and the outbox GC.
pub const MAX_ISSUE_FAILURES: u32 = 5;
/// Last unread counts kept (per person); beyond this the cache starts over.
const COUNTS_CAP: usize = 10_000;
/// `unread-count` answers within this; afterwards the last value (§3.11.3).
const COUNT_BUDGET: Duration = Duration::from_secs(2);
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 100;
/// Rows one list request examines at most; the cursor then continues after
/// the last one examined (hidden rows cost reads, not results).
const SCAN_LIMIT: usize = 500;
/// Events shown per list item (newest first); `more_events` counts the rest.
const ITEM_EVENTS: usize = 3;
/// `note_excerpt` length in characters (§3.11.3).
const EXCERPT_CHARS: usize = 140;
/// Watchers examined for `watcher_count`.
const WATCHER_SCAN: i64 = 1000;
/// Muted projects per person.
pub const MAX_MUTES: usize = 200;
const ACTION_PREFS: &str = "me_notification_preferences_put";

/// Dispatch workers (`OXLEDGER_NOTIFY_WORKERS`, 1–16, default 4).
pub fn workers() -> usize {
    static W: OnceLock<usize> = OnceLock::new();
    *W.get_or_init(|| match std::env::var("OXLEDGER_NOTIFY_WORKERS") {
        Err(_) => DEFAULT_WORKERS,
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if (1..=16).contains(&n) => n,
            _ => {
                tracing::warn!(
                    "OXLEDGER_NOTIFY_WORKERS={v:?} is not 1–16; using {DEFAULT_WORKERS}"
                );
                DEFAULT_WORKERS
            }
        },
    })
}

/// Consecutive dispatch failures by issue (this process).
static FAILURES: Mutex<Option<HashMap<Uuid, u32>>> = Mutex::new(None);

fn failures(f: impl FnOnce(&mut HashMap<Uuid, u32>) -> u32) -> u32 {
    match FAILURES.lock() {
        Ok(mut g) => f(g.get_or_insert_with(HashMap::new)),
        Err(_) => 0,
    }
}

/// Errors that say the database is unreachable rather than that this issue
/// is broken: they do not count towards [`MAX_ISSUE_FAILURES`].
fn systemic(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<sqlx::Error>(),
            Some(
                sqlx::Error::Io(_)
                    | sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::Tls(_)
                    | sqlx::Error::Protocol(_)
                    | sqlx::Error::WorkerCrashed
            )
        )
    })
}

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- issue facts

#[derive(sqlx::FromRow)]
struct IssueRow {
    id: Uuid,
    project_id: Uuid,
    tracker_id: Uuid,
    is_private: bool,
    author_id: Uuid,
    created_on_behalf_of_id: Option<Uuid>,
    assigned_to_id: Option<Uuid>,
    archived: bool,
}

const ISSUE_SELECT: &str = "SELECT i.id, i.project_id, i.tracker_id, i.is_private, i.author_id,
        i.created_on_behalf_of_id, i.assigned_to_id, i.archived_at IS NOT NULL AS archived
 FROM issues i";

impl IssueRow {
    fn facts(&self) -> IssueFacts {
        IssueFacts {
            project_id: self.project_id,
            tracker_id: self.tracker_id,
            is_private: self.is_private,
            author_id: self.author_id,
            assigned_to_id: self.assigned_to_id,
        }
    }

    fn reference(&self) -> IssueRef {
        IssueRef {
            id: self.id,
            project: self.project_id,
            tracker: self.tracker_id,
        }
    }

    /// The person behind the author: an agent author's delegator (§3.11.2).
    fn author_person(&self) -> Uuid {
        self.created_on_behalf_of_id.unwrap_or(self.author_id)
    }
}

async fn issue_row(conn: &mut PgConnection, id: Uuid) -> Result<Option<IssueRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ISSUE_SELECT} WHERE i.id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The issue behind `key` (old keys of moved issues too) if `caller` can
/// see it; missing and invisible are the same `not_found`, and the scope is
/// loaded first either way (as `service::get_in`).
async fn visible_issue(
    conn: &mut PgConnection,
    caller: AuthContext,
    key: &str,
) -> Result<IssueRow> {
    let key = key.trim().to_uppercase();
    let not_found = || api(ErrorCode::NotFound, format!("no issue {key}"));
    let scope = db::load_scope(&mut *conn, caller).await?;
    if !crate::describe::well_formed_key(&key) {
        return Err(not_found());
    }
    let row: Option<IssueRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ISSUE_SELECT} JOIN issue_keys rk ON rk.issue_id = i.id WHERE rk.key = $1"
    )))
    .bind(&key)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(r) if !r.archived && scope.can_see(&r.facts()) => Ok(r),
        _ => Err(not_found()),
    }
}

// ---------------------------------------------------------------- recipients

/// What one person may read, loaded once per dispatch round or request.
struct Recipient {
    ctx: AuthContext,
    reader: Reader,
    /// Readable custom field ids by project.
    cf: HashMap<Uuid, Vec<i32>>,
}

impl Recipient {
    /// `None` when the person cannot read anything any more (locked,
    /// deleted): the same as "sees nothing".
    async fn load(conn: &mut PgConnection, ctx: AuthContext) -> Result<Option<Recipient>> {
        match Reader::new(&mut *conn, ctx).await {
            Ok(reader) => Ok(Some(Recipient {
                ctx,
                reader,
                cf: HashMap::new(),
            })),
            Err(e) if classify(&e).is_some() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn sees_issue(&self, issue: &IssueRow) -> bool {
        !issue.archived && self.reader.scope().can_see(&issue.facts())
    }

    /// [`issue_view::event_view`] for this person, reusing the readable
    /// fields of the project.
    async fn view(
        &mut self,
        conn: &mut PgConnection,
        issue: &IssueRow,
        row: EventRow,
    ) -> Result<Option<issue_view::EventView>> {
        let mut cache = EventCache::default();
        cache.readable_cf = self.cf.get(&issue.project_id).cloned();
        let v = issue_view::event_view(
            &mut *conn,
            &mut self.reader,
            self.ctx,
            issue.reference(),
            &mut cache,
            row,
        )
        .await?;
        if let Some(cf) = cache.readable_cf {
            self.cf.insert(issue.project_id, cf);
        }
        Ok(v)
    }
}

#[derive(Default)]
struct Recipients(HashMap<Uuid, Option<Recipient>>);

impl Recipients {
    async fn get(&mut self, conn: &mut PgConnection, user: Uuid) -> Result<Option<&mut Recipient>> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.0.entry(user) {
            let r = Recipient::load(&mut *conn, AuthContext::notification_recipient(user)).await?;
            e.insert(r);
        }
        Ok(self.0.get_mut(&user).and_then(Option::as_mut))
    }
}

// ---------------------------------------------------------------- dispatch

#[derive(sqlx::FromRow, Clone)]
struct DispatchEvent {
    id: Uuid,
    seq: i64,
    kind: String,
    at: DateTime<Utc>,
    actor: String,
    actor_id: Uuid,
    actor_kind: String,
    on_behalf_of_id: Option<Uuid>,
    payload: Json,
    note_id: Option<Uuid>,
    redacted: bool,
}

impl DispatchEvent {
    fn row(&self) -> EventRow {
        EventRow {
            id: self.id,
            seq: self.seq,
            kind: self.kind.clone(),
            at: self.at,
            actor: self.actor.clone(),
            payload: self.payload.clone(),
            note_id: self.note_id,
            redacted: self.redacted,
        }
    }

    /// The person who acted: the user, or an agent's delegator.
    fn acting_person(&self) -> Option<Uuid> {
        match self.actor_kind.as_str() {
            "user" => Some(self.actor_id),
            "agent" => self.on_behalf_of_id,
            _ => None,
        }
    }

    /// `(old, new)` of an assignment change in this event, if any.
    fn assignment(&self) -> Option<(Option<Uuid>, Option<Uuid>)> {
        if self.redacted {
            return None;
        }
        let id = |v: &Json| v.as_str().and_then(|s| s.parse::<Uuid>().ok());
        match self.kind.as_str() {
            "created" => self.payload["fields"]
                .get("assigned_to_id")
                .map(|v| (None, id(v))),
            "fields_changed" => self.payload["changes"]
                .get("assigned_to_id")
                .map(|c| (id(&c["old"]), id(&c["new"]))),
            _ => None,
        }
    }
}

const EVENT_SELECT: &str =
    "SELECT e.id, e.seq, e.kind, e.at, p.name AS actor, e.actor_id, e.actor_kind,
        e.on_behalf_of_id, e.payload, e.note_id, e.redaction_id IS NOT NULL AS redacted
 FROM issue_events e JOIN principals p ON p.id = e.actor_id";

/// A person's switches (defaults when they never opened the settings).
#[derive(sqlx::FromRow, Clone, Copy)]
struct Switches {
    id: Uuid,
    on_watching: bool,
    on_assigned: bool,
    on_author: bool,
    on_triage_decided: bool,
    on_my_agents: bool,
    on_self: bool,
    on_mentioned: bool,
    auto_watch_author: bool,
    auto_watch_assignee: bool,
    auto_watch_note: bool,
    muted: bool,
}

impl Switches {
    fn wants(&self, reason: &str) -> bool {
        match reason {
            "watching" => self.on_watching,
            "assigned" => self.on_assigned,
            "author" => self.on_author,
            "triage_decided" => self.on_triage_decided,
            "agent_on_behalf" => self.on_my_agents,
            "mentioned" => self.on_mentioned,
            _ => false,
        }
    }
}

/// Who the readable content of these events still mentions (design 06
/// R-MN5): the `issue_event_mentions` rows (written with the event, deleted
/// with a D12 redaction), but none for a note that is retracted or
/// redacted now: its text is no longer there to read.
async fn live_mentions(
    conn: &mut PgConnection,
    events: &[Uuid],
) -> Result<HashMap<Uuid, BTreeSet<Uuid>>> {
    let mut out: HashMap<Uuid, BTreeSet<Uuid>> = HashMap::new();
    if events.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT m.event_id, m.user_id FROM issue_event_mentions m
         JOIN issue_events e ON e.id = m.event_id
         LEFT JOIN issue_notes n ON n.id = e.note_id
         WHERE m.event_id = ANY($1) AND e.redaction_id IS NULL
           AND (e.note_id IS NULL OR (n.redacted_at IS NULL AND n.retracted_at IS NULL))",
    )
    .bind(events)
    .fetch_all(&mut *conn)
    .await?;
    for (e, u) in rows {
        out.entry(e).or_default().insert(u);
    }
    Ok(out)
}

/// Active users among `ids` with their switches and whether they muted
/// `project`. Agents, groups, system principals and locked users are not here.
async fn switches(
    conn: &mut PgConnection,
    ids: &[Uuid],
    project: Uuid,
) -> Result<HashMap<Uuid, Switches>> {
    let rows: Vec<Switches> = sqlx::query_as(
        "SELECT p.id,
                COALESCE(np.on_watching, true) AS on_watching,
                COALESCE(np.on_assigned, true) AS on_assigned,
                COALESCE(np.on_author, true) AS on_author,
                COALESCE(np.on_triage_decided, true) AS on_triage_decided,
                COALESCE(np.on_my_agents, true) AS on_my_agents,
                COALESCE(np.on_self, false) AS on_self,
                COALESCE(np.on_mentioned, true) AS on_mentioned,
                COALESCE(np.auto_watch_author, true) AS auto_watch_author,
                COALESCE(np.auto_watch_assignee, true) AS auto_watch_assignee,
                COALESCE(np.auto_watch_note, true) AS auto_watch_note,
                EXISTS (SELECT 1 FROM notification_project_mutes m
                        WHERE m.user_id = p.id AND m.project_id = $2) AS muted
         FROM principals p LEFT JOIN notification_preferences np ON np.user_id = p.id
         WHERE p.id = ANY($1) AND p.kind = 'user' AND p.status = 'active'",
    )
    .bind(ids)
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|s| (s.id, s)).collect())
}

/// A principal as people: a user is themselves, a group its active user
/// members (§3.11.2: assignment to a group reaches its members), anything
/// else nobody.
async fn people(
    conn: &mut PgConnection,
    cache: &mut HashMap<Uuid, Vec<Uuid>>,
    p: Uuid,
) -> Result<Vec<Uuid>> {
    if let Some(v) = cache.get(&p) {
        return Ok(v.clone());
    }
    let v: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM principals WHERE id = $1 AND kind = 'user'
         UNION
         SELECT u.id FROM group_members gm
         JOIN principals g ON g.id = gm.group_id AND g.kind = 'group'
         JOIN principals u ON u.id = gm.user_id AND u.kind = 'user' AND u.status = 'active'
         WHERE gm.group_id = $1",
    )
    .bind(p)
    .fetch_all(&mut *conn)
    .await?;
    cache.insert(p, v.clone());
    Ok(v)
}

/// What one round did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Round {
    /// Outbox messages read.
    pub messages: usize,
    /// Issue events dispatched (whoever received them).
    pub events: usize,
    /// (recipient, issue) rows written or extended.
    pub delivered: usize,
    /// A full batch: more may follow right away.
    pub more: bool,
}

/// One dispatch round (§3.11.2): up to [`BATCH`] outbox messages below the
/// snapshot horizon, each issue with new events in its own transaction,
/// then the cursor.
pub async fn dispatch_round(pool: &PgPool) -> Result<Round> {
    let started = Instant::now();
    let r = dispatch_round_inner(pool).await;
    crate::metrics::NOTIFY.round(started.elapsed(), r.as_ref().ok());
    r
}

async fn dispatch_round_inner(pool: &PgPool) -> Result<Round> {
    // Missing only after an operator removed it: start from the beginning;
    // the progress rows keep anything from being sent twice.
    sqlx::query(
        "INSERT INTO outbox_cursors (consumer, last_xid, last_id, state)
         VALUES ($1, '0', 0, 'active') ON CONFLICT (consumer) DO NOTHING",
    )
    .bind(CONSUMER)
    .execute(pool)
    .await?;
    let (xid, id): (String, i64) =
        sqlx::query_as("SELECT last_xid::text, last_id FROM outbox_cursors WHERE consumer = $1")
            .bind(CONSUMER)
            .fetch_one(pool)
            .await?;
    // H and the rows come from one snapshot: everything below H is
    // committed or gone (as the search consumer, 03 §4.2).
    let rows: Vec<(i64, String, Uuid)> = sqlx::query_as(
        "WITH h AS MATERIALIZED (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS h)
         SELECT o.id, o.xid::text, o.entity_id FROM outbox o, h
         WHERE o.topic = 'issue' AND o.xid < h.h
           AND (o.xid, o.id) > ($1::text::xid8, $2::bigint)
         ORDER BY o.xid, o.id LIMIT $3",
    )
    .bind(&xid)
    .bind(id)
    .bind(BATCH)
    .fetch_all(pool)
    .await?;
    let Some((last_id, last_xid, _)) = rows.last().cloned() else {
        return Ok(Round::default());
    };
    let mut ids: Vec<Uuid> = Vec::new();
    for (_, _, i) in &rows {
        if !ids.contains(i) {
            ids.push(*i);
        }
    }
    // Most messages (search-only changes, repeats) have no new event.
    let pending: BTreeSet<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT e.issue_id FROM issue_events e
         LEFT JOIN notification_issue_progress np ON np.issue_id = e.issue_id
         WHERE e.issue_id = ANY($1) AND e.seq > COALESCE(np.last_event_seq, 0)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let mut round = Round {
        messages: rows.len(),
        more: rows.len() as i64 == BATCH,
        ..Round::default()
    };
    // Issues are independent (own progress row, own unread rows): a few
    // workers, each with its own connection and recipient cache. A failing
    // issue does not stop the others.
    let todo: Vec<Uuid> = ids.into_iter().filter(|i| pending.contains(i)).collect();
    let workers = workers().min(todo.len()).max(1);
    let mut set = tokio::task::JoinSet::new();
    for w in 0..workers {
        let chunk: Vec<Uuid> = todo.iter().skip(w).step_by(workers).copied().collect();
        let pool = pool.clone();
        set.spawn(async move {
            let mut rcs = Recipients::default();
            let (mut events, mut delivered) = (0, 0);
            let mut failed: Vec<(Uuid, anyhow::Error)> = Vec::new();
            for issue in chunk {
                match dispatch_issue(&pool, &mut rcs, issue).await {
                    Ok((e, d)) => {
                        events += e;
                        delivered += d;
                        failures(|m| {
                            m.remove(&issue);
                            0
                        });
                    }
                    Err(e) => {
                        // The cache may hold a half-loaded recipient.
                        rcs = Recipients::default();
                        failed.push((issue, e));
                    }
                }
            }
            (events, delivered, failed)
        });
    }
    let mut blocking: Option<anyhow::Error> = None;
    while let Some(r) = set.join_next().await {
        let (e, d, failed) = match r {
            Ok(x) => x,
            Err(j) => {
                blocking.get_or_insert(j.into());
                continue;
            }
        };
        round.events += e;
        round.delivered += d;
        for (issue, err) in failed {
            if systemic(&err) {
                blocking.get_or_insert(err);
                continue;
            }
            let n = failures(|m| {
                let n = m.entry(issue).or_insert(0);
                *n += 1;
                *n
            });
            if n < MAX_ISSUE_FAILURES {
                tracing::warn!(%issue, attempt = n, "notification dispatch failed: {err:#}");
                blocking.get_or_insert(err);
                continue;
            }
            tracing::error!(
                %issue,
                "notification dispatch failed {n} times in a row; skipping the issue's pending events: {err:#}"
            );
            if let Err(e) = skip_issue(pool, issue).await {
                blocking.get_or_insert(e);
                continue;
            }
            failures(|m| {
                m.remove(&issue);
                0
            });
            crate::metrics::NOTIFY.skipped();
        }
    }
    if let Some(e) = blocking {
        return Err(e);
    }
    fault("notify.before_cursor").await?;
    sqlx::query(
        "UPDATE outbox_cursors SET last_xid = $2::text::xid8, last_id = $3, updated_at = now()
         WHERE consumer = $1 AND (last_xid, last_id) < ($2::text::xid8, $3)",
    )
    .bind(CONSUMER)
    .bind(&last_xid)
    .bind(last_id)
    .execute(pool)
    .await?;
    Ok(round)
}

/// The events of one issue after its progress row, in one transaction:
/// (events dispatched, rows written).
async fn dispatch_issue(
    pool: &PgPool,
    rcs: &mut Recipients,
    issue_id: Uuid,
) -> Result<(usize, usize)> {
    fault(&format!("notify.issue:{issue_id}")).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    // A row for issues created after the upgrade (seq 0); the lock keeps two
    // dispatchers from sending the same events.
    // (DO UPDATE locks an existing row as FOR UPDATE would.)
    let progress: i64 = sqlx::query_scalar(
        "INSERT INTO notification_issue_progress AS np (issue_id, last_event_seq) VALUES ($1, 0)
         ON CONFLICT (issue_id) DO UPDATE SET last_event_seq = np.last_event_seq
         RETURNING last_event_seq",
    )
    .bind(issue_id)
    .fetch_one(&mut *tx)
    .await?;
    let events: Vec<DispatchEvent> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{EVENT_SELECT} WHERE e.issue_id = $1 AND e.seq > $2 ORDER BY e.seq"
    )))
    .bind(issue_id)
    .bind(progress)
    .fetch_all(&mut *tx)
    .await?;
    let Some(last_seq) = events.last().map(|e| e.seq) else {
        tx.commit().await?;
        return Ok((0, 0));
    };
    let mut delivered = 0;
    if let Some(issue) = issue_row(&mut tx, issue_id).await?
        && !issue.archived
    {
        delivered = deliver_events(&mut tx, rcs, &issue, &events).await?;
    }
    sqlx::query("UPDATE notification_issue_progress SET last_event_seq = $2 WHERE issue_id = $1")
        .bind(issue_id)
        .bind(last_seq)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let now = Utc::now();
    for e in &events {
        crate::metrics::NOTIFY.delay((now - e.at).to_std().unwrap_or_default());
    }
    Ok((events.len(), delivered))
}

/// Moves an issue's progress past its last event without sending anything
/// (after [`MAX_ISSUE_FAILURES`]).
async fn skip_issue(pool: &PgPool, issue_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO notification_issue_progress AS np (issue_id, last_event_seq)
         SELECT $1, COALESCE(max(seq), 0) FROM issue_events WHERE issue_id = $1
         ON CONFLICT (issue_id) DO UPDATE SET last_event_seq = GREATEST(np.last_event_seq, EXCLUDED.last_event_seq)",
    )
    .bind(issue_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Candidates, preferences, the per-recipient re-check, the unread rows
/// and automatic watching for `events` (seq order) of `issue`.
async fn deliver_events(
    conn: &mut PgConnection,
    rcs: &mut Recipients,
    issue: &IssueRow,
    events: &[DispatchEvent],
) -> Result<usize> {
    // Assignee after each event: walk back from the current one.
    let n = events.len();
    let mut after: Vec<Option<Uuid>> = vec![None; n];
    let mut removed: Vec<Option<Uuid>> = vec![None; n];
    let mut cur = issue.assigned_to_id;
    for i in (0..n).rev() {
        after[i] = cur;
        if events[i].kind == "created" {
            cur = None;
        } else if let Some((old, _)) = events[i].assignment() {
            removed[i] = old;
            cur = old;
        }
    }
    let watchers: Vec<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM issue_watchers WHERE issue_id = $1")
            .bind(issue.id)
            .fetch_all(&mut *conn)
            .await?;
    let author = issue.author_person();
    let mut groups: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
    let mentioned = live_mentions(&mut *conn, &ids).await?;
    // Candidates per event: person → reasons.
    let mut cands: Vec<BTreeMap<Uuid, BTreeSet<&'static str>>> = Vec::with_capacity(n);
    for (i, ev) in events.iter().enumerate() {
        let mut c: BTreeMap<Uuid, BTreeSet<&'static str>> = BTreeMap::new();
        // Design 06 R-MN5: the people the content mentions (recorded only
        // for people's writes, R-MN4). Like every reason, each must read
        // the event with their own permissions below; no automatic
        // watching for a mention.
        for u in mentioned.get(&ev.id).into_iter().flatten() {
            c.entry(*u).or_default().insert("mentioned");
        }
        for w in &watchers {
            c.entry(*w).or_default().insert("watching");
        }
        for a in [after[i], removed[i]].into_iter().flatten() {
            for u in people(&mut *conn, &mut groups, a).await? {
                c.entry(u).or_default().insert("assigned");
            }
        }
        c.entry(author).or_default().insert("author");
        if ev.kind == "triage_changed"
            && !ev.redacted
            && matches!(
                ev.payload["new"].as_str(),
                Some("accepted" | "declined" | "duplicate")
            )
        {
            c.entry(author).or_default().insert("triage_decided");
        }
        if ev.actor_kind == "agent"
            && let Some(d) = ev.on_behalf_of_id
        {
            c.entry(d).or_default().insert("agent_on_behalf");
        }
        cands.push(c);
    }
    // Automatic watching (§2.10): the author, a new assignee (not a
    // group's members), whoever adds a note; an agent's delegator for it.
    let mut auto: Vec<(Uuid, &'static str)> = Vec::new();
    for ev in events.iter().filter(|e| !e.redacted) {
        if ev.kind == "created" {
            auto.push((author, "auto_author"));
        }
        if let Some((_, Some(new))) = ev.assignment() {
            auto.push((new, "auto_assignee"));
        }
        if ev.kind == "note_added"
            && let Some(p) = ev.acting_person()
        {
            auto.push((p, "auto_note"));
        }
    }
    let mut everyone: BTreeSet<Uuid> = cands.iter().flat_map(|c| c.keys().copied()).collect();
    everyone.extend(auto.iter().map(|a| a.0));
    let everyone: Vec<Uuid> = everyone.into_iter().collect();
    let sw = switches(&mut *conn, &everyone, issue.project_id).await?;

    // Per recipient: the events they may see, each with its reasons.
    let mut out: BTreeMap<Uuid, Vec<(Uuid, String)>> = BTreeMap::new();
    let mut sees_issue: HashMap<Uuid, bool> = HashMap::new();
    for (ev, c) in events.iter().zip(cands) {
        if ev.redacted {
            continue;
        }
        for (user, reasons) in c {
            let Some(s) = sw.get(&user) else { continue };
            if s.muted || (!s.on_self && ev.actor_kind == "user" && ev.actor_id == user) {
                continue;
            }
            // on_my_agents off: nothing my own agents did, whatever the
            // reason (watching, author, …).
            if !s.on_my_agents && ev.actor_kind == "agent" && ev.on_behalf_of_id == Some(user) {
                continue;
            }
            let reasons: Vec<&'static str> = reasons.into_iter().filter(|r| s.wants(r)).collect();
            if reasons.is_empty() {
                continue;
            }
            let Some(rc) = rcs.get(&mut *conn, user).await? else {
                continue;
            };
            let visible = *sees_issue
                .entry(user)
                .or_insert_with(|| rc.sees_issue(issue));
            if !visible || rc.view(&mut *conn, issue, ev.row()).await?.is_none() {
                continue;
            }
            out.entry(user)
                .or_default()
                .push((ev.id, reasons.join(",")));
        }
    }
    deliver(&mut *conn, issue, &out).await?;
    let delivered = out.len();
    for (user, source) in auto {
        let Some(s) = sw.get(&user) else { continue };
        let on = match source {
            "auto_author" => s.auto_watch_author,
            "auto_assignee" => s.auto_watch_assignee,
            _ => s.auto_watch_note,
        };
        if on {
            // The same lock as `watch`: an unwatch running now either
            // finishes first (its opt-out is seen here) or waits for this
            // transaction and removes the row afterwards.
            lock_watch(&mut *conn, issue.id, user).await?;
            sqlx::query(
                "INSERT INTO issue_watchers (issue_id, user_id, source)
                 SELECT $1, $2, $3
                 WHERE NOT EXISTS (SELECT 1 FROM issue_watch_optouts
                                   WHERE issue_id = $1 AND user_id = $2)
                 ON CONFLICT (issue_id, user_id) DO NOTHING",
            )
            .bind(issue.id)
            .bind(user)
            .bind(source)
            .execute(&mut *conn)
            .await?;
        }
    }
    fault("notify.after_auto_watch").await?;
    Ok(delivered)
}

/// Serializes watching changes of one (issue, person): `watch` and the
/// automatic watching of a dispatch (held until commit).
async fn lock_watch(conn: &mut PgConnection, issue: Uuid, user: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('notify-watch:' || $1::text || ':' || $2::text, 0))")
        .bind(issue)
        .bind(user)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Serializes the unread rows of `users` (held until commit): dispatch
/// takes these before it reads the clock for `last_at`, and `read-all`
/// takes the caller's, so for one person rows commit in `last_at` order
/// and a list never misses a row with an earlier `last_at` that commits
/// later (`read-all {before}` marks only what the list could have shown).
/// Locks are taken in key order (no deadlock between dispatchers).
async fn lock_recipients(conn: &mut PgConnection, users: &[Uuid]) -> Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(s.k) FROM (
           SELECT DISTINCT hashtextextended('notify-recipient:' || u::text, 0) AS k
           FROM unnest($1::uuid[]) u ORDER BY 1) s",
    )
    .bind(users)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `[ids]` of a JSON array as a uuid[] / text[] in order (x.v is one
/// element of `jsonb_array_elements`).
const JSON_UUIDS: &str = "ARRAY(SELECT t.e::uuid FROM jsonb_array_elements_text(x.v->'e') WITH ORDINALITY t(e, o) ORDER BY t.o)";
const JSON_REASONS: &str = "ARRAY(SELECT t.e FROM jsonb_array_elements_text(x.v->'r') WITH ORDINALITY t(e, o) ORDER BY t.o)";
const JSON_EVENT_REASONS: &str = "ARRAY(SELECT t.e FROM jsonb_array_elements_text(x.v->'er') WITH ORDINALITY t(e, o) ORDER BY t.o)";

/// The union of comma-joined per-event reasons, sorted.
fn union_reasons<'a>(per_event: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let set: BTreeSet<&str> = per_event
        .into_iter()
        .flat_map(|r| r.split(',').filter(|x| !x.is_empty()))
        .collect();
    set.into_iter().map(String::from).collect()
}

/// Joins each recipient's events to their unread row of the issue, or
/// opens one (§3.11.2 step 4), for all recipients of the issue at once: the
/// unread rows are locked first, so an event never lands in a row that was
/// just marked read (it opens a new row instead).
async fn deliver(
    conn: &mut PgConnection,
    issue: &IssueRow,
    out: &BTreeMap<Uuid, Vec<(Uuid, String)>>,
) -> Result<()> {
    let mut left: Vec<Uuid> = out.keys().copied().collect();
    if left.is_empty() {
        return Ok(());
    }
    fault("notify.deliver").await?;
    lock_recipients(&mut *conn, &left).await?;
    while !left.is_empty() {
        let cur: Vec<(Uuid, Uuid, Vec<Uuid>, Vec<String>)> = sqlx::query_as(
            "SELECT recipient_id, id, event_ids, event_reasons FROM notifications
             WHERE issue_id = $1 AND recipient_id = ANY($2) AND read_at IS NULL AND kind = 'issue'
             ORDER BY recipient_id FOR UPDATE",
        )
        .bind(issue.id)
        .bind(&left)
        .fetch_all(&mut *conn)
        .await?;
        let mut joined: BTreeSet<Uuid> = BTreeSet::new();
        let mut updates: Vec<Json> = Vec::with_capacity(cur.len());
        for (user, id, mut have, mut per) in cur {
            for (e, r) in &out[&user] {
                if !have.contains(e) {
                    have.push(*e);
                    per.push(r.clone());
                }
            }
            let drop = have.len().saturating_sub(MAX_EVENTS);
            have.drain(..drop);
            per.drain(..drop);
            let rs = union_reasons(&per);
            updates.push(json!({"id": id, "e": have, "er": per, "r": rs}));
            joined.insert(user);
        }
        if !updates.is_empty() {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE notifications n SET event_ids = {JSON_UUIDS}, event_reasons = {JSON_EVENT_REASONS},
                        reasons = {JSON_REASONS},
                        project_id = $2, last_at = GREATEST(n.last_at, clock_timestamp())
                 FROM jsonb_array_elements($1) AS x(v)
                 WHERE n.id = (x.v->>'id')::uuid"
            )))
            .bind(Json::Array(updates))
            .bind(issue.project_id)
            .execute(&mut *conn)
            .await?;
        }
        let fresh: Vec<Json> = left
            .iter()
            .filter(|u| !joined.contains(u))
            .map(|u| {
                let evs = &out[u];
                let keep = &evs[evs.len().saturating_sub(MAX_EVENTS)..];
                let ids: Vec<Uuid> = keep.iter().map(|x| x.0).collect();
                let per: Vec<String> = keep.iter().map(|x| x.1.clone()).collect();
                let rs = union_reasons(&per);
                json!({"u": u, "e": ids, "er": per, "r": rs})
            })
            .collect();
        if fresh.is_empty() {
            break;
        }
        let want: Vec<Uuid> = left.into_iter().filter(|u| !joined.contains(u)).collect();
        let made: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "INSERT INTO notifications (recipient_id, issue_id, project_id, event_ids, event_reasons,
                                        reasons, first_at, last_at)
             SELECT (x.v->>'u')::uuid, $2, $3, {JSON_UUIDS}, {JSON_EVENT_REASONS}, {JSON_REASONS},
                    clock_timestamp(), clock_timestamp()
             FROM jsonb_array_elements($1) AS x(v)
             ON CONFLICT (recipient_id, issue_id) WHERE read_at IS NULL AND kind = 'issue'
             DO NOTHING RETURNING recipient_id"
        )))
        .bind(Json::Array(fresh))
        .bind(issue.id)
        .bind(issue.project_id)
        .fetch_all(&mut *conn)
        .await?;
        // Another transaction opened an unread row meanwhile: lock and join it.
        left = want.into_iter().filter(|u| !made.contains(u)).collect();
    }
    Ok(())
}

/// Tests: rounds until the cursor passed every `issue` outbox message
/// written so far (a round only sees messages below the cluster-wide
/// snapshot horizon, which another database's transaction can hold back).
#[cfg(feature = "test-support")]
pub async fn dispatch_all(pool: &PgPool) -> Result<Round> {
    let target: Option<(String, i64)> = sqlx::query_as(
        "SELECT xid::text, id FROM outbox WHERE topic = 'issue' ORDER BY xid DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let mut total = Round::default();
    for _ in 0..1000 {
        let r = dispatch_round(pool).await?;
        total.messages += r.messages;
        total.events += r.events;
        total.delivered += r.delivered;
        if r.more {
            continue;
        }
        let Some((xid, id)) = &target else {
            return Ok(total);
        };
        let done: bool = sqlx::query_scalar(
            "SELECT (last_xid, last_id) >= ($2::text::xid8, $3) FROM outbox_cursors WHERE consumer = $1",
        )
        .bind(CONSUMER)
        .bind(xid)
        .bind(id)
        .fetch_one(pool)
        .await?;
        if done {
            return Ok(total);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!(
        "{}",
        crate::metrics::horizon_timeout_message(pool, "notification dispatch").await
    )
}

/// Retention (B-25): read rows 30 days after reading, unread rows 90 days
/// after their last event. Returns the rows deleted.
pub async fn purge(pool: &PgPool) -> Result<u64> {
    let mut total = 0;
    loop {
        let n = sqlx::query(
            "DELETE FROM notifications WHERE id IN (
               SELECT id FROM notifications
               WHERE (read_at IS NOT NULL AND read_at < now() - make_interval(days => $1))
                  OR (read_at IS NULL AND last_at < now() - make_interval(days => $2))
               LIMIT 5000)",
        )
        .bind(READ_RETENTION_DAYS)
        .bind(UNREAD_RETENTION_DAYS)
        .execute(pool)
        .await?
        .rows_affected();
        total += n;
        if n < 5000 {
            return Ok(total);
        }
    }
}

// ---------------------------------------------------------------- reading

/// `GET /notifications` query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// `unread` (default) or `all`.
    pub filter: Option<String>,
    /// A project key or id; only rows of issues now in that project.
    pub project: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationList {
    /// Newest arrival first. Rows with nothing the caller can read now are
    /// left out without a trace.
    pub items: Vec<NotificationItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationItem {
    pub id: Uuid,
    /// `issue` or `ai_budget` (design 05 §3.13.4, site administrators only).
    pub kind: String,
    pub read: bool,
    /// watching, assigned, author, agent_on_behalf, triage_decided,
    /// mentioned (design 06 R-MN5); `ai_budget`.
    pub reasons: Vec<String>,
    /// When the first and the latest event of this row arrived.
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    /// Set for `kind = issue`, null for `ai_budget`.
    pub issue: Option<NotificationIssue>,
    /// Set for `kind = ai_budget`: the month's AI spending crossed 80% or 100%.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_budget: Option<AiBudgetNotice>,
    /// The newest readable events (at most 3), newest first.
    pub events: Vec<NotificationEvent>,
    /// Readable events of this row not in `events`.
    pub more_events: u32,
}

/// An `ai_budget` notification (§3.13.4): amounts are read now, not stored.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AiBudgetNotice {
    /// `YYYY-MM`.
    pub month: String,
    /// 80 or 100 (percent).
    pub threshold: i32,
    /// Spent this month so far, USD as a decimal string.
    pub spent_usd: String,
    /// The current monthly budget, USD as a decimal string (null when unset).
    pub budget_usd: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationIssue {
    pub key: String,
    pub subject: String,
    pub project: NotificationProject,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationProject {
    pub id: Uuid,
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationEvent {
    pub id: Uuid,
    /// created, fields_changed, status_changed, note_added, triage_changed, …
    pub kind: String,
    pub at: DateTime<Utc>,
    pub actor: NotificationActor,
    /// What the caller may see of the event, as in the issue history
    /// (`recent_events.detail`); a note adds `note_excerpt` (first 140
    /// characters; absent when the note was retracted or redacted),
    /// `private` and `redacted`. Absent when the event was redacted (D12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Json>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotificationActor {
    pub id: Uuid,
    pub name: String,
    /// user, agent or system. Show agents as "name〔agent〕".
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PersonRef>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PersonRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(sqlx::FromRow)]
struct NotifRow {
    id: Uuid,
    kind: String,
    issue_id: Option<Uuid>,
    event_ids: Vec<Uuid>,
    event_reasons: Vec<String>,
    payload: Option<Json>,
    first_at: DateTime<Utc>,
    last_at: DateTime<Utc>,
    read_at: Option<DateTime<Utc>>,
}

const NOTIF_SELECT: &str =
    "SELECT n.id, n.kind, n.issue_id, n.event_ids, n.event_reasons, n.payload,
        n.first_at, n.last_at, n.read_at
 FROM notifications n";

/// An `ai_budget` row: only an active site administrator reads it (it was
/// sent to the administrators of the time, §3.13.4).
async fn render_budget(
    conn: &mut PgConnection,
    rc: &Recipient,
    row: &NotifRow,
) -> Result<Option<NotificationItem>> {
    let admin: Option<bool> = sqlx::query_scalar(
        "SELECT is_admin FROM principals WHERE id = $1 AND kind = 'user' AND status = 'active'",
    )
    .bind(rc.ctx.principal_id())
    .fetch_optional(&mut *conn)
    .await?;
    if admin != Some(true) {
        return Ok(None);
    }
    let p = row.payload.clone().unwrap_or(Json::Null);
    let month = p["month"].as_str().unwrap_or_default().to_string();
    let threshold = p["threshold"].as_i64().unwrap_or(0) as i32;
    let (spent, budget): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT (SELECT spent_usd::text FROM ai_budget_months WHERE to_char(month, 'YYYY-MM') = $1),
                (SELECT monthly_budget_usd::text FROM ai_settings)",
    )
    .bind(&month)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(NotificationItem {
        id: row.id,
        kind: "ai_budget".into(),
        read: row.read_at.is_some(),
        reasons: vec!["ai_budget".into()],
        first_at: row.first_at,
        last_at: row.last_at,
        issue: None,
        ai_budget: Some(AiBudgetNotice {
            month,
            threshold,
            spent_usd: spent.unwrap_or_else(|| "0".into()),
            budget_usd: budget,
        }),
        events: Vec::new(),
        more_events: 0,
    }))
}

#[derive(sqlx::FromRow)]
struct ReadEvent {
    #[sqlx(flatten)]
    ev: DispatchEvent,
    on_behalf_of_name: Option<String>,
}

/// A row as `rc` may read it now, or `None` when nothing in it is
/// readable (issue gone from sight, every event hidden). With `full =
/// false` only the yes/no matters (unread count, marking).
async fn render(
    conn: &mut PgConnection,
    rc: &mut Recipient,
    row: &NotifRow,
    full: bool,
) -> Result<Option<NotificationItem>> {
    if row.kind == "ai_budget" {
        return render_budget(&mut *conn, rc, row).await;
    }
    let Some(issue_id) = row.issue_id else {
        return Ok(None);
    };
    let Some(issue) = issue_row(&mut *conn, issue_id).await? else {
        return Ok(None);
    };
    if !rc.sees_issue(&issue) {
        return Ok(None);
    }
    let events: Vec<ReadEvent> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT x.*, ob.name AS on_behalf_of_name FROM ({EVENT_SELECT}
            WHERE e.issue_id = $1 AND e.id = ANY($2)) x
         LEFT JOIN principals ob ON ob.id = x.on_behalf_of_id
         ORDER BY x.seq DESC"
    )))
    .bind(issue.id)
    .bind(&row.event_ids)
    .fetch_all(&mut *conn)
    .await?;
    // Design 06 R-MN5 (05 §3.11.3): a mention counts only while the
    // content still mentions the reader (a redaction deletes the mention,
    // a retracted note no longer shows its text); an event that was sent
    // only for such a mention is left out like an unreadable one.
    let reason_of = |id: Uuid| -> Option<&String> {
        row.event_ids
            .iter()
            .position(|e| *e == id)
            .and_then(|i| row.event_reasons.get(i))
    };
    let with_mention: Vec<Uuid> = row
        .event_ids
        .iter()
        .copied()
        .filter(|e| reason_of(*e).is_some_and(|r| r.split(',').any(|x| x == "mentioned")))
        .collect();
    let live = live_mentions(&mut *conn, &with_mention).await?;
    let me = rc.ctx.principal_id();
    let mut kept: Vec<String> = Vec::new();
    let mut shown: Vec<NotificationEvent> = Vec::new();
    let mut visible = 0u32;
    for re in events {
        let mut reason: Option<String> = reason_of(re.ev.id).cloned();
        if let Some(r) = &reason
            && r.split(',').any(|x| x == "mentioned")
            && !me.is_some_and(|me| live.get(&re.ev.id).is_some_and(|s| s.contains(&me)))
        {
            let rest: Vec<&str> = r.split(',').filter(|x| *x != "mentioned").collect();
            if rest.is_empty() {
                continue;
            }
            reason = Some(rest.join(","));
        }
        let Some(v) = rc.view(&mut *conn, &issue, re.ev.row()).await? else {
            continue;
        };
        visible += 1;
        if let Some(r) = reason {
            kept.push(r);
        }
        if !full {
            return Ok(Some(placeholder(row)));
        }
        if shown.len() >= ITEM_EVENTS {
            continue;
        }
        let mut summary = v.detail;
        if let (Some(s), Some(note), "note_added") = (&mut summary, re.ev.note_id, v.kind.as_str())
        {
            let n: Option<(String, String, bool, bool, bool)> = sqlx::query_as(
                "SELECT body, body_format, is_private, redacted_at IS NOT NULL, retracted_at IS NOT NULL
                 FROM issue_notes WHERE id = $1 AND issue_id = $2",
            )
            .bind(note)
            .bind(issue.id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some((body, format, private, redacted, retracted)) = n {
                if !redacted && !retracted {
                    // Design 06 R-M11, R-RF7: the text without Markdown
                    // syntax, references left as written.
                    let text = crate::markdown::plain_text(
                        &body,
                        crate::commands::TextFormat::from_db(&format),
                    );
                    s["note_excerpt"] = json!(text.chars().take(EXCERPT_CHARS).collect::<String>());
                }
                s["private"] = json!(private);
                s["redacted"] = json!(redacted);
                s["retracted"] = json!(retracted);
            }
        }
        shown.push(NotificationEvent {
            id: v.event_id,
            kind: v.kind,
            at: v.at,
            actor: NotificationActor {
                id: re.ev.actor_id,
                name: re.ev.actor.clone(),
                kind: re.ev.actor_kind.clone(),
                on_behalf_of: match (re.ev.actor_kind.as_str(), re.ev.on_behalf_of_id) {
                    ("agent", Some(id)) => Some(PersonRef {
                        id,
                        name: re.on_behalf_of_name.clone().unwrap_or_default(),
                    }),
                    _ => None,
                },
            },
            summary,
            redacted: v.redacted,
        });
    }
    if visible == 0 {
        return Ok(None);
    }
    let (key, subject, pkey, pname): (String, String, String, String) = sqlx::query_as(
        "SELECT k.key, i.subject, p.key, p.name FROM issues i
         JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         JOIN projects p ON p.id = i.project_id WHERE i.id = $1",
    )
    .bind(issue.id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(NotificationItem {
        id: row.id,
        kind: "issue".into(),
        read: row.read_at.is_some(),
        // Only the reasons of events the reader can still read.
        reasons: union_reasons(&kept),
        first_at: row.first_at,
        last_at: row.last_at,
        issue: Some(NotificationIssue {
            key,
            subject,
            project: NotificationProject {
                id: issue.project_id,
                key: pkey,
                name: pname,
            },
        }),
        ai_budget: None,
        more_events: visible - shown.len() as u32,
        events: shown,
    }))
}

/// "Something is readable" without building the item.
fn placeholder(row: &NotifRow) -> NotificationItem {
    NotificationItem {
        id: row.id,
        kind: "issue".into(),
        read: row.read_at.is_some(),
        reasons: Vec::new(),
        first_at: row.first_at,
        last_at: row.last_at,
        issue: None,
        ai_budget: None,
        events: Vec::new(),
        more_events: 0,
    }
}

/// The signed-in person and what they may read.
async fn me(conn: &mut PgConnection, caller: AuthContext) -> Result<(Uuid, Recipient)> {
    let user = commands::executor(&caller)?.user_id;
    let reader = Reader::new(&mut *conn, caller).await?;
    Ok((
        user,
        Recipient {
            ctx: caller,
            reader,
            cf: HashMap::new(),
        },
    ))
}

/// `GET /notifications` (§3.11.3).
pub async fn list(app: &App, caller: AuthContext, q: &ListQuery) -> Result<NotificationList> {
    let unread_only = match q.filter.as_deref().map(str::trim).unwrap_or("unread") {
        "unread" => true,
        "all" => false,
        _ => {
            return Err(
                ApiError::new(ErrorCode::InvalidArgument, "filter is unread or all")
                    .with_details(json!({"path": "filter", "allowed": ["unread", "all"]}))
                    .into(),
            );
        }
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let mut tx = read_tx(app).await?;
    let (user, mut rc) = me(&mut tx, caller).await?;
    // A project the caller cannot see filters to nothing, like a missing one.
    let project: Option<Uuid> = match q.project.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(p) => {
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM projects WHERE id::text = $1 OR key = upper($1)",
            )
            .bind(p)
            .fetch_optional(&mut *tx)
            .await?;
            match found.filter(|id| rc.reader.scope().project_ids().any(|v| v == *id)) {
                Some(id) => Some(id),
                None => {
                    tx.commit().await?;
                    return Ok(NotificationList {
                        items: Vec::new(),
                        next_cursor: None,
                    });
                }
            }
        }
    };
    let hash = cursor::query_hash(&json!({"notifications": unread_only, "project": project}));
    let mut after: Option<(DateTime<Utc>, Uuid)> = match q.cursor.as_deref() {
        None => None,
        Some(c) => match cursor::decode(&app.pool, c, Some(user), &hash).await? {
            Position::Keyset { t, i } => Some((t, i)),
            _ => return Err(cursor::mismatch()),
        },
    };
    let mut items: Vec<NotificationItem> = Vec::new();
    let mut scanned = 0usize;
    let mut next: Option<(DateTime<Utc>, Uuid)> = None;
    'scan: loop {
        let rows: Vec<NotifRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{NOTIF_SELECT} LEFT JOIN issues i ON i.id = n.issue_id
             WHERE n.recipient_id = $1
               AND ($2 = false OR n.read_at IS NULL)
               AND ($3::uuid IS NULL OR i.project_id = $3)
               AND ($4::timestamptz IS NULL OR (n.last_at, n.id) < ($4, $5))
             ORDER BY n.last_at DESC, n.id DESC LIMIT 100"
        )))
        .bind(user)
        .bind(unread_only)
        .bind(project)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1).unwrap_or_default())
        .fetch_all(&mut *tx)
        .await?;
        let n = rows.len();
        for row in rows {
            scanned += 1;
            let pos = (row.last_at, row.id);
            after = Some(pos);
            if let Some(item) = render(&mut tx, &mut rc, &row, true).await? {
                if items.len() == limit {
                    // One more readable row exists: continue after the last shown.
                    let last = items.last().map(|i| (i.last_at, i.id));
                    next = last;
                    break 'scan;
                }
                items.push(item);
            }
            if scanned >= SCAN_LIMIT {
                next = Some(pos);
                break 'scan;
            }
        }
        if n < 100 {
            break;
        }
    }
    tx.commit().await?;
    let next_cursor = match next {
        Some((t, i)) => {
            Some(cursor::encode(&app.pool, Some(user), &hash, Position::Keyset { t, i }).await?)
        }
        None => None,
    };
    Ok(NotificationList { items, next_cursor })
}

/// `GET /notifications/unread-count`.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
pub struct UnreadCount {
    /// Unread rows with something readable now, at most 99; `null` when it
    /// could not be counted in time and the caller's permissions changed
    /// since the last count (show no number).
    pub count: Option<u32>,
    /// More than 99: show "99+".
    pub capped: bool,
    /// Not counted within 2 seconds: the last count (or null).
    pub stale: bool,
}

/// (permission fingerprint, count, capped).
type LastCount = (String, u32, bool);
/// Last counts by person.
static COUNTS: Mutex<Option<HashMap<Uuid, LastCount>>> = Mutex::new(None);

/// Changes when the person's reach may have changed (appendix D V22): the
/// admin flag, groups, and the security epochs of the projects they see or
/// belong to (membership, roles, permissions and restricted fields all move
/// an epoch).
async fn fingerprint(conn: &mut PgConnection, caller: AuthContext) -> Result<String> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let mut projects: Vec<Uuid> = scope.project_ids().collect();
    let member: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT project_id FROM members WHERE principal_id = ANY($1)")
            .bind(&scope.principal_ids)
            .fetch_all(&mut *conn)
            .await?;
    projects.extend(member);
    projects.sort();
    projects.dedup();
    let epochs: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, security_epoch::bigint FROM projects WHERE id = ANY($1) ORDER BY id",
    )
    .bind(&projects)
    .fetch_all(&mut *conn)
    .await?;
    let admin = db::is_admin(&mut *conn, caller).await?;
    let digest = Sha256::digest(format!("{admin}|{:?}|{epochs:?}", scope.principal_ids));
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

async fn count_now(app: &App, caller: AuthContext) -> Result<(u32, bool)> {
    fault("notify.count").await?;
    let mut tx = read_tx(app).await?;
    let (user, mut rc) = me(&mut tx, caller).await?;
    let mut n = 0u32;
    let mut after: Option<(DateTime<Utc>, Uuid)> = None;
    'scan: loop {
        let rows: Vec<NotifRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{NOTIF_SELECT}
             WHERE n.recipient_id = $1 AND n.read_at IS NULL
               AND ($2::timestamptz IS NULL OR (n.last_at, n.id) < ($2, $3))
             ORDER BY n.last_at DESC, n.id DESC LIMIT 200"
        )))
        .bind(user)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1).unwrap_or_default())
        .fetch_all(&mut *tx)
        .await?;
        let len = rows.len();
        for row in rows {
            after = Some((row.last_at, row.id));
            if render(&mut tx, &mut rc, &row, false).await?.is_some() {
                n += 1;
                if n > UNREAD_CAP {
                    break 'scan;
                }
            }
        }
        if len < 200 {
            break;
        }
    }
    tx.commit().await?;
    Ok((n.min(UNREAD_CAP), n > UNREAD_CAP))
}

/// `GET /notifications/unread-count` (§3.11.3, appendix D V22).
pub async fn unread_count(app: &App, caller: AuthContext) -> Result<UnreadCount> {
    let user = commands::executor(&caller)?.user_id;
    let fp = {
        let mut conn = app.pool.acquire().await?;
        fingerprint(&mut conn, caller).await?
    };
    match tokio::time::timeout(COUNT_BUDGET, count_now(app, caller)).await {
        Ok(r) => {
            let (count, capped) = r?;
            if let Ok(mut g) = COUNTS.lock() {
                let m = g.get_or_insert_with(HashMap::new);
                if m.len() >= COUNTS_CAP && !m.contains_key(&user) {
                    m.clear();
                }
                m.insert(user, (fp, count, capped));
            }
            Ok(UnreadCount {
                count: Some(count),
                capped,
                stale: false,
            })
        }
        Err(_) => {
            let last = COUNTS
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|m| m.get(&user).cloned()));
            Ok(match last {
                Some((f, count, capped)) if f == fp => UnreadCount {
                    count: Some(count),
                    capped,
                    stale: true,
                },
                _ => UnreadCount {
                    count: None,
                    capped: false,
                    stale: true,
                },
            })
        }
    }
}

// ---------------------------------------------------------------- marking

/// `POST /notifications/{id}/read|unread`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Marked {
    /// The row now holding these events (after `unread` merged it into
    /// another unread row of the same issue, that row's id).
    pub id: Uuid,
    pub read: bool,
    /// `unread` joined this row to the issue's other unread row (the old id
    /// no longer exists).
    pub merged_into: bool,
}

/// `POST /notifications/read-all` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadAllInput {
    /// Mark rows whose `last_at` is at or before this (the first row's
    /// `last_at` when the list was loaded), so later arrivals stay unread.
    pub before: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadAllDone {
    pub before: DateTime<Utc>,
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such notification")
}

/// The caller's row `id`, locked, if it exists and has something readable
/// now (otherwise `not_found`, like another person's id).
async fn own_row(
    conn: &mut PgConnection,
    rc: &mut Recipient,
    user: Uuid,
    id: &str,
) -> Result<NotifRow> {
    let id: Uuid = id.trim().parse().map_err(|_| not_found())?;
    let row: Option<NotifRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{NOTIF_SELECT} WHERE n.id = $1 AND n.recipient_id = $2 FOR UPDATE"
    )))
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or_else(not_found)?;
    if render(&mut *conn, rc, &row, false).await?.is_none() {
        return Err(not_found());
    }
    Ok(row)
}

async fn write_tx(app: &App) -> Result<sqlx::Transaction<'static, Postgres>> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

fn busy(e: anyhow::Error) -> anyhow::Error {
    if commands::is_lock_timeout(&e) {
        return api(
            ErrorCode::TemporarilyUnavailable,
            "your notifications are busy; send the same request again",
        );
    }
    e
}

/// `POST /notifications/{id}/read` (naturally idempotent).
pub async fn mark_read(app: &App, caller: AuthContext, id: &str) -> Result<Marked> {
    let mut tx = write_tx(app).await?;
    let r = async {
        let (user, mut rc) = me(&mut tx, caller).await?;
        let row = own_row(&mut tx, &mut rc, user, id).await?;
        sqlx::query("UPDATE notifications SET read_at = COALESCE(read_at, now()) WHERE id = $1")
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
        Ok(Marked {
            id: row.id,
            read: true,
            merged_into: false,
        })
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

fn unique_violation(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(c.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505"))
    })
}

/// `POST /notifications/{id}/unread`: when the issue already has another
/// unread row, this row is merged into it (appendix D V8) instead of
/// breaking `notifications_one_unread`.
pub async fn mark_unread(app: &App, caller: AuthContext, id: &str) -> Result<Marked> {
    for attempt in 0.. {
        let mut tx = write_tx(app).await?;
        let r = mark_unread_in(&mut tx, caller, id).await;
        match r {
            Ok(m) => {
                tx.commit().await?;
                return Ok(m);
            }
            // A dispatch opened an unread row between our check and update.
            Err(e) if unique_violation(&e) && attempt < 3 => continue,
            Err(e) => return Err(busy(e)),
        }
    }
    unreachable!()
}

async fn mark_unread_in(conn: &mut PgConnection, caller: AuthContext, id: &str) -> Result<Marked> {
    let (user, mut rc) = me(&mut *conn, caller).await?;
    let a = own_row(&mut *conn, &mut rc, user, id).await?;
    if a.read_at.is_none() {
        return Ok(Marked {
            id: a.id,
            read: false,
            merged_into: false,
        });
    }
    let b: Option<NotifRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{NOTIF_SELECT} WHERE n.recipient_id = $1 AND n.issue_id = $2 AND n.read_at IS NULL
           AND n.kind = 'issue' AND n.id <> $3 FOR UPDATE"
    )))
    .bind(user)
    .bind(a.issue_id)
    .bind(a.id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(b) = b else {
        sqlx::query("UPDATE notifications SET read_at = NULL WHERE id = $1")
            .bind(a.id)
            .execute(&mut *conn)
            .await?;
        return Ok(Marked {
            id: a.id,
            read: false,
            merged_into: false,
        });
    };
    let mut per: HashMap<Uuid, String> = HashMap::new();
    for row in [&a, &b] {
        for (e, r) in row.event_ids.iter().zip(&row.event_reasons) {
            per.entry(*e).or_insert_with(|| r.clone());
        }
    }
    let all: Vec<Uuid> = per.keys().copied().collect();
    let mut ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND id = ANY($2) ORDER BY seq",
    )
    .bind(a.issue_id)
    .bind(&all)
    .fetch_all(&mut *conn)
    .await?;
    let drop = ids.len().saturating_sub(MAX_EVENTS);
    ids.drain(..drop);
    let event_reasons: Vec<String> = ids.iter().map(|e| per[e].clone()).collect();
    let reasons = union_reasons(&event_reasons);
    sqlx::query("DELETE FROM notifications WHERE id = $1")
        .bind(a.id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE notifications SET event_ids = $2, reasons = $3, event_reasons = $6,
                first_at = LEAST(first_at, $4), last_at = GREATEST(last_at, $5)
         WHERE id = $1",
    )
    .bind(b.id)
    .bind(&ids)
    .bind(&reasons)
    .bind(a.first_at)
    .bind(a.last_at)
    .bind(&event_reasons)
    .execute(&mut *conn)
    .await?;
    Ok(Marked {
        id: b.id,
        read: false,
        merged_into: true,
    })
}

/// `POST /notifications/read-all {before}`.
pub async fn read_all(app: &App, caller: AuthContext, input: &ReadAllInput) -> Result<ReadAllDone> {
    let user = commands::executor(&caller)?.user_id;
    let mut tx = write_tx(app).await?;
    db::effective(&mut tx, caller).await?;
    // Waits for dispatches writing to this person's rows (lock_recipients).
    lock_recipients(&mut tx, &[user]).await.map_err(busy)?;
    sqlx::query(
        "UPDATE notifications SET read_at = now()
         WHERE recipient_id = $1 AND read_at IS NULL AND last_at <= $2",
    )
    .bind(user)
    .bind(input.before)
    .execute(&mut *tx)
    .await
    .map_err(|e| busy(e.into()))?;
    tx.commit().await?;
    Ok(ReadAllDone {
        before: input.before,
    })
}

// ---------------------------------------------------------------- watching

/// `PUT|DELETE /issues/{key}/watch`.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct WatchState {
    pub watching: bool,
    /// Active users who can see the issue now and watch it (at most 100).
    pub watcher_count: u32,
    /// More than 100: show "100+".
    pub watcher_count_capped: bool,
}

/// `GET /issues/{key}` → `watch` (people only; agents never watch).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WatchInfo {
    pub watching: bool,
    /// The caller unwatched this issue: being assigned, writing a note
    /// and so on will not watch it again.
    pub opted_out: bool,
    pub watcher_count: u32,
    pub watcher_count_capped: bool,
    /// Why the caller watches automatically: author, assignee or note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_reason: Option<String>,
}

/// One role's grant (`r` roles, `rp` its view_issues row) covers issue `i`
/// for user `up` (`up.id`, `up.pids` = the user and their groups): the
/// tracker restriction and the issues_visibility mode, as ox-core
/// `Scope::role_clause`.
const ROLE_COVERS: &str = "(rp.all_trackers OR i.tracker_id IN (
        SELECT t.tracker_id FROM role_permission_trackers t
        WHERE t.role_id = r.id AND t.permission = rp.permission))
   AND (r.issues_visibility = 'all'
        OR (r.issues_visibility = 'default'
            AND (NOT i.is_private OR i.author_id = up.id OR i.assigned_to_id = ANY(up.pids)))
        OR (r.issues_visibility = 'own'
            AND (i.author_id = up.id OR i.assigned_to_id = ANY(up.pids))))";

/// Watchers who are active users and can see the issue now, counted up
/// to [`WATCHER_CAP`] (B-46, appendix D V22), in one statement: the same
/// rules as `db::load_scope` + `Scope::can_see` for a person (admin, member
/// roles of the user and their groups, the non-member role in public
/// projects, eligible projects only), written in SQL for one issue.
async fn watcher_count(conn: &mut PgConnection, issue: &IssueRow) -> Result<(u32, bool)> {
    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "WITH i AS (SELECT * FROM issues WHERE id = $1),
         eligible AS (SELECT p.id, p.is_public FROM projects p, i
                      JOIN enabled_modules m ON m.project_id = i.project_id AND m.module = $2
                      WHERE p.id = i.project_id AND p.status <> 'archived'),
         up AS (SELECT w.user_id AS id, p.is_admin,
                       array_append(ARRAY(SELECT g.group_id FROM group_members g
                                          WHERE g.user_id = w.user_id), w.user_id) AS pids
                FROM issue_watchers w
                JOIN principals p ON p.id = w.user_id AND p.kind = 'user' AND p.status = 'active'
                WHERE w.issue_id = $1
                ORDER BY w.created_at, w.user_id LIMIT $4)
         SELECT count(*) FROM (
           SELECT 1 FROM up, i
           WHERE EXISTS (SELECT 1 FROM eligible)
             AND (up.is_admin
                  OR EXISTS (SELECT 1 FROM members m
                             JOIN member_roles mr ON mr.member_id = m.id
                             JOIN roles r ON r.id = mr.role_id
                             JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = $3
                             WHERE m.project_id = i.project_id AND m.principal_id = ANY(up.pids)
                               AND {ROLE_COVERS})
                  OR (EXISTS (SELECT 1 FROM eligible WHERE is_public)
                      AND NOT EXISTS (SELECT 1 FROM members m
                                      WHERE m.project_id = i.project_id AND m.principal_id = ANY(up.pids))
                      AND EXISTS (SELECT 1 FROM roles r
                                  JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = $3
                                  WHERE r.builtin = 'non_member' AND {ROLE_COVERS})))
           LIMIT $5) c"
    )))
    .bind(issue.id)
    .bind(db::ISSUE_MODULE)
    .bind(db::VIEW_ISSUES)
    .bind(WATCHER_SCAN)
    .bind(i64::from(WATCHER_CAP) + 1)
    .fetch_one(&mut *conn)
    .await?;
    let n = n as u32;
    Ok((n.min(WATCHER_CAP), n > WATCHER_CAP))
}

/// Tests: [`watcher_count`] next to the same count through
/// `db::load_scope` for each watcher (they must agree).
#[cfg(feature = "test-support")]
pub async fn watcher_count_both(pool: &PgPool, issue_id: Uuid) -> Result<(u32, u32)> {
    let mut conn = pool.acquire().await?;
    let issue = issue_row(&mut conn, issue_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no issue"))?;
    let (sql, _) = watcher_count(&mut conn, &issue).await?;
    let users: Vec<Uuid> = sqlx::query_scalar(
        "SELECT w.user_id FROM issue_watchers w
         JOIN principals p ON p.id = w.user_id AND p.kind = 'user' AND p.status = 'active'
         WHERE w.issue_id = $1",
    )
    .bind(issue_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut n = 0u32;
    for u in users {
        let scope = db::load_scope(&mut conn, AuthContext::notification_recipient(u)).await?;
        if !issue.archived && scope.can_see(&issue.facts()) {
            n += 1;
        }
    }
    Ok((sql, n.min(WATCHER_CAP)))
}

/// The `watch` block of the issue detail for person `me`.
pub(crate) async fn watch_info(
    conn: &mut PgConnection,
    me: Uuid,
    issue_id: Uuid,
) -> Result<WatchInfo> {
    let Some(issue) = issue_row(&mut *conn, issue_id).await? else {
        return Err(api(ErrorCode::NotFound, "no such issue"));
    };
    let source: Option<String> = sqlx::query_scalar(
        "SELECT source FROM issue_watchers WHERE issue_id = $1 AND user_id = $2",
    )
    .bind(issue_id)
    .bind(me)
    .fetch_optional(&mut *conn)
    .await?;
    let opted_out: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM issue_watch_optouts WHERE issue_id = $1 AND user_id = $2)",
    )
    .bind(issue_id)
    .bind(me)
    .fetch_one(&mut *conn)
    .await?;
    let (watcher_count, watcher_count_capped) = watcher_count(&mut *conn, &issue).await?;
    Ok(WatchInfo {
        watching: source.is_some(),
        opted_out,
        watcher_count,
        watcher_count_capped,
        auto_reason: source.and_then(|s| s.strip_prefix("auto_").map(String::from)),
    })
}

/// `PUT /issues/{key}/watch` (`on`) and `DELETE` (§3.11.1): yourself only,
/// visible issues only (an invisible issue is `not_found`, exactly like a
/// missing one). Naturally idempotent (no Idempotency-Key).
pub async fn watch(app: &App, caller: AuthContext, key: &str, on: bool) -> Result<WatchState> {
    let user = commands::executor(&caller)?.user_id;
    let mut tx = write_tx(app).await?;
    let issue = visible_issue(&mut tx, caller, key).await?;
    let r = async {
        lock_watch(&mut tx, issue.id, user).await?;
        if on {
            sqlx::query(
                "INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ($1, $2, 'manual')
                 ON CONFLICT (issue_id, user_id) DO UPDATE SET source = 'manual'",
            )
            .bind(issue.id)
            .bind(user)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM issue_watch_optouts WHERE issue_id = $1 AND user_id = $2")
                .bind(issue.id)
                .bind(user)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM issue_watchers WHERE issue_id = $1 AND user_id = $2")
                .bind(issue.id)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO issue_watch_optouts (issue_id, user_id) VALUES ($1, $2)
                 ON CONFLICT (issue_id, user_id) DO NOTHING",
            )
            .bind(issue.id)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        }
        anyhow::Ok(())
    }
    .await;
    r.map_err(busy)?;
    let (watcher_count, watcher_count_capped) = watcher_count(&mut tx, &issue).await?;
    tx.commit().await?;
    Ok(WatchState {
        watching: on,
        watcher_count,
        watcher_count_capped,
    })
}

// ---------------------------------------------------------------- preferences

/// `GET /me/notification-preferences` (§3.11.4).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct NotificationPreferences {
    pub lock_version: i32,
    /// Notify me about: issues I watch,
    pub on_watching: bool,
    /// issues assigned to me (or to my group),
    pub on_assigned: bool,
    /// issues I created,
    pub on_author: bool,
    /// the Triage outcome of issues I reported,
    pub on_triage_decided: bool,
    /// what my agents did,
    pub on_my_agents: bool,
    /// and my own changes (off by default).
    pub on_self: bool,
    /// Someone @mentioned me in a description or note I can read (design 06
    /// R-MN5; on by default). Mentions do not make me watch the issue.
    pub on_mentioned: bool,
    /// Watch automatically what I create, what is assigned to me, where I
    /// write a note.
    pub auto_watch_author: bool,
    pub auto_watch_assignee: bool,
    pub auto_watch_note: bool,
    /// Projects (ids) that send me nothing; only projects I can see.
    pub muted_projects: Vec<Uuid>,
}

/// `PUT /me/notification-preferences`: fields left out stay as they are.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreferencesPut {
    pub lock_version: i32,
    #[serde(default)]
    pub on_watching: Option<bool>,
    #[serde(default)]
    pub on_assigned: Option<bool>,
    #[serde(default)]
    pub on_author: Option<bool>,
    #[serde(default)]
    pub on_triage_decided: Option<bool>,
    #[serde(default)]
    pub on_my_agents: Option<bool>,
    #[serde(default)]
    pub on_self: Option<bool>,
    /// Left out of the request fingerprint when absent, so a request sent
    /// before M2.1 replays the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_mentioned: Option<bool>,
    #[serde(default)]
    pub auto_watch_author: Option<bool>,
    #[serde(default)]
    pub auto_watch_assignee: Option<bool>,
    #[serde(default)]
    pub auto_watch_note: Option<bool>,
    /// The whole list of project ids to mute (at most 200, each visible to
    /// you; otherwise `invalid_value` kind `unknown_project`).
    #[serde(default)]
    pub muted_projects: Option<Vec<String>>,
}

#[derive(sqlx::FromRow)]
struct PrefRow {
    lock_version: i32,
    on_watching: bool,
    on_assigned: bool,
    on_author: bool,
    on_triage_decided: bool,
    on_my_agents: bool,
    on_self: bool,
    on_mentioned: bool,
    auto_watch_author: bool,
    auto_watch_assignee: bool,
    auto_watch_note: bool,
}

async fn ensure_prefs(conn: &mut PgConnection, user: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO notification_preferences (user_id) VALUES ($1) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn load_prefs(conn: &mut PgConnection, user: Uuid, lock: bool) -> Result<PrefRow> {
    let sql = format!(
        "SELECT lock_version, on_watching, on_assigned, on_author, on_triage_decided, on_my_agents,
                on_self, on_mentioned, auto_watch_author, auto_watch_assignee, auto_watch_note
         FROM notification_preferences WHERE user_id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .fetch_one(&mut *conn)
        .await?)
}

/// Muted projects the caller can see now (the others stay stored, unseen).
async fn visible_mutes(
    conn: &mut PgConnection,
    caller: AuthContext,
    user: Uuid,
) -> Result<Vec<Uuid>> {
    let scope = db::load_scope(&mut *conn, caller).await?;
    let visible: BTreeSet<Uuid> = scope.project_ids().collect();
    let all: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM notification_project_mutes WHERE user_id = $1 ORDER BY project_id",
    )
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    Ok(all.into_iter().filter(|p| visible.contains(p)).collect())
}

async fn render_prefs(
    conn: &mut PgConnection,
    caller: AuthContext,
    user: Uuid,
) -> Result<NotificationPreferences> {
    let r = load_prefs(&mut *conn, user, false).await?;
    Ok(NotificationPreferences {
        lock_version: r.lock_version,
        on_watching: r.on_watching,
        on_assigned: r.on_assigned,
        on_author: r.on_author,
        on_triage_decided: r.on_triage_decided,
        on_my_agents: r.on_my_agents,
        on_self: r.on_self,
        on_mentioned: r.on_mentioned,
        auto_watch_author: r.auto_watch_author,
        auto_watch_assignee: r.auto_watch_assignee,
        auto_watch_note: r.auto_watch_note,
        muted_projects: visible_mutes(&mut *conn, caller, user).await?,
    })
}

/// `GET /me/notification-preferences`: the first read creates the row
/// with the defaults (not a user write: no Idempotency-Key).
pub async fn preferences(app: &App, caller: AuthContext) -> Result<NotificationPreferences> {
    let user = commands::executor(&caller)?.user_id;
    let mut tx = app.pool.begin().await?;
    ensure_prefs(&mut tx, user).await?;
    tx.commit().await?;
    let mut tx = read_tx(app).await?;
    let v = render_prefs(&mut tx, caller, user).await?;
    tx.commit().await?;
    Ok(v)
}

fn invalid(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

/// `PUT /me/notification-preferences` (Idempotency-Key, `lock_version`).
pub async fn put_preferences(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &PreferencesPut,
) -> Result<NotificationPreferences> {
    let mut tx = write_tx(app).await?;
    let r = async {
        let who = commands::executor(&caller)?;
        let user = who.user_id;
        let hash: [u8; 32] = Sha256::digest(serde_json::to_vec(input)?).into();
        let fp = commands::fingerprint(ACTION_PREFS, &hash);
        let Some(request_id) =
            commands::open_request(&mut tx, user, operation_id, ACTION_PREFS, &fp).await?
        else {
            // The same request again: the current state.
            commands::stored_result(&mut tx, user, operation_id, ACTION_PREFS, &fp).await?;
            ensure_prefs(&mut tx, user).await?;
            return render_prefs(&mut tx, caller, user).await;
        };
        commands::lock_executor(&mut tx, &who).await?;
        ensure_prefs(&mut tx, user).await?;
        let cur = load_prefs(&mut tx, user, true).await?;
        if input.lock_version != cur.lock_version {
            let now = render_prefs(&mut tx, caller, user).await?;
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "the notification settings changed elsewhere; here is the current version",
            )
            .with_details(json!({"current_lock_version": cur.lock_version,
                                 "current": serde_json::to_value(now)?}))
            .into());
        }
        let pick = |v: Option<bool>, old: bool| v.unwrap_or(old);
        let next = PrefRow {
            lock_version: cur.lock_version,
            on_watching: pick(input.on_watching, cur.on_watching),
            on_assigned: pick(input.on_assigned, cur.on_assigned),
            on_author: pick(input.on_author, cur.on_author),
            on_triage_decided: pick(input.on_triage_decided, cur.on_triage_decided),
            on_my_agents: pick(input.on_my_agents, cur.on_my_agents),
            on_self: pick(input.on_self, cur.on_self),
            on_mentioned: pick(input.on_mentioned, cur.on_mentioned),
            auto_watch_author: pick(input.auto_watch_author, cur.auto_watch_author),
            auto_watch_assignee: pick(input.auto_watch_assignee, cur.auto_watch_assignee),
            auto_watch_note: pick(input.auto_watch_note, cur.auto_watch_note),
        };
        let flags = |r: &PrefRow| {
            [
                r.on_watching,
                r.on_assigned,
                r.on_author,
                r.on_triage_decided,
                r.on_my_agents,
                r.on_self,
                r.on_mentioned,
                r.auto_watch_author,
                r.auto_watch_assignee,
                r.auto_watch_note,
            ]
        };
        let mut changed = flags(&next) != flags(&cur);
        if let Some(list) = &input.muted_projects {
            if list.len() > MAX_MUTES {
                return Err(invalid(
                    "muted_projects",
                    "too_many_projects",
                    format!("at most {MAX_MUTES} muted projects"),
                ));
            }
            let scope = db::load_scope(&mut tx, caller).await?;
            let visible: BTreeSet<Uuid> = scope.project_ids().collect();
            let mut want: BTreeSet<Uuid> = BTreeSet::new();
            for (i, p) in list.iter().enumerate() {
                // Missing and not visible are one answer.
                match p.trim().parse::<Uuid>() {
                    Ok(id) if visible.contains(&id) => {
                        want.insert(id);
                    }
                    _ => {
                        return Err(invalid(
                            &format!("muted_projects[{i}]"),
                            "unknown_project",
                            "no such project",
                        ));
                    }
                }
            }
            let have: BTreeSet<Uuid> = visible_mutes(&mut tx, caller, user)
                .await?
                .into_iter()
                .collect();
            if want != have {
                changed = true;
                let gone: Vec<Uuid> = have.difference(&want).copied().collect();
                let new: Vec<Uuid> = want.difference(&have).copied().collect();
                sqlx::query(
                    "DELETE FROM notification_project_mutes WHERE user_id = $1 AND project_id = ANY($2)",
                )
                .bind(user)
                .bind(&gone)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO notification_project_mutes (user_id, project_id)
                     SELECT $1, unnest($2::uuid[]) ON CONFLICT DO NOTHING",
                )
                .bind(user)
                .bind(&new)
                .execute(&mut *tx)
                .await?;
            }
        }
        if changed {
            sqlx::query(
                "UPDATE notification_preferences SET on_watching = $2, on_assigned = $3,
                        on_author = $4, on_triage_decided = $5, on_my_agents = $6, on_self = $7,
                        auto_watch_author = $8, auto_watch_assignee = $9, auto_watch_note = $10,
                        on_mentioned = $11,
                        lock_version = lock_version + 1, updated_at = now()
                 WHERE user_id = $1",
            )
            .bind(user)
            .bind(next.on_watching)
            .bind(next.on_assigned)
            .bind(next.on_author)
            .bind(next.on_triage_decided)
            .bind(next.on_my_agents)
            .bind(next.on_self)
            .bind(next.auto_watch_author)
            .bind(next.auto_watch_assignee)
            .bind(next.auto_watch_note)
            .bind(next.on_mentioned)
            .execute(&mut *tx)
            .await?;
        }
        commands::complete_request(&mut tx, request_id, &json!({})).await?;
        render_prefs(&mut tx, caller, user).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}
