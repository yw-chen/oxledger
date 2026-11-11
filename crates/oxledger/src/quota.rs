// SPDX-License-Identifier: GPL-2.0-or-later
//! Agent quotas and the circuit breaker (contract 02 §8.9, §16.6).
//!
//! Three tiers, never mixed (§16.6):
//! - a short-term rate (per minute, one write per context at a time) →
//!   `rate_limited`, `wait_and_retry`;
//! - an hourly quota or the open-proposal cap → `quota_exceeded`, ask a person;
//! - the incident breaker (≥3 `audience_expansion_forbidden` in an hour, or
//!   more agent writes to one issue in 10 minutes than allowed) → the
//!   context is suspended in its own committed transaction and the request
//!   gets `agent_suspended` (`details.scope = "context"`). Running out of a
//!   normal quota never suspends.
//!
//! Counting is over `agent_write_log`, agent-wide (across tokens, delegations
//! and contexts). Replays of an idempotency key return before any counting.

use crate::error::{ApiError, ErrorCode};
use anyhow::Result;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Every `agent_write_log.kind` (the union rule of design 05 §2.1; T-M1-124
/// compares it with the CHECK constraint). `time_taint_review` is written
/// by the built-in AI (M1.10).
pub const WRITE_LOG_KINDS: [&str; 11] = [
    "direct_write",
    "issue_create",
    "issue_write",
    "propose",
    "audience_blocked",
    "version_create",
    "version_write",
    "validate",
    "time_write",
    "chain_review",
    "time_taint_review",
];

/// Initial values (deployment may lower them; a delegation only narrows).
pub mod limits {
    /// §16.6: successful direct writes (documents) per agent.
    pub const DIRECT_PER_MINUTE: i64 = 10;
    pub const DIRECT_PER_HOUR: i64 = 100;
    /// §16.6: agent writes to one issue per 10 minutes (more trips the breaker).
    pub const ISSUE_WRITES_PER_10_MIN: i64 = 10;
    /// §16.6: issues an agent creates per hour.
    pub const CREATES_PER_HOUR: i64 = 20;
    /// §16.6 (replaces §8.9's per-delegation cap): open proposals per agent.
    pub const OPEN_PROPOSALS: i64 = 100;
    /// §8.9: propose + revise per delegation per hour.
    pub const PROPOSALS_PER_HOUR: i64 = 60;
    /// §16.6: audience refusals per hour that trip the breaker.
    pub const AUDIENCE_BLOCKS_PER_HOUR: i64 = 3;
    /// §17.9a: versions an agent creates per hour.
    pub const VERSION_CREATES_PER_HOUR: i64 = 10;
    /// §17.9a: agent writes to one version per 10 minutes (more trips the breaker).
    pub const VERSION_WRITES_PER_10_MIN: i64 = 5;
    /// Contract 05 §4.3: time entries an agent logs per hour.
    pub const LOG_TIME_PER_HOUR: i64 = 20;
    /// §8.9: validate per delegation per hour.
    pub const VALIDATES_PER_HOUR: i64 = 120;
    /// §8.9: proposal writes of one delegation running at the same time.
    pub const CONCURRENT_PROPOSAL_WRITES: i32 = 2;
    /// §8.9: the proposal document as sent (serialized JSON bytes).
    pub const RAW_PAYLOAD_BYTES: usize = 768 * 1024;
    /// §8.9: the normalized command document (canonical JSON bytes).
    pub const NORMALIZED_BYTES: usize = 512 * 1024;
}

/// Contract 05 §4.3: at most [`limits::LOG_TIME_PER_HOUR`] entries an hour
/// (checked after [`reserve_direct`], which holds the agent row).
pub async fn reserve_time(conn: &mut PgConnection, agent: Uuid, logs: i64) -> Result<()> {
    if logs > 0
        && count(conn, BY_AGENT, agent, "time_write", 3600).await? + logs
            > limits::LOG_TIME_PER_HOUR
    {
        return Err(quota_exceeded(
            "log_time_per_hour",
            limits::LOG_TIME_PER_HOUR,
        ));
    }
    Ok(())
}

/// Counts `logs` new time entries for [`reserve_time`].
pub async fn record_time(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
    logs: i64,
) -> Result<()> {
    for _ in 0..logs {
        sqlx::query(
            "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
             VALUES ($1, $2, $3, 'time_write')",
        )
        .bind(agent)
        .bind(context)
        .bind(delegation)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A version document's part in the counters (§17.9a).
#[derive(Debug, Clone, Copy, Default)]
pub struct VersionWrite {
    pub create: bool,
    /// The existing version updated.
    pub update: Option<Uuid>,
}

/// The breaker has to trip: committed by [`trip`] outside the request's
/// transaction, then reported as `agent_suspended`.
#[derive(Debug)]
pub struct TripBreaker {
    pub context: Uuid,
    pub reason: &'static str,
}

impl std::fmt::Display for TripBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "circuit breaker: {}", self.reason)
    }
}

