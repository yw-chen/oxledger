// SPDX-License-Identifier: GPL-2.0-or-later
//! Agent clients, agent principals, work contexts, delegations and agent
//! tokens (contract 02 §3.5, §3.8). Everything here is done by a person in a
//! browser session; creating a delegation, a new context or a token also needs
//! a recent step-up.
//!
//! Locks follow the §3.7 order: agent_work_contexts → principals →
//! browser_sessions → delegations → agent_clients → projects.
//!
//! Not here yet (S6): `security_epoch` triggers, so epochs only change when
//! something writes them; audience inclusion across projects (M8, M9) at
//! propose/apply time; the delegation policy table of §16.

use crate::auth::{self, AuthContext, SessionPolicy};
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub const PROPOSAL_OPS: [&str; 5] = [
    "create_issue",
    "update_fields",
    "transition",
    "add_note",
    "duplicate_of",
];
/// Built-in field keys a delegation may make writable; custom fields are `cf:<id>`.
pub const BUILTIN_FIELDS: [&str; 7] = [
    "subject",
    "description",
    "assigned_to_id",
    "status_id",
    "start_date",
    "due_date",
    "fixed_version_id",
];
pub const DEFAULT_DELEGATION_DAYS: i64 = 7;
pub const MAX_DELEGATION_DAYS: i64 = 30;
pub const DEFAULT_TOKEN_DAYS: i64 = 30;
pub const MAX_TOKEN_DAYS: i64 = 30;

// ---------------------------------------------------------------- views

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AgentClientView {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, schemars::JsonSchema)]
pub struct DelegationView {
    pub id: Uuid,
    pub agent_principal_id: Uuid,
    pub agent_client_id: Uuid,
    pub context_id: Uuid,
    pub project_id: Uuid,
    pub proposal_ops: Vec<String>,
    pub writable_fields: Vec<String>,
    pub max_ops: i32,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ContextView {
    pub id: Uuid,
    pub agent_principal_id: Uuid,
    pub agent_client_id: Uuid,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// R: every project ever granted to this context, with the epoch recorded then.
    pub source_projects: Vec<SourceProject>,
    /// Source projects whose settings changed since (the context can no longer be used).
    pub changed_projects: Vec<Uuid>,
}

#[derive(Debug, Serialize, sqlx::FromRow, schemars::JsonSchema)]
pub struct SourceProject {
    pub project_id: Uuid,
    pub security_epoch: i64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IssuedCredential {
    pub id: Uuid,
    pub context_id: Uuid,
    /// Shown once; only its hash is stored.
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- helpers

fn invalid(path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path}))
        .into()
}

fn days(v: Option<i64>, default: i64, max: i64, path: &str) -> Result<Duration> {
    let d = v.unwrap_or(default);
    if !(1..=max).contains(&d) {
        return Err(invalid(
            path,
            format!("{path} must be between 1 and {max} days"),
        ));
    }
    Ok(Duration::days(d))
}

/// Whether every role that can give `view_issues` in the project reads the
/// same non-private issues (§3.8): visibility `all` or `default`, no tracker
/// restriction. Effective roles are resolved like `load_scope`: member roles
/// (direct or through a group) plus, on a public project, the Non member and
/// Anonymous built-in roles.
pub async fn audience_consistent(conn: &mut PgConnection, project_id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (
            SELECT 1 FROM roles r
            JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = $2
            WHERE (r.id IN (SELECT mr.role_id FROM members m JOIN member_roles mr ON mr.member_id = m.id
                            WHERE m.project_id = $1)
                   OR (r.builtin IN ('non_member', 'anonymous')
                       AND EXISTS (SELECT 1 FROM projects p WHERE p.id = $1 AND p.is_public)))
              AND (r.issues_visibility NOT IN ('all', 'default') OR NOT rp.all_trackers))",
    )
    .bind(project_id)
    .bind(db::VIEW_ISSUES)
    .fetch_one(conn)
    .await?)
}

