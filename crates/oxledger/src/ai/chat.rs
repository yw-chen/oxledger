// SPDX-License-Identifier: GPL-2.0-or-later
//! The built-in AI chat of the right-hand column (design 05 §3.13): the
//! user's `oxledger-ai@<login>` principal, its runtime and context,
//! conversations, messages and their re-checks, deletion, retention and the
//! D12 hook.
//!
//! "Not visible = does not exist": another person's conversation, run or
//! message is `not_found` (site administrators too, P2). Stored tool results
//! and the replies drawn from them are re-checked every time they are read
//! or sent to the model (§3.13.3 point 3).

use super::runner;
use crate::agents::{self, DelegationInput, PolicyInput};
use crate::auth::{self, AuthContext, SessionPolicy};
use crate::cursor::{self, Position};
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::policy::PolicyMode;
use crate::service::App;
use chrono::{DateTime, Utc};
use ox_core::visibility::{IssueFacts, Scope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

pub const CLIENT_NAME: &str = "oxledger-ai";
/// Placeholder text for content that can no longer be read (§3.13.3).
pub const UNAVAILABLE_TEXT: &str = "（這段內容已無法讀取）";
pub const PAGE_UNAVAILABLE_TEXT: &str = "AI 助理讀不到這個頁面（私有議題或沒委派的專案）";
pub const RUN_INTERRUPTED_TEXT: &str = "回覆中斷";
const MAX_TEXT: usize = 8000;
const MAX_PAGE_REFS: usize = 5;
const MAX_TITLE: usize = 120;

// ---------------------------------------------------------------- basics

/// The settings row and the built-in client exist (a fresh database has
/// them from 0030; a test reset may have truncated them). Returns the
/// client id.
pub(crate) async fn ensure_basics(conn: &mut PgConnection) -> anyhow::Result<Uuid> {
    sqlx::query("INSERT INTO ai_settings DEFAULT VALUES ON CONFLICT DO NOTHING")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO agent_clients (name, enabled, egress_policy_ref, builtin)
         SELECT $1, s.enabled, 'ai_settings.active_provider_id', true FROM ai_settings s
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(CLIENT_NAME)
    .execute(&mut *conn)
    .await?;
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM agent_clients WHERE name = $1 AND builtin")
            .bind(CLIENT_NAME)
            .fetch_optional(&mut *conn)
            .await?;
    id.ok_or_else(|| anyhow::anyhow!("agent client oxledger-ai is not the built-in client"))
}

pub(crate) fn unavailable(kind: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::AiUnavailable, msg.to_string())
        .with_details(json!({"kind": kind}))
        .into()
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such conversation")
}

/// The user's built-in principal, runtime and active context, if set up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Assistant {
    pub principal: Uuid,
    pub runtime: Uuid,
    pub context: Option<Uuid>,
}

pub(crate) async fn assistant_of(
    conn: &mut PgConnection,
    user: Uuid,
) -> anyhow::Result<Option<Assistant>> {
    let row: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT p.agent_principal_id, p.runtime_id,
                (SELECT w.id FROM agent_work_contexts w WHERE w.runtime_id = r.id AND w.state = 'active')
         FROM agent_principals p JOIN agent_runtimes r ON r.id = p.runtime_id
         WHERE p.delegator_user_id = $1 AND r.kind = 'builtin'
           AND r.retired_at IS NULL AND p.retired_at IS NULL
         ORDER BY r.created_at LIMIT 1",
    )
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(principal, runtime, context)| Assistant {
        principal,
        runtime,
        context,
    }))
}