impl std::error::Error for TripBreaker {}

pub fn suspended(reason: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::AgentSuspended,
        "writes by this agent are paused; the delegator can resume them in the web app",
    )
    .with_details(json!({"scope": "context", "reason": reason}))
    .into()
}

fn rate_limited(kind: &str, retry_after_ms: u64) -> anyhow::Error {
    ApiError::new(
        ErrorCode::RateLimited,
        "too many writes right now; wait and send the same request again",
    )
    .with_details(json!({"kind": kind, "retry_after_ms": retry_after_ms}))
    .with_retry_after(retry_after_ms)
    .into()
}

fn quota_exceeded(kind: &str, limit: i64) -> anyhow::Error {
    ApiError::new(
        ErrorCode::QuotaExceeded,
        "this agent used up its quota for now; ask the delegator",
    )
    .with_details(json!({"kind": kind, "limit": limit}))
    .into()
}

/// Rejects when the context is suspended. Read without a lock: writes are
/// refused from the commit of the suspension on.
pub async fn check_not_suspended(conn: &mut PgConnection, context: Uuid) -> Result<()> {
    let r: Option<Option<String>> = sqlx::query_scalar(
        "SELECT suspension_reason FROM agent_context_write_controls WHERE context_id = $1",
    )
    .bind(context)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(Some(reason)) = r {
        return Err(suspended(&reason));
    }
    Ok(())
}

async fn count(conn: &mut PgConnection, sql: &str, id: Uuid, kind: &str, secs: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(id)
        .bind(kind)
        .bind(secs as f64)
        .fetch_one(&mut *conn)
        .await?)
}

const BY_AGENT: &str = "SELECT count(*) FROM agent_write_log
     WHERE agent_principal_id = $1 AND kind = $2 AND at > clock_timestamp() - make_interval(secs => $3)";
const BY_DELEGATION: &str = "SELECT count(*) FROM agent_write_log
     WHERE delegation_id = $1 AND kind = $2 AND at > clock_timestamp() - make_interval(secs => $3)";