/// Serializes the (user, client) pair for the rest of the transaction, so two
/// first delegations cannot both create the agent principal or the first
/// context (they would hit a unique index and fail with a 500). Advisory locks
/// are taken before any row lock, so this does not change the §3.7 order.
async fn serialize_agent(conn: &mut PgConnection, user: Uuid, client: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 2))")
        .bind(format!("agent:{user}:{client}"))
        .execute(conn)
        .await?;
    Ok(())
}

/// The agent principal for (user, client), created on first use. The client
/// must be enabled.
async fn agent_principal(conn: &mut PgConnection, user: Uuid, client: Uuid) -> Result<Uuid> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT agent_principal_id FROM agent_principals WHERE delegator_user_id = $1 AND agent_client_id = $2",
    )
    .bind(user)
    .bind(client)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let names: Option<(String, String)> = sqlx::query_as(
        "SELECT u.name, c.name FROM principals u, agent_clients c WHERE u.id = $1 AND c.id = $2 AND c.enabled",
    )
    .bind(user)
    .bind(client)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((user_name, client_name)) = names else {
        return Err(api(ErrorCode::NotFound, "no such agent client"));
    };
    let id: Uuid =
        sqlx::query_scalar("INSERT INTO principals (kind, name) VALUES ('agent', $1) RETURNING id")
            .bind(format!("{user_name} 的 {client_name}"))
            .fetch_one(&mut *conn)
            .await?;
    sqlx::query(
        "INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id) VALUES ($1,$2,$3)",
    )
    .bind(id)
    .bind(user)
    .bind(client)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Locks the enabled client (FOR SHARE) or fails.
async fn lock_enabled_client(conn: &mut PgConnection, client: Uuid) -> Result<()> {
    let ok: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM agent_clients WHERE id = $1 FOR SHARE")
            .bind(client)
            .fetch_optional(conn)
            .await?;
    match ok {
        Some(true) => Ok(()),
        _ => Err(api(ErrorCode::NotFound, "no such agent client")),
    }
}

/// Closes a context: revokes its delegations and tokens. The caller holds the
/// context row FOR UPDATE.
async fn close_context_locked(conn: &mut PgConnection, context: Uuid, reason: &str) -> Result<()> {
    sqlx::query("UPDATE agent_work_contexts SET state = 'closed', closed_at = now() WHERE id = $1")
        .bind(context)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE delegations SET revoked_at = now(), revoked_reason = $2
         WHERE context_id = $1 AND revoked_at IS NULL",
    )
    .bind(context)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE agent_credentials SET revoked_at = now() WHERE context_id = $1 AND revoked_at IS NULL")
        .bind(context)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Projects of R whose epoch differs from the one recorded (the context is
/// quarantined if any). Locks the project rows FOR SHARE in id order.
async fn changed_sources(conn: &mut PgConnection, context: Uuid, lock: bool) -> Result<Vec<Uuid>> {
    let sql = format!(
        "SELECT p.id, (p.security_epoch <> r.security_epoch) AS changed
         FROM agent_context_source_projects r JOIN projects p ON p.id = r.project_id
         WHERE r.context_id = $1 ORDER BY p.id{}",
        if lock { " FOR SHARE OF p" } else { "" }
    );
    let rows: Vec<(Uuid, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(context)
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, c)| *c)
        .map(|(id, _)| id)
        .collect())
}

pub(crate) fn quarantined() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ContextUnavailable,
        "the agent's work context can no longer be used because a project's permissions changed; start a new work context",
    )
    .with_details(json!({"kind": "quarantined"}))
    .into()
}

// ---------------------------------------------------------------- agent clients

pub async fn list_agent_clients(pool: &PgPool, ctx: &AuthContext) -> Result<Vec<AgentClientView>> {
    ctx.require_human_session()?;
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM agent_clients WHERE enabled ORDER BY name")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| AgentClientView { id, name })
        .collect())
}