/// Creates the built-in runtime, principal and first context on first use
/// (§3.13.1). The caller holds a transaction.
async fn ensure_assistant(conn: &mut PgConnection, ctx: &AuthContext) -> anyhow::Result<Assistant> {
    let (user, session) = ctx.require_human_session()?;
    let client = ensure_basics(&mut *conn).await?;
    agents::serialize_sponsor(&mut *conn, user).await?;
    let existing = assistant_of(&mut *conn, user).await?;
    let (principal, runtime) = match existing {
        Some(a) => (a.principal, a.runtime),
        None => {
            let label = agents::free_runtime_label(&mut *conn, user, CLIENT_NAME).await?;
            let runtime: Uuid = sqlx::query_scalar(
                "INSERT INTO agent_runtimes (sponsor_user_id, kind, label, isolation_ack, created_by_session)
                 VALUES ($1, 'builtin', $2, true, $3) RETURNING id",
            )
            .bind(user)
            .bind(&label)
            .bind(session)
            .fetch_one(&mut *conn)
            .await?;
            let principal = agents::insert_principal(
                &mut *conn,
                &agents::NewPrincipal {
                    user,
                    runtime,
                    client,
                    role_key: "personal",
                    template: "builtin_chat",
                    template_version: crate::ai_team::TEMPLATE_VERSION,
                    duties: "",
                },
            )
            .await?;
            auth::audit(
                &mut *conn,
                "ai_assistant_created",
                Some(user),
                Some(principal),
                json!({"runtime_id": runtime}),
            )
            .await?;
            (principal, runtime)
        }
    };
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE runtime_id = $1 AND state = 'active'",
    )
    .bind(runtime)
    .fetch_optional(&mut *conn)
    .await?;
    let context = match active {
        Some(c) => c,
        None => {
            let had: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM agent_work_contexts WHERE runtime_id = $1)",
            )
            .bind(runtime)
            .fetch_one(&mut *conn)
            .await?;
            if had {
                // A closed context: only the explicit new context (step-up,
                // §3.13.7) opens the next one.
                return Err(agents::runtime_context_closed());
            }
            sqlx::query_scalar(
                "INSERT INTO agent_work_contexts (created_for_principal_id, runtime_id, fresh_context_ack, created_by_session)
                 VALUES ($1, $2, true, $3) RETURNING id",
            )
            .bind(principal)
            .bind(runtime)
            .bind(session)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(Assistant {
        principal,
        runtime,
        context: Some(context),
    })
}

