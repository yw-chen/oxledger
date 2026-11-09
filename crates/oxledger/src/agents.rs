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
use crate::policy::{self, PolicyMode};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const PROPOSAL_OPS: [&str; 7] = [
    "create_issue",
    "update_fields",
    "transition",
    "add_note",
    "duplicate_of",
    "create_version",
    "update_version",
];
/// Commands that need the delegator's `manage_versions` for a policy row (§17.11).
pub const VERSION_OPS: [&str; 2] = ["create_version", "update_version"];
/// Built-in field keys a delegation may make writable; custom fields are `cf:<id>`.
pub const BUILTIN_FIELDS: [&str; 7] = [
    "subject",
    "description",
    "assigned_to_id",
    "priority_id",
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
    /// The agent as history shows it (§16.2), e.g. `claude-code@alice`.
    pub agent_label: String,
    pub agent_client_id: Uuid,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Writes paused (§16.6, §16.10): when, and `manual` or the breaker's reason.
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspension_reason: Option<String>,
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
    let login: String =
        sqlx::query_scalar("SELECT coalesce(login, '') FROM principals WHERE id = $1")
            .bind(user)
            .fetch_one(&mut *conn)
            .await?;
    let base = handle_base(&client_name, &login);
    // UNIQUE and never changed (§16.2): a clash (two clients slugging alike)
    // takes the first free -2, -3, …; serialize_agent makes this race-free
    // per (user, client), the unique index across pairs.
    let mut n = 1;
    loop {
        let handle = if n == 1 {
            base.clone()
        } else {
            match base.split_once('@') {
                Some((a, b)) => format!("{a}-{n}@{b}"),
                None => format!("{base}-{n}"),
            }
        };
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM agent_principals WHERE handle = $1)")
                .bind(&handle)
                .fetch_one(&mut *conn)
                .await?;
        if !taken {
            sqlx::query(
                "INSERT INTO agent_principals (agent_principal_id, delegator_user_id, agent_client_id, handle)
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(id)
            .bind(user)
            .bind(client)
            .bind(&handle)
            .execute(&mut *conn)
            .await?;
            return Ok(id);
        }
        n += 1;
    }
}

/// `claude-code@alice`: the client name slugged, then the delegator's login.
pub fn handle_base(client_name: &str, login: &str) -> String {
    let mut slug = String::new();
    for c in client_name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "agent" } else { slug };
    format!("{slug}@{login}")
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

pub(crate) fn context_closed() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ContextUnavailable,
        "this agent work context was closed; a new context needs a new token",
    )
    .with_details(json!({"kind": "closed"}))
    .into()
}

/// SQL condition (aliases `c` credential, `w` context): the credential was
/// valid until its context closed (T-X20), not revoked on its own before.
pub(crate) const CLOSED_WITH_CONTEXT: &str = "w.state = 'closed' AND c.expires_at > now() AND (c.revoked_at IS NULL OR c.revoked_at >= w.closed_at)";

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
    /// Modes inside the ceiling (§16.10); anything not listed is `direct`.
    pub policies: Option<PolicyInput>,
}