/// Admin CLI: registers an agent client. Disabled until enabled.
pub async fn create_agent_client(
    pool: &PgPool,
    name: &str,
    egress_policy_ref: &str,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_clients (name, egress_policy_ref) VALUES ($1, $2) RETURNING id",
    )
    .bind(name.trim())
    .bind(egress_policy_ref.trim())
    .fetch_one(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "agent_client_created",
        None,
        Some(id),
        json!({"name": name.trim(), "egress_policy_ref": egress_policy_ref.trim()}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Admin CLI: enables or disables a client. Disabling stops every agent of
/// that client on its next request (`db::effective` checks it).
pub async fn set_agent_client_enabled(pool: &PgPool, name: &str, enabled: bool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let id: Option<Uuid> =
        sqlx::query_scalar("UPDATE agent_clients SET enabled = $2 WHERE name = $1 RETURNING id")
            .bind(name.trim())
            .bind(enabled)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(id) = id else {
        return Err(api(
            ErrorCode::NotFound,
            format!("no agent client {name:?}"),
        ));
    };
    auth::audit(
        &mut tx,
        if enabled {
            "agent_client_enabled"
        } else {
            "agent_client_disabled"
        },
        None,
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- delegations

#[derive(Debug, Clone)]
pub struct DelegationInput {
    pub agent_client_id: Uuid,
    pub project_id: Uuid,
    pub proposal_ops: Vec<String>,
    pub writable_fields: Vec<String>,
    pub max_ops: Option<i32>,
    pub expires_in_days: Option<i64>,
}

/// Creates a delegation (§3.5). Adds it to the agent's active work context,
/// or creates the agent's first context. Needs a step-up.
pub async fn create_delegation(
    pool: &PgPool,
    policy: &SessionPolicy,
    ctx: &AuthContext,
    input: DelegationInput,
) -> Result<DelegationView> {
    let (user, session) = ctx.require_human_session()?;
    // Validate the request shape before taking locks.
    if input.proposal_ops.is_empty() {
        return Err(invalid("proposal_ops", "choose at least one operation"));
    }
    for op in &input.proposal_ops {
        if !PROPOSAL_OPS.contains(&op.as_str()) {
            return Err(ApiError::new(
                ErrorCode::InvalidValue,
                format!("unknown operation {op:?}"),
            )
            .with_details(json!({"path": "proposal_ops", "allowed": PROPOSAL_OPS}))
            .into());
        }
    }
    let max_ops = input.max_ops.unwrap_or(10);
    if !(1..=10).contains(&max_ops) {
        return Err(invalid("max_ops", "max_ops must be between 1 and 10"));
    }
    let lifetime = days(
        input.expires_in_days,
        DEFAULT_DELEGATION_DAYS,
        MAX_DELEGATION_DAYS,
        "expires_in_days",
    )?;

    let mut tx = pool.begin().await?;
    // Field keys: built-in names or cf:<id> of an existing field.
    for f in &input.writable_fields {
        let ok = match f.strip_prefix("cf:") {
            Some(n) => match n.parse::<i32>() {
                Ok(id) => sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM field_defs WHERE id = $1 AND visible AND index_state <> 'dropping')",
                )
                .bind(id)
                .fetch_one(&mut *tx)
                .await?,
                Err(_) => false,
            },
            None => BUILTIN_FIELDS.contains(&f.as_str()),
        };
        if !ok {
            return Err(
                ApiError::new(ErrorCode::InvalidValue, format!("unknown field {f:?}"))
                    .with_details(json!({"path": "writable_fields", "allowed": BUILTIN_FIELDS}))
                    .into(),
            );
        }
    }

    serialize_agent(&mut tx, user, input.agent_client_id).await?;
    let agent = agent_principal(&mut tx, user, input.agent_client_id).await?;
    // 1. context (FOR UPDATE)
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE agent_principal_id = $1 AND state = 'active' FOR UPDATE",
    )
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    // 2. principals, 3. session (step-up)
    sqlx::query("SELECT id FROM principals WHERE id = ANY($1) ORDER BY id FOR SHARE")
        .bind(vec![user.min(agent), user.max(agent)])
        .execute(&mut *tx)
        .await?;
    auth::require_step_up(&mut tx, policy, ctx).await?;
    // 4. client
    lock_enabled_client(&mut tx, input.agent_client_id).await?;

    let context = match active {
        Some(c) => c,
        None => {
            let had_one: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM agent_work_contexts WHERE agent_principal_id = $1)",
            )
            .bind(agent)
            .fetch_one(&mut *tx)
            .await?;
            if had_one {
                // A closed context exists: only an explicit new context
                // (with the fresh-context declaration) may follow it.
                return Err(ApiError::new(
                    ErrorCode::ContextUnavailable,
                    "this agent's work context is closed; start a new work context first",
                )
                .with_details(json!({"kind": "closed"}))
                .into());
            }
            sqlx::query_scalar(
                "INSERT INTO agent_work_contexts (agent_principal_id, fresh_context_ack, created_by_session)
                 VALUES ($1, true, $2) RETURNING id",
            )
            .bind(agent)
            .bind(session)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    // 5. projects: R ∪ the new one, FOR SHARE in id order.
    let mut lock_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM agent_context_source_projects WHERE context_id = $1",
    )
    .bind(context)
    .fetch_all(&mut *tx)
    .await?;
    lock_ids.push(input.project_id);
    lock_ids.sort();
    lock_ids.dedup();
    sqlx::query("SELECT id FROM projects WHERE id = ANY($1) ORDER BY id FOR SHARE")
        .bind(&lock_ids)
        .execute(&mut *tx)
        .await?;
    if !changed_sources(&mut tx, context, false).await?.is_empty() {
        return Err(quarantined());
    }
    // The delegator must be able to read the project; otherwise it does not exist for them.
    let scope = db::load_scope(&mut tx, *ctx).await?;
    if !scope.project_ids().any(|p| p == input.project_id) {
        return Err(api(ErrorCode::NotFound, "no such project"));
    }
    if !audience_consistent(&mut tx, input.project_id).await? {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "agents can only be delegated projects where everyone who can see issues sees the same issues (no \"own\" or tracker-limited roles)",
        )
        .with_details(json!({"kind": "project_not_audience_consistent"}))
        .into());
    }
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM delegations WHERE context_id = $1 AND project_id = $2 AND revoked_at IS NULL)",
    )
    .bind(context)
    .bind(input.project_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        return Err(ApiError::new(
            ErrorCode::InvalidValue,
            "this agent already has a delegation for the project; revoke it first to change the scope",
        )
        .with_details(json!({"path": "project_id", "kind": "already_delegated"}))
        .into());
    }
    let epoch: i64 = sqlx::query_scalar("SELECT security_epoch FROM projects WHERE id = $1")
        .bind(input.project_id)
        .fetch_one(&mut *tx)
        .await?;
    let now = Utc::now();
    let view: DelegationView = sqlx::query_as(
        "INSERT INTO delegations (agent_principal_id, delegator_user_id, context_id, project_id, security_epoch,
                                  proposal_ops, writable_fields, max_ops, created_at, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         RETURNING id, agent_principal_id, $11::uuid AS agent_client_id, context_id, project_id, proposal_ops,
                   writable_fields, max_ops, created_at, expires_at, revoked_at",
    )
    .bind(agent)
    .bind(user)
    .bind(context)
    .bind(input.project_id)
    .bind(epoch)
    .bind(&input.proposal_ops)
    .bind(&input.writable_fields)
    .bind(max_ops)
    .bind(now)
    .bind(now + lifetime)
    .bind(input.agent_client_id)
    .fetch_one(&mut *tx)
    .await?;
    // R grows in the same transaction (M3); never shrinks (M4).
    sqlx::query(
        "INSERT INTO agent_context_source_projects (context_id, project_id, security_epoch, first_delegation_id)
         VALUES ($1,$2,$3,$4) ON CONFLICT (context_id, project_id) DO NOTHING",
    )
    .bind(context)
    .bind(input.project_id)
    .bind(epoch)
    .bind(view.id)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "delegation_created",
        Some(user),
        Some(view.id),
        json!({"project_id": input.project_id, "context_id": context}),
    )
    .await?;
    tx.commit().await?;
    Ok(view)
}