// ---------------------------------------------------------------- status

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderRef {
    pub name: String,
    /// `anthropic` or `openai_compat`.
    pub kind: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AssistantStatus {
    pub principal: Uuid,
    pub handle: String,
    pub runtime_id: Uuid,
    /// Unrevoked, unexpired delegations of the built-in AI.
    pub delegated_projects: i64,
    /// `active`, `quarantined` (a project's permissions changed) or `closed`.
    pub context_state: String,
    /// The earliest expiry among those delegations.
    pub delegations_expire_at: Option<DateTime<Utc>>,
    /// Within 7 days: the column offers 「延長」.
    pub expiring_soon: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AiLimits {
    pub runs_left_this_hour: i64,
    /// USD, a decimal string.
    pub budget_left_today_usd: String,
    /// When the oldest run of the last hour stops counting (null when none).
    pub hour_resets_at: Option<DateTime<Utc>>,
}

/// `GET /ai/status` (§3.13.2).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AiStatus {
    /// false: the column shows no AI tab.
    pub enabled: bool,
    pub provider: Option<ProviderRef>,
    pub assistant: Option<AssistantStatus>,
    pub limits: AiLimits,
}

pub async fn status(app: &App, ctx: AuthContext) -> anyhow::Result<AiStatus> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    ensure_basics(&mut tx).await?;
    let (enabled, provider): (bool, Option<(String, String, String)>) = {
        let row: (bool, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT s.enabled, p.name, p.kind, p.model FROM ai_settings s
             LEFT JOIN ai_providers p ON p.id = s.active_provider_id AND p.retired_at IS NULL",
        )
        .fetch_one(&mut *tx)
        .await?;
        let p = match (row.1, row.2, row.3) {
            (Some(n), Some(k), Some(m)) => Some((n, k, m)),
            _ => None,
        };
        (row.0, p)
    };
    let assistant = match assistant_of(&mut tx, user).await? {
        None => None,
        Some(a) => {
            let handle: String = sqlx::query_scalar(
                "SELECT handle FROM agent_principals WHERE agent_principal_id = $1",
            )
            .bind(a.principal)
            .fetch_one(&mut *tx)
            .await?;
            let (n, earliest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT count(*), min(expires_at) FROM delegations
                 WHERE agent_principal_id = $1 AND context_id = $2
                   AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(a.principal)
            .bind(a.context)
            .fetch_one(&mut *tx)
            .await?;
            let context_state = match a.context {
                None => "closed".to_string(),
                Some(c) if agents::context_changed(&mut tx, c).await? => "quarantined".into(),
                Some(_) => "active".into(),
            };
            Some(AssistantStatus {
                principal: a.principal,
                handle,
                runtime_id: a.runtime,
                delegated_projects: n,
                context_state,
                delegations_expire_at: earliest,
                expiring_soon: earliest.is_some_and(|e| e - Utc::now() < chrono::Duration::days(7)),
            })
        }
    };
    let limits = super::budget::limits_of(&mut tx, user).await?;
    tx.commit().await?;
    Ok(AiStatus {
        enabled,
        provider: provider.map(|(name, kind, model)| ProviderRef { name, kind, model }),
        assistant,
        limits,
    })
}

async fn require_enabled(conn: &mut PgConnection) -> anyhow::Result<()> {
    ensure_basics(&mut *conn).await?;
    let on: bool = sqlx::query_scalar("SELECT enabled FROM ai_settings")
        .fetch_one(&mut *conn)
        .await?;
    if !on {
        return Err(unavailable("disabled", "the AI assistant is not enabled"));
    }
    Ok(())
}

/// `POST /ai/assistant`: sets up the built-in AI (runtime, principal and
/// context) and answers its status. Delegations are then created with
/// `POST /delegations {agent_principal_id}` (step-up), as for any role.
pub async fn setup(app: &App, ctx: AuthContext) -> anyhow::Result<AiStatus> {
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    require_enabled(&mut tx).await?;
    ensure_assistant(&mut tx, &ctx).await?;
    tx.commit().await?;
    status(app, ctx).await
}

// ---------------------------------------------------------------- delegations

/// A delegation to carry over or renew with the same policies.
#[derive(Debug, Clone)]
pub(crate) struct Carried {
    pub id: Uuid,
    pub project: Uuid,
    pub ops: Vec<String>,
    pub fields: Vec<String>,
    pub max_ops: i32,
    pub read_own_time: bool,
    pub commands: BTreeMap<String, PolicyMode>,
    pub field_modes: BTreeMap<String, PolicyMode>,
}

/// The live delegations of the built-in runtime's active context.
pub(crate) async fn carried_delegations(
    conn: &mut PgConnection,
    runtime: Uuid,
) -> anyhow::Result<Vec<Carried>> {
    type Row = (Uuid, Uuid, Vec<String>, Vec<String>, i32, bool);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT d.id, d.project_id, d.proposal_ops, d.writable_fields, d.max_ops, d.read_own_time
         FROM delegations d JOIN agent_work_contexts w ON w.id = d.context_id
         WHERE w.runtime_id = $1 AND w.state = 'active'
           AND d.revoked_at IS NULL AND d.expires_at > now()
         ORDER BY d.project_id",
    )
    .bind(runtime)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for (id, project, ops, fields, max_ops, read_own_time) in rows {
        let commands: Vec<(String, PolicyMode)> = sqlx::query_as(
            "SELECT command_kind, mode FROM delegation_command_policies WHERE delegation_id = $1",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        let field_modes: Vec<(String, PolicyMode)> = sqlx::query_as(
            "SELECT field_key, mode FROM delegation_field_policies WHERE delegation_id = $1",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        out.push(Carried {
            id,
            project,
            ops,
            fields,
            max_ops,
            read_own_time,
            commands: commands.into_iter().collect(),
            field_modes: field_modes.into_iter().collect(),
        });
    }
    Ok(out)
}

/// The conversations of a built-in runtime become read-only (its context
/// was replaced, §3.13.7).
pub(crate) async fn seal_conversations(
    conn: &mut PgConnection,
    runtime: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE ai_conversations SET state = 'read_only' WHERE runtime_id = $1 AND state = 'open'",
    )
    .bind(runtime)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Recreates `carried` in the runtime's active context with the same
/// policies and a new 30-day term, for the projects the user may still
/// delegate (readable and audience-consistent); the others are dropped.
pub(crate) async fn recreate_delegations(
    conn: &mut PgConnection,
    policy: &SessionPolicy,
    ctx: &AuthContext,
    runtime: Uuid,
    carried: &[Carried],
) -> anyhow::Result<usize> {
    if carried.is_empty() {
        return Ok(0);
    }
    let (user, _) = ctx.require_human_session()?;
    let principal: Uuid = sqlx::query_scalar(
        "SELECT agent_principal_id FROM agent_principals WHERE runtime_id = $1 AND retired_at IS NULL
         ORDER BY created_at LIMIT 1",
    )
    .bind(runtime)
    .fetch_one(&mut *conn)
    .await?;
    let client = ensure_basics(&mut *conn).await?;
    let scope = db::load_scope(&mut *conn, *ctx).await?;
    let readable: HashSet<Uuid> = scope.project_ids().collect();
    let mut n = 0;
    for c in carried {
        if !readable.contains(&c.project)
            || !agents::audience_consistent(&mut *conn, c.project).await?
        {
            continue;
        }
        let input = DelegationInput {
            agent_client_id: client,
            project_id: c.project,
            proposal_ops: c.ops.clone(),
            writable_fields: c.fields.clone(),
            max_ops: Some(c.max_ops),
            expires_in_days: Some(agents::MAX_DELEGATION_DAYS),
            policies: Some(PolicyInput {
                commands: c.commands.clone(),
                fields: c.field_modes.clone(),
            }),
            read_own_time: c.read_own_time,
            read_own_time_ack: c.read_own_time,
        };
        let spec = agents::check_spec(&input)?;
        agents::delegate_in(
            &mut *conn,
            policy,
            ctx,
            agents::Target {
                agent: principal,
                runtime,
                client,
            },
            &input,
            spec,
            agents::ClosedAs::Runtime,
        )
        .await?;
        n += 1;
    }
    auth::audit(
        &mut *conn,
        "ai_assistant_delegations_carried",
        Some(user),
        Some(principal),
        json!({"carried": n, "dropped": carried.len() - n}),
    )
    .await?;
    Ok(n)
}

/// `POST /ai/assistant/extend` (§3.13.7, T-M1-134): every live delegation
/// of the built-in AI is replaced by a new one with the same policies and a
/// new 30-day term (a delegation's term counts from its creation). Step-up.
pub async fn extend(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
) -> anyhow::Result<AiStatus> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    require_enabled(&mut tx).await?;
    agents::serialize_sponsor(&mut tx, user).await?;
    let Some(a) = assistant_of(&mut tx, user).await? else {
        return Err(api(ErrorCode::NotFound, "the AI assistant is not set up"));
    };
    let Some(context) = a.context else {
        return Err(agents::runtime_context_closed());
    };
    sqlx::query("SELECT id FROM agent_work_contexts WHERE id = $1 FOR UPDATE")
        .bind(context)
        .execute(&mut *tx)
        .await?;
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    let carried = carried_delegations(&mut tx, a.runtime).await?;
    let ids: Vec<Uuid> = carried.iter().map(|c| c.id).collect();
    sqlx::query(
        "UPDATE delegations SET revoked_at = now(), revoked_reason = 'extended'
         WHERE id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    recreate_delegations(&mut tx, policy, &ctx, a.runtime, &carried).await?;
    tx.commit().await?;
    status(app, ctx).await
}

// ---------------------------------------------------------------- conversations

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversationView {
    pub id: Uuid,
    /// What the user typed; empty: show the creation time.
    pub title: String,
    /// `open` or `read_only` (the AI's work context was replaced).
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConversationList {
    pub items: Vec<ConversationView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationsQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewConversation {
    /// Up to 120 characters, typed by the user (never made up by the model).
    #[serde(default)]
    pub title: Option<String>,
}

type ConvRow = (Uuid, String, String, DateTime<Utc>, DateTime<Utc>, bool);

const CONV_SELECT: &str = "SELECT c.id, c.title, c.state, c.created_at, c.last_at,
        (c.state = 'open' AND EXISTS (SELECT 1 FROM agent_work_contexts w
                                       WHERE w.id = c.context_id AND w.state = 'active'))
 FROM ai_conversations c";

fn conv_view(r: ConvRow) -> ConversationView {
    ConversationView {
        id: r.0,
        title: r.1,
        state: if r.5 {
            "open".into()
        } else {
            "read_only".into()
        },
        created_at: r.3,
        last_at: r.4,
    }
}

/// `GET /ai/conversations`: the caller's own, newest activity first.
pub async fn list(
    app: &App,
    ctx: AuthContext,
    q: &ConversationsQuery,
) -> anyhow::Result<ConversationList> {
    let (user, _) = ctx.require_human_session()?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100) as i64;
    let hash = cursor::query_hash(&json!({"ai_conversations": true}));
    let after = match q.cursor.as_deref() {
        None => None,
        Some(c) => match cursor::decode(&app.pool, c, Some(user), &hash).await? {
            Position::Keyset { t, i } => Some((t, i)),
            _ => return Err(cursor::mismatch()),
        },
    };
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    let rows: Vec<ConvRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{CONV_SELECT} WHERE c.user_id = $1 AND c.deleted_at IS NULL AND c.purged_at IS NULL
           AND ($2::timestamptz IS NULL OR (c.last_at, c.id) < ($2, $3))
         ORDER BY c.last_at DESC, c.id DESC LIMIT $4"
    )))
    .bind(user)
    .bind(after.map(|a| a.0))
    .bind(after.map(|a| a.1).unwrap_or_default())
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut items: Vec<ConversationView> = rows.into_iter().map(conv_view).collect();
    let next_cursor = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        let last = items.last().expect("one item");
        Some(
            cursor::encode(
                &app.pool,
                Some(user),
                &hash,
                Position::Keyset {
                    t: last.last_at,
                    i: last.id,
                },
            )
            .await?,
        )
    } else {
        None
    };
    Ok(ConversationList { items, next_cursor })
}