/// Per-command and per-field modes (§16.3). Keys must be inside the ceiling.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    #[serde(default)]
    pub commands: BTreeMap<String, PolicyMode>,
    #[serde(default)]
    pub fields: BTreeMap<String, PolicyMode>,
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
    if let Some(p) = &input.policies {
        check_policy_keys(p, &input.proposal_ops, &input.writable_fields)?;
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
    // §16.3 rule 1: one row per command and field in the ceiling.
    let manage_versions = {
        let actor = crate::perm::actor(&mut tx, *ctx).await?;
        crate::perm::project_perms(&mut tx, &actor, input.project_id)
            .await?
            .is_some_and(|pp| pp.allowed_any(crate::perm::MANAGE_VERSIONS))
    };
    let chosen = input.policies.clone().unwrap_or_default();
    for op in &input.proposal_ops {
        if VERSION_OPS.contains(&op.as_str()) && !manage_versions {
            continue; // offered later as a new operation (§17.11)
        }
        let mode = chosen
            .commands
            .get(op)
            .copied()
            .unwrap_or(PolicyMode::Direct);
        sqlx::query(
            "INSERT INTO delegation_command_policies VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(view.id)
        .bind(op)
        .bind(mode)
        .execute(&mut *tx)
        .await?;
    }
    for f in &input.writable_fields {
        let mode = chosen.fields.get(f).copied().unwrap_or(PolicyMode::Direct);
        sqlx::query(
            "INSERT INTO delegation_field_policies VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(view.id)
        .bind(f)
        .bind(mode)
        .execute(&mut *tx)
        .await?;
    }
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

fn check_policy_keys(p: &PolicyInput, ops: &[String], fields: &[String]) -> Result<()> {
    for k in p.commands.keys() {
        if !ops.contains(k) {
            return Err(ApiError::new(
                ErrorCode::InvalidValue,
                format!("{k:?} is not one of the delegated operations"),
            )
            .with_details(json!({"path": "policies.commands", "allowed": ops}))
            .into());
        }
    }
    for k in p.fields.keys() {
        if !fields.contains(k) {
            return Err(ApiError::new(
                ErrorCode::InvalidValue,
                format!("{k:?} is not one of the delegated fields"),
            )
            .with_details(json!({"path": "policies.fields", "allowed": fields}))
            .into());
        }
    }
    Ok(())
}

/// A delegation's policies (§16.3) and what could be added (rule 1: commands
/// the server supports that the delegation lacks are never added by themselves).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PolicyView {
    pub delegation_id: Uuid,
    pub policy_version: i64,
    pub commands: BTreeMap<String, PolicyMode>,
    pub fields: BTreeMap<String, PolicyMode>,
    /// Operations the server supports that this delegation does not have.
    pub available_commands: Vec<String>,
    /// Built-in fields the server supports that this delegation cannot write
    /// yet (T-V13: e.g. `fixed_version_id` on a delegation from before U39);
    /// the delegation page offers to add them.
    pub available_fields: Vec<String>,
}

async fn policy_view_in(conn: &mut PgConnection, user: Uuid, id: Uuid) -> Result<PolicyView> {
    let row: Option<(i64, Vec<String>)> = sqlx::query_as(
        "SELECT policy_version, writable_fields FROM delegations WHERE id = $1 AND delegator_user_id = $2",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((policy_version, writable)) = row else {
        return Err(api(ErrorCode::NotFound, "no such delegation"));
    };
    let p = policy::load(&mut *conn, id).await?;
    let available_commands = PROPOSAL_OPS
        .iter()
        .filter(|o| !p.commands.contains_key(**o))
        .map(|o| o.to_string())
        .collect();
    let available_fields = BUILTIN_FIELDS
        .iter()
        .filter(|f| !writable.iter().any(|w| w == **f) && !p.fields.contains_key(**f))
        .map(|f| f.to_string())
        .collect();
    Ok(PolicyView {
        delegation_id: id,
        policy_version,
        commands: p.commands,
        fields: p.fields,
        available_commands,
        available_fields,
    })
}

/// The delegator reads a delegation's policies.
pub async fn delegation_policies(pool: &PgPool, ctx: &AuthContext, id: Uuid) -> Result<PolicyView> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let v = policy_view_in(&mut tx, user, id).await?;
    tx.commit().await?;
    Ok(v)
}

async fn manages_versions(
    conn: &mut PgConnection,
    ctx: &AuthContext,
    project: Uuid,
) -> Result<bool> {
    let actor = crate::perm::actor(&mut *conn, *ctx).await?;
    Ok(crate::perm::project_perms(&mut *conn, &actor, project)
        .await?
        .is_some_and(|pp| pp.allowed_any(crate::perm::MANAGE_VERSIONS)))
}

/// The delegator changes modes or adds commands / fields (§16.3 rule 4: the
/// delegation row FOR UPDATE, policy_version + 1). Any change may widen what
/// the agent does without review, so it needs a step-up like creating one.
pub async fn set_delegation_policies(
    pool: &PgPool,
    session_policy: &SessionPolicy,
    ctx: &AuthContext,
    id: Uuid,
    input: PolicyInput,
) -> Result<PolicyView> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    // §3.7 lock order: the browser session (step-up) before the delegation.
    auth::require_step_up(&mut tx, session_policy, ctx).await?;
    type Row = (
        Uuid,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
        Vec<String>,
        Vec<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT project_id, revoked_at, expires_at, proposal_ops, writable_fields FROM delegations
         WHERE id = $1 AND delegator_user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((project, revoked, expires, mut ops, mut fields)) = row else {
        return Err(api(ErrorCode::NotFound, "no such delegation"));
    };
    if revoked.is_some() || expires <= Utc::now() {
        return Err(api(
            ErrorCode::DelegationUnavailable,
            "the delegation was revoked or expired",
        ));
    }
    for k in input.commands.keys() {
        if !PROPOSAL_OPS.contains(&k.as_str()) {
            return Err(
                ApiError::new(ErrorCode::InvalidValue, format!("unknown operation {k:?}"))
                    .with_details(json!({"path": "commands", "allowed": PROPOSAL_OPS}))
                    .into(),
            );
        }
        if VERSION_OPS.contains(&k.as_str()) && !manages_versions(&mut tx, ctx, project).await? {
            return Err(ApiError::new(
                ErrorCode::ForbiddenOp,
                "you need manage_versions in this project to delegate version commands",
            )
            .with_details(json!({"reason": "permission", "path": "commands"}))
            .into());
        }
        if !ops.contains(k) {
            ops.push(k.clone());
        }
    }
    for k in input.fields.keys() {
        let ok = match k.strip_prefix("cf:") {
            Some(n) => match n.parse::<i32>() {
                Ok(fid) => sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM field_defs WHERE id = $1 AND visible AND index_state <> 'dropping')",
                )
                .bind(fid)
                .fetch_one(&mut *tx)
                .await?,
                Err(_) => false,
            },
            None => BUILTIN_FIELDS.contains(&k.as_str()),
        };
        if !ok {
            return Err(
                ApiError::new(ErrorCode::InvalidValue, format!("unknown field {k:?}"))
                    .with_details(json!({"path": "fields", "allowed": BUILTIN_FIELDS}))
                    .into(),
            );
        }
        if !fields.contains(k) {
            fields.push(k.clone());
        }
    }
    sqlx::query(
        "UPDATE delegations SET policy_version = policy_version + 1, proposal_ops = $2, writable_fields = $3
         WHERE id = $1",
    )
    .bind(id)
    .bind(&ops)
    .bind(&fields)
    .execute(&mut *tx)
    .await?;
    for (k, m) in &input.commands {
        sqlx::query(
            "INSERT INTO delegation_command_policies VALUES ($1, $2, $3)
             ON CONFLICT (delegation_id, command_kind) DO UPDATE SET mode = EXCLUDED.mode",
        )
        .bind(id)
        .bind(k)
        .bind(m)
        .execute(&mut *tx)
        .await?;
    }
    for (k, m) in &input.fields {
        sqlx::query(
            "INSERT INTO delegation_field_policies VALUES ($1, $2, $3)
             ON CONFLICT (delegation_id, field_key) DO UPDATE SET mode = EXCLUDED.mode",
        )
        .bind(id)
        .bind(k)
        .bind(m)
        .execute(&mut *tx)
        .await?;
    }
    auth::audit(
        &mut tx,
        "delegation_policy_changed",
        Some(user),
        Some(id),
        json!({"commands": input.commands, "fields": input.fields}),
    )
    .await?;
    let v = policy_view_in(&mut tx, user, id).await?;
    tx.commit().await?;
    Ok(v)
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
        let (agent_label, suspended_at, suspension_reason): (
            String,
            Option<DateTime<Utc>>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT p.name, c.suspended_at, c.suspension_reason
             FROM principals p LEFT JOIN agent_context_write_controls c ON c.context_id = $2
             WHERE p.id = $1",
        )
        .bind(agent)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        out.push(ContextView {
            id,
            agent_principal_id: agent,
            agent_label,
            suspended_at,
            suspension_reason,
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

/// "Pause writes" (§16.10): the delegator suspends the context's writes;
/// reads go on.
pub async fn suspend_context(pool: &PgPool, ctx: &AuthContext, context: Uuid) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    context_owned(&mut tx, user, context, false).await?;
    sqlx::query(
        "UPDATE agent_context_write_controls SET suspended_at = clock_timestamp(), suspension_reason = 'manual'
         WHERE context_id = $1 AND suspended_at IS NULL",
    )
    .bind(context)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "context_suspended",
        Some(user),
        Some(context),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Lifts a suspension (§16.6: only the delegator, in the web app, after
/// re-checking the context: active, R's epochs, a usable delegation). Needs
/// a step-up. Quotas are not reset: they count the log, which stays.
pub async fn resume_context(
    pool: &PgPool,
    policy: &SessionPolicy,
    ctx: &AuthContext,
    context: Uuid,
) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let (_, state) = context_owned(&mut tx, user, context, true).await?;
    if state != "active" {
        return Err(context_closed());
    }
    auth::require_step_up(&mut tx, policy, ctx).await?;
    if !changed_sources(&mut tx, context, true).await?.is_empty() {
        return Err(quarantined());
    }
    let usable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM delegations WHERE context_id = $1 AND revoked_at IS NULL
                                                  AND expires_at > clock_timestamp())",
    )
    .bind(context)
    .fetch_one(&mut *tx)
    .await?;
    if !usable {
        return Err(api(
            ErrorCode::DelegationUnavailable,
            "this context has no usable delegation",
        ));
    }
    let reason: Option<Option<String>> = sqlx::query_scalar(
        "SELECT suspension_reason FROM agent_context_write_controls WHERE context_id = $1 FOR UPDATE",
    )
    .bind(context)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE agent_context_write_controls SET suspended_at = NULL, suspension_reason = NULL WHERE context_id = $1",
    )
    .bind(context)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut tx,
        "context_resumed",
        Some(user),
        Some(context),
        json!({"was": reason.flatten()}),
    )
    .await?;
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