/// The direct-write gate (§16.5: after the domain locks' projects, see
/// decisions.tsv): the context's control row FOR UPDATE NOWAIT (one write at
/// a time), then the agent row FOR UPDATE (agent-wide counts are exact),
/// then the counters. `issues` are the existing issues written; `creates`
/// the number of new issues.
pub async fn reserve_direct(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    issues: &[Uuid],
    creates: i64,
    version: VersionWrite,
) -> Result<()> {
    let row: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT suspension_reason FROM agent_context_write_controls WHERE context_id = $1 FOR UPDATE NOWAIT",
    )
    .bind(context)
    .fetch_one(&mut *conn)
    .await;
    let reason = match row {
        Ok(r) => r,
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("55P03") => {
            return Err(rate_limited("concurrent_write", 1000));
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(r) = reason {
        return Err(suspended(&r));
    }
    sqlx::query("SELECT 1 FROM agent_principals WHERE agent_principal_id = $1 FOR UPDATE")
        .bind(agent)
        .execute(&mut *conn)
        .await?;
    if count(conn, BY_AGENT, agent, "direct_write", 60).await? >= limits::DIRECT_PER_MINUTE {
        return Err(rate_limited("per_minute", 60_000));
    }
    if count(conn, BY_AGENT, agent, "direct_write", 3600).await? >= limits::DIRECT_PER_HOUR {
        return Err(quota_exceeded(
            "direct_writes_per_hour",
            limits::DIRECT_PER_HOUR,
        ));
    }
    if creates > 0
        && count(conn, BY_AGENT, agent, "issue_create", 3600).await? + creates
            > limits::CREATES_PER_HOUR
    {
        return Err(quota_exceeded(
            "issue_creates_per_hour",
            limits::CREATES_PER_HOUR,
        ));
    }
    if version.create
        && count(conn, BY_AGENT, agent, "version_create", 3600).await?
            >= limits::VERSION_CREATES_PER_HOUR
    {
        return Err(quota_exceeded(
            "version_creates_per_hour",
            limits::VERSION_CREATES_PER_HOUR,
        ));
    }
    if let Some(v) = version.update {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_write_log
             WHERE version_id = $1 AND kind = 'version_write' AND agent_principal_id = $2
               AND at > clock_timestamp() - interval '10 minutes'",
        )
        .bind(v)
        .bind(agent)
        .fetch_one(&mut *conn)
        .await?;
        if n >= limits::VERSION_WRITES_PER_10_MIN {
            return Err(TripBreaker {
                context,
                reason: "version_write_burst",
            }
            .into());
        }
    }
    for i in issues {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_write_log
             WHERE issue_id = $1 AND kind = 'issue_write' AND agent_principal_id = $2
               AND at > clock_timestamp() - interval '10 minutes'",
        )
        .bind(i)
        .bind(agent)
        .fetch_one(&mut *conn)
        .await?;
        if n >= limits::ISSUE_WRITES_PER_10_MIN {
            return Err(TripBreaker {
                context,
                reason: "issue_write_burst",
            }
            .into());
        }
    }
    Ok(())
}

/// After a successful direct write, in its transaction.
pub async fn record_direct(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
    written: &[Uuid],
    created: &[Uuid],
    version: Option<(Uuid, bool)>,
) -> Result<()> {
    let ins = "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind, issue_id)
               VALUES ($1, $2, $3, $4, $5)";
    sqlx::query(ins)
        .bind(agent)
        .bind(context)
        .bind(delegation)
        .bind("direct_write")
        .bind(None::<Uuid>)
        .execute(&mut *conn)
        .await?;
    for i in created {
        sqlx::query(ins)
            .bind(agent)
            .bind(context)
            .bind(delegation)
            .bind("issue_create")
            .bind(i)
            .execute(&mut *conn)
            .await?;
    }
    if let Some((v, created)) = version {
        sqlx::query(
            "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind, version_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(agent)
        .bind(context)
        .bind(delegation)
        .bind(if created { "version_create" } else { "version_write" })
        .bind(v)
        .execute(&mut *conn)
        .await?;
    }
    for i in written {
        sqlx::query(ins)
            .bind(agent)
            .bind(context)
            .bind(delegation)
            .bind("issue_write")
            .bind(i)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Proposals (§8.9 per delegation, §16.6 open per agent), before storing.
pub async fn reserve_proposal(
    conn: &mut PgConnection,
    agent: Uuid,
    delegation: Uuid,
    new: bool,
) -> Result<()> {
    sqlx::query("SELECT 1 FROM agent_principals WHERE agent_principal_id = $1 FOR UPDATE")
        .bind(agent)
        .execute(&mut *conn)
        .await?;
    if count(conn, BY_DELEGATION, delegation, "propose", 3600).await? >= limits::PROPOSALS_PER_HOUR
    {
        return Err(quota_exceeded(
            "proposals_per_hour",
            limits::PROPOSALS_PER_HOUR,
        ));
    }
    if new {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM changesets
             WHERE proposer_id = $1 AND state IN ('proposed', 'approved', 'executing')",
        )
        .bind(agent)
        .fetch_one(&mut *conn)
        .await?;
        if open >= limits::OPEN_PROPOSALS {
            return Err(quota_exceeded("open_proposals", limits::OPEN_PROPOSALS));
        }
    }
    Ok(())
}

pub async fn record_proposal(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
         VALUES ($1, $2, $3, 'propose')",
    )
    .bind(agent)
    .bind(context)
    .bind(delegation)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Design 05 §3.12.4: a `direct` document the chain limit sent for review.
/// An audit row only; no counter reads this kind.
pub async fn log_chain_review(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
         VALUES ($1, $2, $3, 'chain_review')",
    )
    .bind(agent)
    .bind(context)
    .bind(delegation)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Records a built-in AI write turned into a review because its run read
/// the user's time (design 05 §3.13.3, Q3).
pub async fn log_time_taint_review(
    conn: &mut PgConnection,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
         VALUES ($1, $2, $3, 'time_taint_review')",
    )
    .bind(agent)
    .bind(context)
    .bind(delegation)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Suspends `context` in its own transaction (the refused request rolled
/// back; the suspension must not).
pub async fn trip(pool: &PgPool, context: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_context_write_controls (context_id, suspended_at, suspension_reason)
         VALUES ($1, clock_timestamp(), $2)
         ON CONFLICT (context_id) DO UPDATE
           SET suspended_at = COALESCE(agent_context_write_controls.suspended_at, EXCLUDED.suspended_at),
               suspension_reason = COALESCE(agent_context_write_controls.suspension_reason, EXCLUDED.suspension_reason)",
    )
    .bind(context)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// After a request was refused with `audience_expansion_forbidden`: logs it
/// in its own transaction; at the threshold the context is suspended and
/// the caller reports `agent_suspended` instead (§16.6).
pub async fn audience_blocked(pool: &PgPool, agent: Uuid, context: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, kind) VALUES ($1, $2, 'audience_blocked')",
    )
    .bind(agent)
    .bind(context)
    .execute(&mut *tx)
    .await?;
    let n = count(&mut tx, BY_AGENT, agent, "audience_blocked", 3600).await?;
    tx.commit().await?;
    if n >= limits::AUDIENCE_BLOCKS_PER_HOUR {
        trip(pool, context, "audience_blocked").await?;
        return Ok(true);
    }
    Ok(false)
}

fn too_large(what: &str, limit: usize) -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidRequest,
        format!("the {what} is larger than {} KiB", limit / 1024),
    )
    .with_details(json!({"reason": "payload_too_large", "what": what, "limit_bytes": limit}))
    .into()
}