fn check_title(t: Option<&str>) -> anyhow::Result<String> {
    let t = t.unwrap_or("").trim().to_string();
    if t.chars().count() > MAX_TITLE || t.chars().any(|c| c.is_control()) {
        return Err(ApiError::new(
            ErrorCode::InvalidValue,
            "the title is at most 120 characters on one line",
        )
        .with_details(json!({"path": "title"}))
        .into());
    }
    Ok(t)
}

/// `POST /ai/conversations` → 201, bound to the current active context.
pub async fn create(
    app: &App,
    ctx: AuthContext,
    input: &NewConversation,
) -> anyhow::Result<ConversationView> {
    let (user, _) = ctx.require_human_session()?;
    let title = check_title(input.title.as_deref())?;
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    require_enabled(&mut tx).await?;
    let a = ensure_assistant(&mut tx, &ctx).await?;
    let row: ConvRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH c AS (
           INSERT INTO ai_conversations (user_id, agent_principal_id, runtime_id, context_id, title, purge_after)
           SELECT $1, $2, $3, $4, $5, now() + make_interval(days => s.conversation_retention_days)
           FROM ai_settings s RETURNING *)
         {} ",
        CONV_SELECT.replace("FROM ai_conversations c", "FROM c")
    )))
    .bind(user)
    .bind(a.principal)
    .bind(a.runtime)
    .bind(a.context)
    .bind(&title)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut v = conv_view(row);
    v.state = "open".into();
    Ok(v)
}