// ---------------------------------------------------------------- describe {"scope": "context"} (§5.3)

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ContextPublish {
    /// `same_or_narrower`: new non-private content written there passes the
    /// audience check against the current R; `blocked`: it would not.
    pub public_content: String,
    /// `per_target`: private notes and new private issues are judged by
    /// their actual readers when proposed; `blocked`: no delegation.
    pub private_destinations: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ContextProject {
    pub project: String,
    pub project_id: Uuid,
    /// `active`, `revoked` or `expired`.
    pub delegation: String,
    pub readable: bool,
    pub publish: ContextPublish,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ContextDescription {
    pub context_id: Uuid,
    pub state: String,
    /// R: every project ever delegated in this context (no reader lists).
    pub projects: Vec<ContextProject>,
}

/// §5.3: the agent's current work context. A closed or quarantined context
/// is `context_unavailable` (checked by `db::effective`).
pub async fn describe_context(
    app: &crate::service::App,
    caller: AuthContext,
) -> Result<ContextDescription> {
    let mut tx = crate::service::read_tx(app).await?;
    let me = crate::db::effective(&mut tx, caller).await?;
    let Some(a) = me.agent else {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "scope context is for agents; people see their delegations in the web app",
        )
        .with_details(json!({"reason": "agent_only"}))
        .into());
    };
    let state: String = sqlx::query_scalar("SELECT state FROM agent_work_contexts WHERE id = $1")
        .bind(a.context_id)
        .fetch_one(&mut *tx)
        .await?;
    let rows: Vec<(Uuid, String, bool, bool)> = sqlx::query_as(
        "SELECT p.id, p.key,
                EXISTS (SELECT 1 FROM delegations d WHERE d.context_id = r.context_id AND d.project_id = p.id
                          AND d.revoked_at IS NULL AND d.expires_at > clock_timestamp()),
                EXISTS (SELECT 1 FROM delegations d WHERE d.context_id = r.context_id AND d.project_id = p.id
                          AND d.revoked_at IS NOT NULL)
         FROM agent_context_source_projects r JOIN projects p ON p.id = r.project_id
         WHERE r.context_id = $1 ORDER BY p.key",
    )
    .bind(a.context_id)
    .fetch_all(&mut *tx)
    .await?;
    let scope = crate::db::load_scope(&mut tx, caller).await?;
    let readable: std::collections::BTreeSet<Uuid> = scope.project_ids().collect();
    let mut audiences = Vec::new();
    for (id, ..) in &rows {
        audiences.push(crate::audience::project_audience(&mut tx, *id).await?);
    }
    let projects = rows
        .iter()
        .enumerate()
        .map(|(i, (id, key, active, revoked))| {
            let fits = audiences.iter().all(|r| audiences[i].is_subset(r));
            ContextProject {
                project: key.clone(),
                project_id: *id,
                delegation: if *active {
                    "active"
                } else if *revoked {
                    "revoked"
                } else {
                    "expired"
                }
                .into(),
                readable: *active && readable.contains(id),
                publish: ContextPublish {
                    public_content: if *active && fits {
                        "same_or_narrower"
                    } else {
                        "blocked"
                    }
                    .into(),
                    private_destinations: if *active { "per_target" } else { "blocked" }.into(),
                },
            }
        })
        .collect();
    Ok(ContextDescription {
        context_id: a.context_id,
        state,
        projects,
    })
}