/// §8.9: the document as the agent sent it. Measured re-serialized (the
/// transport already parsed it), so insignificant whitespace is not counted.
pub fn check_raw_payload(doc: &serde_json::Value) -> Result<()> {
    if serde_json::to_vec(doc)?.len() > limits::RAW_PAYLOAD_BYTES {
        return Err(too_large("payload", limits::RAW_PAYLOAD_BYTES));
    }
    Ok(())
}

/// §8.9: the normalized command document (after `commands::parse`).
pub fn check_normalized(canonical_len: usize) -> Result<()> {
    if canonical_len > limits::NORMALIZED_BYTES {
        return Err(too_large("normalized document", limits::NORMALIZED_BYTES));
    }
    Ok(())
}

/// §8.9: at most two proposal writes of one delegation at a time. A slot is
/// a transaction-scoped advisory lock (released at commit or rollback); a
/// third writer is told `rate_limited` instead of waiting. Taken before the
/// agent row lock, so two writers really do overlap up to it.
pub async fn take_proposal_slot(conn: &mut PgConnection, delegation: Uuid) -> Result<()> {
    for slot in 0..limits::CONCURRENT_PROPOSAL_WRITES {
        let got: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtext('oxledger.proposal_slot.' || $1::text), $2)",
        )
        .bind(delegation)
        .bind(slot)
        .fetch_one(&mut *conn)
        .await?;
        if got {
            return Ok(());
        }
    }
    Err(rate_limited("concurrent_proposal_writes", 1000))
}

/// §8.9: validate, 120 per delegation per hour, reserved and recorded in
/// its own committed transaction before the (read-only) trial run.
pub async fn reserve_validate(
    pool: &PgPool,
    agent: Uuid,
    context: Uuid,
    delegation: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT 1 FROM agent_principals WHERE agent_principal_id = $1 FOR UPDATE")
        .bind(agent)
        .execute(&mut *tx)
        .await?;
    if count(&mut tx, BY_DELEGATION, delegation, "validate", 3600).await?
        >= limits::VALIDATES_PER_HOUR
    {
        return Err(quota_exceeded(
            "validates_per_hour",
            limits::VALIDATES_PER_HOUR,
        ));
    }
    sqlx::query(
        "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
         VALUES ($1, $2, $3, 'validate')",
    )
    .bind(agent)
    .bind(context)
    .bind(delegation)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