/// (agent principal, context, open, last provider, its config time then).
type ConvLock = (Uuid, Uuid, bool, Option<Uuid>, Option<DateTime<Utc>>);

/// The caller's live conversation, locked; otherwise `not_found`.
async fn own_conversation(
    conn: &mut PgConnection,
    user: Uuid,
    id: Uuid,
) -> anyhow::Result<ConvLock> {
    let row: Option<ConvLock> = sqlx::query_as(
        "SELECT c.agent_principal_id, c.context_id,
                (c.state = 'open' AND EXISTS (SELECT 1 FROM agent_work_contexts w
                                               WHERE w.id = c.context_id AND w.state = 'active')),
                c.last_provider_id, c.last_provider_config_at
         FROM ai_conversations c
         WHERE c.id = $1 AND c.user_id = $2 AND c.deleted_at IS NULL AND c.purged_at IS NULL
         FOR UPDATE OF c",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *conn)
    .await?;
    row.ok_or_else(not_found)
}

/// Empties a conversation's content in the caller's transaction (deletion
/// and retention, §3.13.5); runs, their counts and costs stay.
async fn purge_content(conn: &mut PgConnection, conversation: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE ai_messages SET parts = NULL, page_context = NULL, purged_at = now()
         WHERE conversation_id = $1 AND purged_at IS NULL",
    )
    .bind(conversation)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `DELETE /ai/conversations/{id}`: not undoable; the writes the AI made
/// stay (they are ordinary executions).
pub async fn delete(app: &App, ctx: AuthContext, id: Uuid) -> anyhow::Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    own_conversation(&mut tx, user, id).await?;
    // A run still going stops; it never writes into a deleted conversation.
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ai_runs WHERE conversation_id = $1 AND state IN ('queued','running')",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    purge_content(&mut tx, id).await?;
    sqlx::query("UPDATE ai_conversations SET title = '', deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(r) = active {
        sqlx::query("UPDATE ai_runs SET cancel_requested_at = coalesce(cancel_requested_at, now()) WHERE id = $1")
            .bind(r)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    if let Some(r) = active {
        runner::signal_cancel(r);
    }
    Ok(())
}

// ---------------------------------------------------------------- re-checks

/// What a stored tool result or page context drew from.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sources {
    pub projects: Vec<Uuid>,
    pub issues: Vec<Uuid>,
}

/// Can `scope` read all of `s` now? (§3.13.3 point 3.)
pub(crate) async fn readable(
    conn: &mut PgConnection,
    scope: &Scope,
    s: &Sources,
) -> anyhow::Result<bool> {
    let projects: HashSet<Uuid> = scope.project_ids().collect();
    if s.projects.iter().any(|p| !projects.contains(p)) {
        return Ok(false);
    }
    if s.issues.is_empty() {
        return Ok(true);
    }
    type F = (Uuid, Uuid, Uuid, bool, Uuid, Option<Uuid>);
    let facts: Vec<F> = sqlx::query_as(
        "SELECT id, project_id, tracker_id, is_private, author_id, assigned_to_id FROM issues WHERE id = ANY($1)",
    )
    .bind(&s.issues)
    .fetch_all(&mut *conn)
    .await?;
    if facts.len() != s.issues.len() {
        return Ok(false);
    }
    Ok(facts.into_iter().all(
        |(_, project_id, tracker_id, is_private, author_id, assigned_to_id)| {
            scope.can_see(&IssueFacts {
                project_id,
                tracker_id,
                is_private,
                author_id,
                assigned_to_id,
            })
        },
    ))
}