const DELEGATION_COLUMNS: &str =
    "d.id, d.agent_principal_id, ap.agent_client_id, d.context_id, d.project_id,
    d.proposal_ops, d.writable_fields, d.max_ops, d.created_at, d.expires_at, d.revoked_at";

pub async fn list_delegations(pool: &PgPool, ctx: &AuthContext) -> Result<Vec<DelegationView>> {
    let (user, _) = ctx.require_human_session()?;
    let sql = format!(
        "SELECT {DELEGATION_COLUMNS} FROM delegations d
         JOIN agent_principals ap ON ap.agent_principal_id = d.agent_principal_id
         WHERE d.delegator_user_id = $1 ORDER BY d.created_at DESC"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .fetch_all(pool)
        .await?)
}

/// Revokes one delegation: only that project shrinks (R keeps it, M4).
pub async fn revoke_delegation(pool: &PgPool, ctx: &AuthContext, id: Uuid) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let context: Option<Uuid> = sqlx::query_scalar(
        "SELECT context_id FROM delegations WHERE id = $1 AND delegator_user_id = $2",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(context) = context else {
        return Err(api(ErrorCode::NotFound, "no such delegation"));
    };
    sqlx::query("SELECT id FROM agent_work_contexts WHERE id = $1 FOR UPDATE")
        .bind(context)
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query(
        "UPDATE delegations SET revoked_at = now(), revoked_reason = 'revoked_by_delegator'
         WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        auth::audit(
            &mut tx,
            "delegation_revoked",
            Some(user),
            Some(id),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- work contexts

async fn context_owned(
    conn: &mut PgConnection,
    user: Uuid,
    context: Uuid,
    lock: bool,
) -> Result<(Uuid, String)> {
    let sql = format!(
        "SELECT w.agent_principal_id, w.state FROM agent_work_contexts w
         JOIN agent_principals ap ON ap.agent_principal_id = w.agent_principal_id
         WHERE w.id = $1 AND ap.delegator_user_id = $2{}",
        if lock { " FOR UPDATE OF w" } else { "" }
    );
    let row: Option<(Uuid, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(context)
        .bind(user)
        .fetch_optional(conn)
        .await?;
    row.ok_or_else(|| api(ErrorCode::NotFound, "no such work context"))
}

type ContextRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

pub async fn list_contexts(pool: &PgPool, ctx: &AuthContext) -> Result<Vec<ContextView>> {
    let (user, _) = ctx.require_human_session()?;
    let mut conn = pool.acquire().await?;
    let rows: Vec<ContextRow> = sqlx::query_as(
        "SELECT w.id, w.agent_principal_id, ap.agent_client_id, w.state, w.created_at, w.closed_at
         FROM agent_work_contexts w JOIN agent_principals ap ON ap.agent_principal_id = w.agent_principal_id
         WHERE ap.delegator_user_id = $1 ORDER BY w.created_at DESC",
    )
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, agent, client, state, created_at, closed_at) in rows {
        let source_projects: Vec<SourceProject> = sqlx::query_as(
            "SELECT project_id, security_epoch FROM agent_context_source_projects WHERE context_id = $1 ORDER BY project_id",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let changed_projects = changed_sources(&mut conn, id, false).await?;
        out.push(ContextView {
            id,
            agent_principal_id: agent,
            agent_client_id: client,
            state,
            created_at,
            closed_at,
            source_projects,
            changed_projects,
        });
    }
    Ok(out)
}

/// Starts a new work context for the agent of `client` (§3.5). The person
/// declares that the agent starts without what it read before
/// (`fresh_context_ack`); the old context closes with its delegations and
/// tokens. Needs a step-up.
pub async fn new_context(
    pool: &PgPool,
    policy: &SessionPolicy,
    ctx: &AuthContext,
    agent_client_id: Uuid,
    fresh_context_ack: bool,
) -> Result<Uuid> {
    let (user, session) = ctx.require_human_session()?;
    if !fresh_context_ack {
        return Err(invalid(
            "fresh_context_ack",
            "confirm that the agent starts a new work context without what it read before",
        ));
    }
    let mut tx = pool.begin().await?;
    let agent = agent_principal(&mut tx, user, agent_client_id).await?;
    let old: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE agent_principal_id = $1 AND state = 'active' FOR UPDATE",
    )
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query("SELECT id FROM principals WHERE id = ANY($1) ORDER BY id FOR SHARE")
        .bind(vec![user.min(agent), user.max(agent)])
        .execute(&mut *tx)
        .await?;
    auth::require_step_up(&mut tx, policy, ctx).await?;
    lock_enabled_client(&mut tx, agent_client_id).await?;
    if let Some(old) = old {
        close_context_locked(&mut tx, old, "replaced_by_new_context").await?;
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_work_contexts (agent_principal_id, fresh_context_ack, created_by_session)
         VALUES ($1, true, $2) RETURNING id",
    )
    .bind(agent)
    .bind(session)
    .fetch_one(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "context_created",
        Some(user),
        Some(id),
        json!({"replaced": old}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// "Stop this agent": closes the context, revokes its delegations and tokens.
pub async fn close_context(pool: &PgPool, ctx: &AuthContext, context: Uuid) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let (_, state) = context_owned(&mut tx, user, context, true).await?;
    if state == "active" {
        close_context_locked(&mut tx, context, "context_closed").await?;
        auth::audit(
            &mut tx,
            "context_closed",
            Some(user),
            Some(context),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- tokens

/// Issues a token for an active context (shown once). Needs a step-up.
pub async fn issue_credential(
    pool: &PgPool,
    policy: &SessionPolicy,
    ctx: &AuthContext,
    context: Uuid,
    label: &str,
    expires_in_days: Option<i64>,
) -> Result<IssuedCredential> {
    let (user, _) = ctx.require_human_session()?;
    let lifetime = days(
        expires_in_days,
        DEFAULT_TOKEN_DAYS,
        MAX_TOKEN_DAYS,
        "expires_in_days",
    )?;
    if label.chars().count() > 100 {
        return Err(invalid("label", "label is at most 100 characters"));
    }
    let mut tx = pool.begin().await?;
    // Lock first, then read the state, so a concurrent close cannot slip in between.
    sqlx::query("SELECT id FROM agent_work_contexts WHERE id = $1 FOR SHARE")
        .bind(context)
        .execute(&mut *tx)
        .await?;
    let (_, state) = context_owned(&mut tx, user, context, false).await?;
    auth::require_step_up(&mut tx, policy, ctx).await?;
    if state != "active" {
        return Err(
            ApiError::new(ErrorCode::ContextUnavailable, "this work context is closed")
                .with_details(json!({"kind": "closed"}))
                .into(),
        );
    }
    let (token, hash) = auth::new_agent_token();
    let expires_at = Utc::now() + lifetime;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agent_credentials (context_id, token_sha256, label, expires_at) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(context)
    .bind(&hash[..])
    .bind(label)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "agent_token_issued",
        Some(user),
        Some(id),
        json!({"context_id": context}),
    )
    .await?;
    tx.commit().await?;
    Ok(IssuedCredential {
        id,
        context_id: context,
        token,
        expires_at,
    })
}

pub async fn revoke_credential(pool: &PgPool, ctx: &AuthContext, id: Uuid) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "UPDATE agent_credentials c SET revoked_at = now()
         FROM agent_work_contexts w JOIN agent_principals ap ON ap.agent_principal_id = w.agent_principal_id
         WHERE c.id = $1 AND c.context_id = w.id AND ap.delegator_user_id = $2 AND c.revoked_at IS NULL",
    )
    .bind(id)
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM agent_credentials c JOIN agent_work_contexts w ON w.id = c.context_id
             JOIN agent_principals ap ON ap.agent_principal_id = w.agent_principal_id
             WHERE c.id = $1 AND ap.delegator_user_id = $2)",
        )
        .bind(id)
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(api(ErrorCode::NotFound, "no such token"));
        }
    } else {
        auth::audit(
            &mut tx,
            "agent_token_revoked",
            Some(user),
            Some(id),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