/// One stored message.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct MsgRow {
    pub id: Uuid,
    pub seq: i32,
    pub role: String,
    pub run_id: Option<Uuid>,
    pub parts: Option<Json>,
    pub source_projects: Vec<Uuid>,
    pub time_tainted: bool,
    pub page_context: Option<Json>,
    pub created_at: DateTime<Utc>,
}

pub(crate) async fn messages_of(
    conn: &mut PgConnection,
    conversation: Uuid,
) -> anyhow::Result<Vec<MsgRow>> {
    load_messages(conn, conversation).await
}

async fn load_messages(conn: &mut PgConnection, conversation: Uuid) -> anyhow::Result<Vec<MsgRow>> {
    Ok(sqlx::query_as(
        "SELECT id, seq, role, run_id, parts, source_projects, time_tainted, page_context, created_at
         FROM ai_messages WHERE conversation_id = $1 ORDER BY seq",
    )
    .bind(conversation)
    .fetch_all(&mut *conn)
    .await?)
}

/// The sources a tool-role message records.
pub(crate) fn sources_of(m: &MsgRow) -> Sources {
    let mut issues = Vec::new();
    if let Some(parts) = m.parts.as_ref().and_then(Json::as_array) {
        for p in parts {
            if let Some(a) = p["issues"].as_array() {
                issues.extend(a.iter().filter_map(|v| v.as_str()?.parse::<Uuid>().ok()));
            }
        }
    }
    Sources {
        projects: m.source_projects.clone(),
        issues,
    }
}

/// Runs whose tool results (or page contexts) `scope` can no longer read
/// all of, and those messages themselves. A tool message already emptied
/// (D12, purge) counts as unreadable too: its run's replies drew on it.
pub(crate) async fn unreadable(
    conn: &mut PgConnection,
    scope: &Scope,
    msgs: &[MsgRow],
) -> anyhow::Result<(HashSet<Uuid>, HashSet<Uuid>)> {
    let mut runs = HashSet::new();
    let mut ids = HashSet::new();
    for m in msgs.iter().filter(|m| m.role == "tool") {
        if m.parts.is_none() || !readable(&mut *conn, scope, &sources_of(m)).await? {
            ids.insert(m.id);
            if let Some(r) = m.run_id {
                runs.insert(r);
            }
        }
    }
    Ok((runs, ids))
}

// ---------------------------------------------------------------- reading messages

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MessageView {
    pub id: Uuid,
    pub seq: i32,
    /// `user` or `assistant` (tool results are never returned).
    pub role: String,
    pub run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Typed parts (§3.13.5): `text {text}`, `tool_progress {tool, summary,
    /// ok}`, `write {…as the SSE write event}`, `unavailable {reason:
    /// source_revoked|redacted|purged}`, `notice {kind, text}`.
    #[schemars(with = "Vec<serde_json::Value>")]
    pub parts: Vec<Json>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MessageList {
    pub conversation: ConversationView,
    pub items: Vec<MessageView>,
}

/// `GET /ai/conversations/{id}/messages`: re-checked for the reader now,
/// with the scope an agent of theirs would have in every project they can
/// read (the user's own view is masked too, §3.13.5).
pub async fn messages(app: &App, ctx: AuthContext, id: Uuid) -> anyhow::Result<MessageList> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let me = db::effective(&mut tx, ctx).await?;
    let row: Option<ConvRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{CONV_SELECT} WHERE c.id = $1 AND c.user_id = $2 AND c.deleted_at IS NULL"
    )))
    .bind(id)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(not_found());
    };
    let conv_purged: bool =
        sqlx::query_scalar("SELECT purged_at IS NOT NULL FROM ai_conversations WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let msgs = load_messages(&mut tx, id).await?;
    let scope = db::agent_scope(&mut tx, user, me.is_admin, None).await?;
    let (bad_runs, _) = unreadable(&mut tx, &scope, &msgs).await?;
    tx.commit().await?;
    let mut items = Vec::new();
    for m in msgs.into_iter().filter(|m| m.role != "tool") {
        let parts = match &m.parts {
            None => vec![json!({"type": "unavailable",
                                 "reason": if conv_purged { "purged" } else { "redacted" }})],
            Some(p) => {
                let all = p.as_array().cloned().unwrap_or_default();
                if m.role == "assistant" && m.run_id.is_some_and(|r| bad_runs.contains(&r)) {
                    let mut v = vec![json!({"type": "unavailable", "reason": "source_revoked"})];
                    v.extend(all.into_iter().filter(|x| x["type"] == "notice"));
                    v
                } else {
                    all.into_iter()
                        .filter(|x| x["type"] != "tool_call")
                        .collect()
                }
            }
        };
        items.push(MessageView {
            id: m.id,
            seq: m.seq,
            role: m.role,
            run_id: m.run_id,
            created_at: m.created_at,
            parts,
        });
    }
    Ok(MessageList {
        conversation: conv_view(row),
        items,
    })
}

// ---------------------------------------------------------------- sending

/// A page the user is on: a reference, never its text (§3.13.3 point 2).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageRef {
    Issue { key: String },
    View { id: String },
    Project { key: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendInput {
    /// 1–8000 characters typed by the user.
    pub text: String,
    /// At most 5 references to the pages the user is on.
    #[serde(default)]
    pub page_context: Vec<PageRef>,
    /// The active provider's id, after the user confirmed sending this
    /// conversation to it (`ai_provider_changed`, Q4).
    #[serde(default)]
    pub ack_provider_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Accepted {
    pub run_id: Uuid,
    /// `GET` this with `Accept: text/event-stream`.
    pub stream_url: String,
}

const ACTION_SEND: &str = "ai_message_send";

/// `POST /ai/conversations/{id}/messages` → 202 (§3.13.4). Checks in the
/// contract's order, reserves the budget atomically, stores the user's
/// message and a queued run, then starts the run in this process.
pub async fn send(
    app: &Arc<App>,
    ctx: AuthContext,
    operation_id: Uuid,
    id: Uuid,
    input: &SendInput,
    spa_origin: &str,
) -> anyhow::Result<Accepted> {
    let who = crate::commands::executor(&ctx)?;
    let user = who.user_id;
    let n = input.text.chars().count();
    if n == 0 || n > MAX_TEXT || input.text.trim().is_empty() {
        return Err(
            ApiError::new(ErrorCode::InvalidValue, "the message is 1–8000 characters")
                .with_details(json!({"path": "text"}))
                .into(),
        );
    }
    if input.page_context.len() > MAX_PAGE_REFS {
        return Err(
            ApiError::new(ErrorCode::InvalidValue, "at most 5 page references")
                .with_details(json!({"path": "page_context"}))
                .into(),
        );
    }
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    let fp = crate::commands::fingerprint(
        ACTION_SEND,
        &serde_json::to_vec(&json!({"c": id, "i": input}))?,
    );
    let Some(request_id) =
        crate::commands::open_request(&mut tx, user, operation_id, ACTION_SEND, &fp).await?
    else {
        let r =
            crate::commands::stored_result(&mut tx, user, operation_id, ACTION_SEND, &fp).await?;
        tx.commit().await?;
        return Ok(serde_json::from_value(r)?);
    };
    // 1. AI enabled, the active provider usable.
    require_enabled(&mut tx).await?;
    let provider = super::admin::active_provider(&mut tx).await?;
    // 2. Mine and open.
    let (_, _, open, last_provider, last_config) = own_conversation(&mut tx, user, id).await?;
    if !open {
        return Err(api(
            ErrorCode::AiConversationReadOnly,
            "this conversation is archived (the AI's work context was replaced); start a new one",
        ));
    }
    // 3. Q4: the provider changed since this conversation's last run.
    let has_messages: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ai_messages WHERE conversation_id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    // The same provider with a new endpoint or model counts as a change.
    let changed = has_messages
        && (last_provider != Some(provider.id)
            || last_config.is_some_and(|t| t != provider.config_changed_at));
    if changed && input.ack_provider_id != Some(provider.id) {
        return Err(ApiError::new(
            ErrorCode::AiProviderChanged,
            "the AI service changed; confirm before this conversation is sent to it",
        )
        .with_details(
            json!({"provider": {"id": provider.id, "name": provider.name,
                                          "kind": provider.kind, "model": provider.model}}),
        )
        .into());
    }
    // 4. One run at a time.
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM ai_runs WHERE user_id = $1 AND state IN ('queued','running'))",
    )
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(api(
            ErrorCode::AiRunInProgress,
            "a reply is still being written; wait or stop it",
        ));
    }
    // 5, 6. The hourly rate, then the atomic budget reservation.
    let reserved = super::budget::reserve(&mut tx, user, &provider).await?;
    // 7. The run and the user's message.
    let run: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_runs (conversation_id, user_id, provider_id, model, prompt_version, state,
                              lease_until, reserved_usd)
         VALUES ($1, $2, $3, $4, $5, 'queued', clock_timestamp() + make_interval(secs => $6), $7::numeric)
         RETURNING id",
    )
    .bind(id)
    .bind(user)
    .bind(provider.id)
    .bind(&provider.model)
    .bind(super::PROMPT_VERSION)
    .bind(super::config::current().limits.lease.as_secs_f64())
    .bind(&reserved)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => {
            api(ErrorCode::AiRunInProgress, "a reply is still being written; wait or stop it")
        }
        _ => e.into(),
    })?;
    if changed {
        // Q4: shown before the reply (SSE `notice` and a stored part).
        let text = format!(
            "AI 服務已換成〈{}（{}）〉；先前的對話內容已送到新服務。",
            provider.name, provider.model
        );
        sqlx::query(
            "INSERT INTO ai_messages (conversation_id, run_id, seq, role, parts)
             SELECT $1, $2, coalesce(max(seq), 0) + 1, 'assistant', $3 FROM ai_messages WHERE conversation_id = $1",
        )
        .bind(id)
        .bind(run)
        .bind(json!([{"type": "notice", "kind": "provider_changed", "text": text}]))
        .execute(&mut *tx)
        .await?;
    }
    let page = if input.page_context.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&input.page_context)?)
    };
    sqlx::query(
        "INSERT INTO ai_messages (conversation_id, run_id, seq, role, parts, page_context)
         SELECT $1, NULL, coalesce(max(seq), 0) + 1, 'user', $2, $3 FROM ai_messages WHERE conversation_id = $1",
    )
    .bind(id)
    .bind(json!([{"type": "text", "text": input.text}]))
    .bind(page)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE ai_conversations c SET last_at = now(), last_provider_id = $2,
                last_provider_config_at = $3,
                purge_after = now() + make_interval(days => s.conversation_retention_days)
         FROM ai_settings s WHERE c.id = $1",
    )
    .bind(id)
    .bind(provider.id)
    .bind(provider.config_changed_at)
    .execute(&mut *tx)
    .await?;
    let accepted = Accepted {
        run_id: run,
        stream_url: format!("/api/v1/ai/runs/{run}/stream"),
    };
    crate::commands::complete_request(&mut tx, request_id, &serde_json::to_value(&accepted)?)
        .await?;
    tx.commit().await?;
    runner::start(app.clone(), run, spa_origin.to_string());
    Ok(accepted)
}

// ---------------------------------------------------------------- D12 and retention

/// D12 (§3.13.7): messages drawn from `project` lose their content in the
/// caller's transaction (by project, erring wide).
pub(crate) async fn redact_project(conn: &mut PgConnection, project: Uuid) -> anyhow::Result<u64> {
    // The assistant messages of every run that drew on the project go too,
    // whatever their own sources say (a reply may repeat a page context).
    let replies = sqlx::query(
        "UPDATE ai_messages SET parts = NULL, page_context = NULL, purged_at = now()
         WHERE role = 'assistant' AND purged_at IS NULL
           AND run_id IN (SELECT run_id FROM ai_messages
                          WHERE $1 = ANY(source_projects) AND run_id IS NOT NULL)",
    )
    .bind(project)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(replies
        + sqlx::query(
            "UPDATE ai_messages SET parts = NULL, page_context = NULL, purged_at = now()
             WHERE $1 = ANY(source_projects) AND purged_at IS NULL",
        )
        .bind(project)
        .execute(&mut *conn)
        .await?
        .rows_affected())
}

/// The daily retention job (§3.13.5): conversations past `purge_after`
/// lose their content and title; 90 days after that the rows go (messages
/// by cascade; runs keep their counts and costs, `conversation_id` NULL).
pub async fn retention(pool: &sqlx::PgPool) -> anyhow::Result<(u64, u64)> {
    let mut tx = pool.begin().await?;
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ai_conversations WHERE purged_at IS NULL AND purge_after <= now()
         ORDER BY id LIMIT 1000 FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;
    for c in &due {
        purge_content(&mut tx, *c).await?;
    }
    sqlx::query("UPDATE ai_conversations SET title = '', purged_at = now() WHERE id = ANY($1)")
        .bind(&due)
        .execute(&mut *tx)
        .await?;
    let gone = sqlx::query(
        "DELETE FROM ai_conversations
         WHERE coalesce(purged_at, deleted_at) <= now() - interval '90 days'
           AND NOT EXISTS (SELECT 1 FROM ai_runs r WHERE r.conversation_id = ai_conversations.id
                             AND r.state IN ('queued','running'))",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((due.len() as u64, gone))
}

/// Stops the user's runs and conversations when they leave (§3.13.7): a
/// run in progress is canceled; conversations stay until retention.
pub async fn on_user_deactivated(pool: &sqlx::PgPool, user: Uuid) -> anyhow::Result<()> {
    let runs: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE ai_runs SET cancel_requested_at = coalesce(cancel_requested_at, now())
         WHERE user_id = $1 AND state IN ('queued','running') RETURNING id",
    )
    .bind(user)
    .fetch_all(pool)
    .await?;
    for r in runs {
        runner::signal_cancel(r);
    }
    Ok(())
}
