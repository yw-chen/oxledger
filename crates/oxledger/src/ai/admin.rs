// SPDX-License-Identifier: GPL-2.0-or-later
//! The AI settings page (administrators, design 05 §3.13.2, §6.20): site
//! settings and budgets, the endpoints the deployment registered, providers
//! (D17, P-3, P-6), "test connection", usage and the audit trail.
//!
//! The web side never names a URL, a key or an environment variable (Q1):
//! a provider names a registered endpoint. Keys never leave the process.

use super::client::{self, CallError};
use super::config::{self, AiConfig, Endpoint};
use super::net::{self, AddrClass};
use super::protocol::{Block, Kind, ModelRequest, Msg, Role, ToolSpec};
use crate::auth::{self, AuthContext, SessionPolicy};
use crate::db;
use crate::error::{ApiError, ErrorCode};
use crate::service::App;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";

async fn require_admin(conn: &mut PgConnection, ctx: AuthContext) -> anyhow::Result<Uuid> {
    let (user, _) = ctx.require_human_session()?;
    let me = db::effective(&mut *conn, ctx).await?;
    if !me.is_admin || me.agent.is_some() {
        return Err(
            ApiError::new(ErrorCode::ForbiddenOp, "AI settings are for administrators")
                .with_details(json!({"reason": "admin"}))
                .into(),
        );
    }
    Ok(user)
}

fn invalid(path: &str, kind: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg.to_string())
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn endpoint_invalid(kind: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::AiEndpointInvalid, msg.to_string())
        .with_details(json!({"kind": kind}))
        .into()
}

/// `^\d{1,int}(\.\d{1,frac})?$`: money as text (no decimal crate).
fn check_money(path: &str, v: &str, int: usize, frac: usize, positive: bool) -> anyhow::Result<()> {
    let (i, f) = v.split_once('.').unwrap_or((v, ""));
    let ok = !i.is_empty()
        && i.len() <= int
        && i.bytes().all(|b| b.is_ascii_digit())
        && (!v.contains('.') || (!f.is_empty() && f.len() <= frac))
        && f.bytes().all(|b| b.is_ascii_digit())
        && (!positive || v.bytes().any(|b| (b'1'..=b'9').contains(&b)));
    if !ok {
        return Err(invalid(
            path,
            "amount",
            "a decimal amount in USD, e.g. 3.00",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- providers (shared with runs)

/// The active provider, as a run uses it.
#[derive(Debug, Clone)]
pub(crate) struct Provider {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub endpoint_name: String,
    pub model: String,
    /// When its endpoint or model last changed (Q4 compares it).
    pub config_changed_at: DateTime<Utc>,
}

type ProviderRow = (Uuid, String, String, String, String, DateTime<Utc>);

/// The active, unretired provider whose endpoint is registered (with a key
/// for Anthropic) and whose breaker is closed; otherwise `ai_unavailable`.
pub(crate) async fn active_provider(conn: &mut PgConnection) -> anyhow::Result<Provider> {
    let row: Option<ProviderRow> = sqlx::query_as(
        "SELECT p.id, p.name, p.kind, p.endpoint_name, p.model, p.config_changed_at
         FROM ai_settings s JOIN ai_providers p ON p.id = s.active_provider_id AND p.retired_at IS NULL",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, name, kind, endpoint_name, model, config_changed_at)) = row else {
        return Err(super::chat::unavailable(
            "not_configured",
            "no AI provider is in use",
        ));
    };
    let p = Provider {
        id,
        name,
        kind,
        endpoint_name,
        model,
        config_changed_at,
    };
    let cfg = config::current();
    let usable = cfg.endpoints.get(&p.endpoint_name).is_some_and(|ep| {
        Kind::parse(&p.kind).is_some_and(|k| k != Kind::Anthropic || ep.has_key())
    });
    if !usable || cfg.model_disallowed(&p.model) {
        return Err(super::chat::unavailable(
            "not_configured",
            "the AI provider in use is not configured in this server",
        ));
    }
    if super::runner::breaker_open(p.id, &p.model) {
        return Err(super::chat::unavailable(
            "circuit_open",
            "the AI service is failing; try again later",
        ));
    }
    Ok(p)
}

// ---------------------------------------------------------------- endpoints

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EndpointView {
    pub name: String,
    /// `scheme://host[:port]`, never the path's secrets (there are none).
    pub origin: String,
    /// Where it resolves now: `public`, `private`, `loopback`, `blocked`;
    /// null when it does not resolve.
    pub address_class: Option<AddrClass>,
    /// Whether `OXLEDGER_AI_KEY_<NAME>` was set at start-up. Never the key.
    pub key_configured: bool,
    /// An internal CA was loaded (`OXLEDGER_AI_CA_FILE_<NAME>`).
    pub custom_ca: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EndpointList {
    pub items: Vec<EndpointView>,
    /// `OXLEDGER_AI_ALLOW_HTTP` (http to private and loopback addresses).
    pub allow_http: bool,
    /// Whether this build can reach providers at all (feature `ai`).
    pub transport: bool,
}

async fn endpoint_view(cfg: &AiConfig, ep: &Endpoint) -> EndpointView {
    EndpointView {
        name: ep.name.clone(),
        origin: ep.origin(),
        address_class: net::class_of(cfg.resolver.as_ref(), &ep.host, ep.port).await,
        key_configured: ep.has_key(),
        custom_ca: !ep.ca_der.is_empty(),
    }
}

/// `GET /admin/ai/endpoints`.
pub async fn endpoints(app: &App, ctx: AuthContext) -> anyhow::Result<EndpointList> {
    let mut conn = app.pool.acquire().await?;
    require_admin(&mut conn, ctx).await?;
    drop(conn);
    let cfg = config::current();
    let mut items = Vec::new();
    for ep in cfg.endpoints.values() {
        items.push(endpoint_view(&cfg, ep).await);
    }
    Ok(EndpointList {
        items,
        allow_http: cfg.allow_http,
        transport: client::TRANSPORT,
    })
}

/// The registered endpoint for `name` and `kind`, checked as P-3 and
/// §5 `ai_endpoint_invalid` say. Returns where it resolves (None: does not
/// resolve now).
async fn check_endpoint(name: &str, kind: Kind) -> anyhow::Result<Option<AddrClass>> {
    let cfg = config::current();
    let Some(ep) = cfg.endpoints.get(name) else {
        return Err(endpoint_invalid(
            "unregistered",
            "this endpoint is not registered in the server's settings",
        ));
    };
    if kind == Kind::Anthropic && !ep.has_key() {
        return Err(endpoint_invalid(
            "no_key",
            "Anthropic needs a key: set OXLEDGER_AI_KEY_<NAME> on the server",
        ));
    }
    match net::pick(
        cfg.resolver.as_ref(),
        &ep.host,
        ep.port,
        ep.https,
        cfg.allow_http,
    )
    .await
    {
        Err(net::Refused::Blocked) => Err(endpoint_invalid(
            "endpoint_blocked",
            "the endpoint resolves to an address that is not allowed",
        )),
        Err(net::Refused::Unresolved) => Ok(None),
        Ok(_) => Ok(net::class_of(cfg.resolver.as_ref(), &ep.host, ep.port).await),
    }
}

// ---------------------------------------------------------------- provider CRUD

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderView {
    pub id: Uuid,
    pub name: String,
    /// `anthropic` or `openai_compat`.
    pub kind: String,
    pub endpoint_name: String,
    /// Whether the endpoint is still registered in this process.
    pub endpoint_registered: bool,
    pub model: String,
    pub max_output_tokens: i32,
    pub input_usd_per_mtok: String,
    pub output_usd_per_mtok: String,
    pub supports_tools: bool,
    pub last_test_at: Option<DateTime<Utc>>,
    pub last_test_ok_at: Option<DateTime<Utc>>,
    /// `{ok, kind?, status?, supports_tools?}`; never provider text.
    #[schemars(with = "Option<serde_json::Value>")]
    pub last_test_result: Option<Json>,
    pub d17_attested_at: DateTime<Utc>,
    pub org_account_attested_at: Option<DateTime<Utc>>,
    pub config_changed_at: DateTime<Utc>,
    pub active: bool,
    pub retired_at: Option<DateTime<Utc>>,
    pub lock_version: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderList {
    pub items: Vec<ProviderView>,
}

#[derive(sqlx::FromRow)]
struct ViewRow {
    id: Uuid,
    name: String,
    kind: String,
    endpoint_name: String,
    model: String,
    max_output_tokens: i32,
    input_usd_per_mtok: String,
    output_usd_per_mtok: String,
    supports_tools: bool,
    last_test_at: Option<DateTime<Utc>>,
    last_test_ok_at: Option<DateTime<Utc>>,
    last_test_result: Option<Json>,
    d17_attested_at: DateTime<Utc>,
    org_account_attested_at: Option<DateTime<Utc>>,
    config_changed_at: DateTime<Utc>,
    active: bool,
    retired_at: Option<DateTime<Utc>>,
    lock_version: i32,
}

const VIEW_SELECT: &str = "SELECT p.id, p.name, p.kind, p.endpoint_name, p.model, p.max_output_tokens,
        p.input_usd_per_mtok::text AS input_usd_per_mtok, p.output_usd_per_mtok::text AS output_usd_per_mtok,
        p.supports_tools, p.last_test_at, p.last_test_ok_at, p.last_test_result, p.d17_attested_at,
        p.org_account_attested_at, p.config_changed_at,
        EXISTS (SELECT 1 FROM ai_settings s WHERE s.active_provider_id = p.id) AS active,
        p.retired_at, p.lock_version
 FROM ai_providers p";

fn view(r: ViewRow) -> ProviderView {
    let cfg = config::current();
    ProviderView {
        endpoint_registered: cfg.endpoints.contains_key(&r.endpoint_name),
        id: r.id,
        name: r.name,
        kind: r.kind,
        endpoint_name: r.endpoint_name,
        model: r.model,
        max_output_tokens: r.max_output_tokens,
        input_usd_per_mtok: r.input_usd_per_mtok,
        output_usd_per_mtok: r.output_usd_per_mtok,
        supports_tools: r.supports_tools,
        last_test_at: r.last_test_at,
        last_test_ok_at: r.last_test_ok_at,
        last_test_result: r.last_test_result,
        d17_attested_at: r.d17_attested_at,
        org_account_attested_at: r.org_account_attested_at,
        config_changed_at: r.config_changed_at,
        active: r.active,
        retired_at: r.retired_at,
        lock_version: r.lock_version,
    }
}

async fn provider_view(conn: &mut PgConnection, id: Uuid) -> anyhow::Result<ProviderView> {
    let r: ViewRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{VIEW_SELECT} WHERE p.id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(view(r))
}

/// `GET /admin/ai/providers`.
pub async fn providers(app: &App, ctx: AuthContext) -> anyhow::Result<ProviderList> {
    let mut conn = app.pool.acquire().await?;
    require_admin(&mut conn, ctx).await?;
    let rows: Vec<ViewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{VIEW_SELECT} ORDER BY p.retired_at IS NOT NULL, p.created_at, p.id"
    )))
    .fetch_all(&mut *conn)
    .await?;
    Ok(ProviderList {
        items: rows.into_iter().map(view).collect(),
    })
}

/// `POST /admin/ai/providers`. Unknown fields (a URL, a key, an environment
/// variable name) are refused by the schema (T-M1-112).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewProvider {
    /// 1–60 characters, shown to users ("AI 服務已換成〈name〉").
    pub name: String,
    /// `anthropic` or `openai_compat`.
    pub kind: String,
    /// A name from `GET /admin/ai/endpoints`.
    pub endpoint_name: String,
    /// Anthropic: default `claude-sonnet-5-5`; OpenAI-compatible: required.
    #[serde(default)]
    pub model: Option<String>,
    /// The run's total output limit (256–32000, default 4096).
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    /// USD per million input tokens, e.g. "3.00".
    pub input_usd_per_mtok: String,
    pub output_usd_per_mtok: String,
    /// P-1: 「我確認這個模型與服務符合 D17」. Must be true.
    pub d17_attested: bool,
    /// P-6: required when the endpoint resolves to a public address.
    #[serde(default)]
    pub org_account_attested: bool,
}

/// `PATCH /admin/ai/providers/{id}`: the fields to change; the D17
/// confirmation is given again with every change.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    pub lock_version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub endpoint_name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    #[serde(default)]
    pub input_usd_per_mtok: Option<String>,
    #[serde(default)]
    pub output_usd_per_mtok: Option<String>,
    pub d17_attested: bool,
    #[serde(default)]
    pub org_account_attested: bool,
}

fn check_name(name: &str) -> anyhow::Result<()> {
    let n = name.trim().chars().count();
    if n == 0 || n > 60 || name.chars().any(char::is_control) {
        return Err(invalid("name", "length", "1–60 characters on one line"));
    }
    Ok(())
}

fn check_model(cfg: &AiConfig, model: &str) -> anyhow::Result<()> {
    let n = model.trim().chars().count();
    if n == 0 || n > 200 || model.chars().any(char::is_control) {
        return Err(invalid("model", "length", "1–200 characters"));
    }
    if cfg.model_disallowed(model) {
        return Err(ApiError::new(
            ErrorCode::AiModelDisallowed,
            "this model is on the D17 list (models and services of Chinese companies)",
        )
        .with_details(json!({"field": "model"}))
        .into());
    }
    Ok(())
}

fn check_max_out(v: Option<i32>) -> anyhow::Result<i32> {
    let v = v.unwrap_or(4096);
    if !(256..=32000).contains(&v) {
        return Err(invalid(
            "max_output_tokens",
            "range",
            "between 256 and 32000",
        ));
    }
    Ok(v)
}

/// P-6: an external service (a public address, or one that does not
/// resolve now) needs the organisation-account confirmation.
fn needs_org_attest(class: Option<AddrClass>) -> bool {
    !matches!(class, Some(AddrClass::Private | AddrClass::Loopback))
}

async fn event(
    conn: &mut PgConnection,
    provider: Option<Uuid>,
    actor: Uuid,
    kind: &str,
    payload: Json,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO ai_provider_events (provider_id, actor_id, kind, payload) VALUES ($1, $2, $3, $4)")
        .bind(provider)
        .bind(actor)
        .bind(kind)
        .bind(payload)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn create_provider(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    input: &NewProvider,
) -> anyhow::Result<ProviderView> {
    let cfg = config::current();
    check_name(&input.name)?;
    let Some(kind) = Kind::parse(&input.kind) else {
        return Err(invalid("kind", "allowed", "anthropic or openai_compat"));
    };
    let model = match (&input.model, kind) {
        (Some(m), _) => m.trim().to_string(),
        (None, Kind::Anthropic) => DEFAULT_ANTHROPIC_MODEL.to_string(),
        (None, Kind::OpenAiCompat) => {
            return Err(invalid("model", "required", "the model is required"));
        }
    };
    check_model(&cfg, &model)?;
    let max_out = check_max_out(input.max_output_tokens)?;
    check_money("input_usd_per_mtok", &input.input_usd_per_mtok, 6, 4, false)?;
    check_money(
        "output_usd_per_mtok",
        &input.output_usd_per_mtok,
        6,
        4,
        false,
    )?;
    if !input.d17_attested {
        return Err(invalid(
            "d17_attested",
            "attest_required",
            "confirm that the model and service comply with D17",
        ));
    }
    let class = check_endpoint(&input.endpoint_name, kind).await?;
    if needs_org_attest(class) && !input.org_account_attested {
        return Err(invalid(
            "org_account_attested",
            "attest_required",
            "confirm that this external service uses the organisation's key",
        ));
    }
    let mut tx = app.pool.begin().await?;
    let user = require_admin(&mut tx, ctx).await?;
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_providers (name, kind, endpoint_name, model, max_output_tokens,
                                   input_usd_per_mtok, output_usd_per_mtok, d17_attested_by, d17_attested_at,
                                   org_account_attested_by, org_account_attested_at)
         VALUES ($1, $2, $3, $4, $5, $6::numeric, $7::numeric, $8, now(),
                 CASE WHEN $9 THEN $8 END, CASE WHEN $9 THEN now() END)
         RETURNING id",
    )
    .bind(input.name.trim())
    .bind(&input.kind)
    .bind(&input.endpoint_name)
    .bind(&model)
    .bind(max_out)
    .bind(&input.input_usd_per_mtok)
    .bind(&input.output_usd_per_mtok)
    .bind(user)
    .bind(input.org_account_attested)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => {
            invalid("name", "taken", "another provider has this name")
        }
        _ => e.into(),
    })?;
    event(
        &mut tx,
        Some(id),
        user,
        "created",
        json!({"name": input.name.trim(), "kind": input.kind, "endpoint_name": input.endpoint_name,
               "model": model, "max_output_tokens": max_out,
               "input_usd_per_mtok": input.input_usd_per_mtok, "output_usd_per_mtok": input.output_usd_per_mtok,
               "org_account_attested": input.org_account_attested}),
    )
    .await?;
    let v = provider_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(v)
}

fn not_found() -> anyhow::Error {
    crate::error::api(ErrorCode::NotFound, "no such AI provider")
}

pub async fn update_provider(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
    input: &ProviderPatch,
) -> anyhow::Result<ProviderView> {
    let cfg = config::current();
    if !input.d17_attested {
        return Err(invalid(
            "d17_attested",
            "attest_required",
            "confirm that the model and service comply with D17",
        ));
    }
    let mut tx = app.pool.begin().await?;
    let user = require_admin(&mut tx, ctx).await?;
    let cur: Option<(String, String, String, i32, bool)> = sqlx::query_as(
        "SELECT kind, endpoint_name, model, lock_version, retired_at IS NOT NULL FROM ai_providers WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((kind_s, endpoint, model, lock, retired)) = cur else {
        return Err(not_found());
    };
    if retired {
        return Err(invalid("id", "retired", "this provider was retired"));
    }
    if lock != input.lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the provider changed elsewhere; reload it",
        )
        .with_details(json!({"current_lock_version": lock}))
        .into());
    }
    let kind = Kind::parse(&kind_s).unwrap_or(Kind::OpenAiCompat);
    if let Some(n) = &input.name {
        check_name(n)?;
    }
    let new_endpoint = input.endpoint_name.clone().unwrap_or(endpoint.clone());
    let new_model = input
        .model
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or(model.clone());
    check_model(&cfg, &new_model)?;
    let max_out = match input.max_output_tokens {
        Some(v) => Some(check_max_out(Some(v))?),
        None => None,
    };
    if let Some(v) = &input.input_usd_per_mtok {
        check_money("input_usd_per_mtok", v, 6, 4, false)?;
    }
    if let Some(v) = &input.output_usd_per_mtok {
        check_money("output_usd_per_mtok", v, 6, 4, false)?;
    }
    let class = check_endpoint(&new_endpoint, kind).await?;
    let changed_config = new_endpoint != endpoint || new_model != model;
    let org_attested: bool = sqlx::query_scalar(
        "SELECT org_account_attested_at IS NOT NULL FROM ai_providers WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    // A different endpoint needs the confirmation again (P-6).
    let keep_org = org_attested && new_endpoint == endpoint;
    if needs_org_attest(class) && !(keep_org || input.org_account_attested) {
        return Err(invalid(
            "org_account_attested",
            "attest_required",
            "confirm that this external service uses the organisation's key",
        ));
    }
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    sqlx::query(
        "UPDATE ai_providers SET
            name = coalesce($2, name), endpoint_name = $3, model = $4,
            max_output_tokens = coalesce($5, max_output_tokens),
            input_usd_per_mtok = coalesce($6::numeric, input_usd_per_mtok),
            output_usd_per_mtok = coalesce($7::numeric, output_usd_per_mtok),
            d17_attested_by = $8, d17_attested_at = now(),
            org_account_attested_by = CASE WHEN $9 THEN $8 WHEN $10 THEN org_account_attested_by END,
            org_account_attested_at = CASE WHEN $9 THEN now() WHEN $10 THEN org_account_attested_at END,
            config_changed_at = CASE WHEN $11 THEN now() ELSE config_changed_at END,
            lock_version = lock_version + 1, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(input.name.as_deref().map(str::trim))
    .bind(&new_endpoint)
    .bind(&new_model)
    .bind(max_out)
    .bind(&input.input_usd_per_mtok)
    .bind(&input.output_usd_per_mtok)
    .bind(user)
    .bind(input.org_account_attested)
    .bind(keep_org)
    .bind(changed_config)
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => {
            invalid("name", "taken", "another provider has this name")
        }
        _ => e.into(),
    })?;
    event(
        &mut tx,
        Some(id),
        user,
        "updated",
        json!({"name": input.name, "endpoint_name": new_endpoint, "model": new_model,
               "max_output_tokens": max_out, "input_usd_per_mtok": input.input_usd_per_mtok,
               "output_usd_per_mtok": input.output_usd_per_mtok, "config_changed": changed_config}),
    )
    .await?;
    // §3.13.2: `enabled` needs a test that succeeded after the provider's
    // last change. A new endpoint or model of the provider in use turns the
    // AI off until it is tested and switched on again; conversations then
    // need the user's confirmation (Q4, the conversation records the
    // provider's `config_changed_at`).
    if changed_config {
        let off: Option<bool> = sqlx::query_scalar(
            "UPDATE ai_settings SET enabled = false, lock_version = lock_version + 1, updated_at = now()
             WHERE active_provider_id = $1 AND enabled RETURNING true",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if off.is_some() {
            sync_client(&mut tx).await?;
            event(
                &mut tx,
                Some(id),
                user,
                "settings_updated",
                json!({"enabled": false, "reason": "provider_config_changed"}),
            )
            .await?;
        }
    }
    let v = provider_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(v)
}

/// `POST /admin/ai/providers/{id}/retire` (step-up). Retiring the provider
/// in use turns the AI off (the settings CHECK needs a provider while
/// enabled); conversations stay and continue with the next provider (Q4).
pub async fn retire_provider(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
) -> anyhow::Result<ProviderView> {
    let mut tx = app.pool.begin().await?;
    let user = require_admin(&mut tx, ctx).await?;
    super::chat::ensure_basics(&mut tx).await?;
    sqlx::query("SELECT id FROM ai_settings FOR UPDATE")
        .execute(&mut *tx)
        .await?;
    let found: Option<bool> =
        sqlx::query_scalar("SELECT retired_at IS NULL FROM ai_providers WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match found {
        None => return Err(not_found()),
        Some(false) => {
            return Err(invalid(
                "id",
                "retired",
                "this provider was already retired",
            ));
        }
        Some(true) => {}
    }
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    sqlx::query("UPDATE ai_providers SET retired_at = now(), updated_at = now(), lock_version = lock_version + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let was_active = sqlx::query(
        "UPDATE ai_settings SET enabled = false, active_provider_id = NULL, lock_version = lock_version + 1,
                updated_by = $2, updated_at = now()
         WHERE active_provider_id = $1",
    )
    .bind(id)
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    sync_client(&mut tx).await?;
    event(
        &mut tx,
        Some(id),
        user,
        "retired",
        json!({"was_active": was_active}),
    )
    .await?;
    let v = provider_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- test connection

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestResult {
    pub ok: bool,
    /// On failure: `timeout`, `http_status`, `malformed`, `endpoint_blocked`,
    /// `not_configured`, `connect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub supports_tools: bool,
    pub at: DateTime<Utc>,
}

/// The fixed test request (P-5): no organisation data.
fn test_request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        system:
            "This is a connection test from oxledger. Call the tool `ping` once, then reply OK."
                .into(),
        messages: vec![Msg {
            role: Role::User,
            blocks: vec![Block::Text("ping".into())],
        }],
        tools: vec![ToolSpec {
            name: "ping".into(),
            description: "Connection test; takes no arguments.".into(),
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }],
        max_tokens: 64,
    }
}

fn failure(e: CallError) -> (String, Option<u16>) {
    match e {
        CallError::Blocked => ("endpoint_blocked".into(), None),
        CallError::NotConfigured => ("not_configured".into(), None),
        CallError::Connect => ("connect".into(), None),
        CallError::Timeout => ("timeout".into(), None),
        CallError::Status(s) => ("http_status".into(), Some(s)),
        CallError::Malformed | CallError::TooLarge => ("malformed".into(), None),
        CallError::Canceled => ("canceled".into(), None),
    }
}

/// `POST /admin/ai/providers/{id}/test` (step-up, P-5). Only the
/// registered endpoint is reached, so the answer is no intranet scanner.
pub async fn test_provider(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
) -> anyhow::Result<TestResult> {
    let (user, kind, endpoint, model) = {
        let mut tx = app.pool.begin().await?;
        let user = require_admin(&mut tx, ctx).await?;
        let row: Option<(String, String, String, bool)> = sqlx::query_as(
            "SELECT kind, endpoint_name, model, retired_at IS NOT NULL FROM ai_providers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((kind, endpoint, model, retired)) = row else {
            return Err(not_found());
        };
        if retired {
            return Err(invalid("id", "retired", "this provider was retired"));
        }
        auth::require_step_up(&mut tx, policy, &ctx).await?;
        tx.commit().await?;
        (user, kind, endpoint, model)
    };
    let kind = Kind::parse(&kind).unwrap_or(Kind::OpenAiCompat);
    check_endpoint(&endpoint, kind).await?;
    let cfg = config::current();
    let ep = cfg.endpoints.get(&endpoint).cloned().ok_or_else(|| {
        endpoint_invalid(
            "unregistered",
            "this endpoint is not registered in the server's settings",
        )
    })?;
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let outcome = client::call(&cfg, &ep, kind, &test_request(&model), &mut rx, &mut |_| {}).await;
    let at = Utc::now();
    let result = match &outcome {
        Ok(turn) => TestResult {
            ok: true,
            kind: None,
            status: None,
            supports_tools: turn.calls.iter().any(|c| c.1 == "ping"),
            at,
        },
        Err(e) => {
            let (k, s) = failure(*e);
            TestResult {
                ok: false,
                kind: Some(k),
                status: s,
                supports_tools: false,
                at,
            }
        }
    };
    let mut tx = app.pool.begin().await?;
    sqlx::query(
        "UPDATE ai_providers SET last_test_at = $2, last_test_ok_at = CASE WHEN $3 THEN $2 ELSE last_test_ok_at END,
                last_test_result = $4, supports_tools = CASE WHEN $3 THEN $5 ELSE supports_tools END,
                updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(at)
    .bind(result.ok)
    .bind(serde_json::to_value(&result)?)
    .bind(result.supports_tools)
    .execute(&mut *tx)
    .await?;
    event(
        &mut tx,
        Some(id),
        user,
        "test_run",
        serde_json::to_value(&result)?,
    )
    .await?;
    tx.commit().await?;
    match outcome {
        Ok(_) => Ok(result),
        Err(CallError::Blocked) => Err(endpoint_invalid(
            "endpoint_blocked",
            "the endpoint resolves to an address that is not allowed",
        )),
        Err(e) => Err(ApiError::new(
            ErrorCode::AiProviderError,
            "the AI service did not answer the test",
        )
        .with_details(e.details().unwrap_or_else(|| json!({"kind": result.kind})))
        .into()),
    }
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SettingsView {
    pub enabled: bool,
    pub active_provider_id: Option<Uuid>,
    pub monthly_budget_usd: Option<String>,
    pub user_daily_budget_usd: String,
    pub user_runs_per_hour: i32,
    pub conversation_retention_days: i32,
    pub lock_version: i32,
    pub updated_at: DateTime<Utc>,
    /// Why `enabled` cannot be turned on now: `no_provider`,
    /// `d17_not_attested`, `not_tested`, `test_failed`, `no_budget`,
    /// `org_account_not_attested`, `endpoint_unregistered`.
    pub enable_blockers: Vec<String>,
    /// This month (organisation time zone): spent, reserved, budget.
    pub month: MonthUsage,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonthUsage {
    /// `YYYY-MM`.
    pub month: String,
    pub spent_usd: String,
    pub reserved_usd: String,
    pub budget_usd: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsInput {
    pub lock_version: i32,
    pub enabled: bool,
    pub active_provider_id: Option<Uuid>,
    /// USD, e.g. "100.00"; required to enable.
    pub monthly_budget_usd: Option<String>,
    pub user_daily_budget_usd: String,
    /// 1–600.
    pub user_runs_per_hour: i32,
    /// 1–365.
    pub conversation_retention_days: i32,
}

type SettingsRow = (
    bool,
    Option<Uuid>,
    Option<String>,
    String,
    i32,
    i32,
    i32,
    DateTime<Utc>,
);

async fn settings_row(conn: &mut PgConnection, lock: bool) -> anyhow::Result<SettingsRow> {
    super::chat::ensure_basics(&mut *conn).await?;
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT enabled, active_provider_id, monthly_budget_usd::text, user_daily_budget_usd::text,
                user_runs_per_hour, conversation_retention_days, lock_version, updated_at
         FROM ai_settings{}",
        if lock { " FOR UPDATE" } else { "" }
    )))
    .fetch_one(&mut *conn)
    .await?)
}

/// The §3.13.2 conditions for `enabled = true` with `provider` and
/// `budget`.
async fn blockers(
    conn: &mut PgConnection,
    provider: Option<Uuid>,
    budget: bool,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    type P = (
        String,
        String,
        bool,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
        bool,
    );
    let row: Option<P> = match provider {
        None => None,
        Some(id) => {
            sqlx::query_as(
                "SELECT kind, endpoint_name, d17_attested_at IS NOT NULL, last_test_at, last_test_ok_at,
                        config_changed_at, org_account_attested_at IS NOT NULL
                 FROM ai_providers WHERE id = $1 AND retired_at IS NULL",
            )
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    match row {
        None => out.push("no_provider".to_string()),
        Some((kind, endpoint, d17, tested, ok, changed, org)) => {
            let cfg = config::current();
            let registered = cfg.endpoints.get(&endpoint).filter(|ep| {
                Kind::parse(&kind).is_some_and(|k| k != Kind::Anthropic || ep.has_key())
            });
            if !d17 {
                out.push("d17_not_attested".into());
            }
            match registered {
                None => out.push("endpoint_unregistered".into()),
                Some(ep) => {
                    let class = net::class_of(cfg.resolver.as_ref(), &ep.host, ep.port).await;
                    if needs_org_attest(class) && !org {
                        out.push("org_account_not_attested".into());
                    }
                }
            }
            let ok_after = ok.is_some_and(|o| o >= changed);
            let failed_last = tested.is_some_and(|t| t >= changed && ok.is_none_or(|o| o < t));
            if failed_last {
                out.push("test_failed".into());
            } else if !ok_after {
                out.push("not_tested".into());
            }
        }
    }
    if !budget {
        out.push("no_budget".into());
    }
    Ok(out)
}

async fn month_usage(
    conn: &mut PgConnection,
    budget: Option<String>,
) -> anyhow::Result<MonthUsage> {
    let (month, spent, reserved): (String, String, String) = sqlx::query_as(
        "SELECT to_char(d.m, 'YYYY-MM'), coalesce(b.spent_usd, 0)::text, coalesce(b.reserved_usd, 0)::text
         FROM (SELECT date_trunc('month', (now() AT TIME ZONE timezone))::date AS m FROM org_settings) d
         LEFT JOIN ai_budget_months b ON b.month = d.m",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(MonthUsage {
        month,
        spent_usd: spent,
        reserved_usd: reserved,
        budget_usd: budget,
    })
}

async fn settings_view(conn: &mut PgConnection) -> anyhow::Result<SettingsView> {
    let r = settings_row(&mut *conn, false).await?;
    let enable_blockers = blockers(&mut *conn, r.1, r.2.is_some()).await?;
    let month = month_usage(&mut *conn, r.2.clone()).await?;
    Ok(SettingsView {
        enabled: r.0,
        active_provider_id: r.1,
        monthly_budget_usd: r.2,
        user_daily_budget_usd: r.3,
        user_runs_per_hour: r.4,
        conversation_retention_days: r.5,
        lock_version: r.6,
        updated_at: r.7,
        enable_blockers,
        month,
    })
}

/// `GET /admin/ai/settings`.
pub async fn settings(app: &App, ctx: AuthContext) -> anyhow::Result<SettingsView> {
    let mut tx = app.pool.begin().await?;
    require_admin(&mut tx, ctx).await?;
    let v = settings_view(&mut tx).await?;
    tx.commit().await?;
    Ok(v)
}

/// `agent_clients('oxledger-ai').enabled` follows `ai_settings.enabled`.
async fn sync_client(conn: &mut PgConnection) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_clients SET enabled = (SELECT enabled FROM ai_settings)
         WHERE name = $1 AND builtin",
    )
    .bind(super::chat::CLIENT_NAME)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `PUT /admin/ai/settings`. Turning the AI on or off and changing the
/// provider in use need a step-up (P-4).
pub async fn put_settings(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    input: &SettingsInput,
) -> anyhow::Result<SettingsView> {
    if let Some(b) = &input.monthly_budget_usd {
        check_money("monthly_budget_usd", b, 8, 2, true)?;
    }
    check_money(
        "user_daily_budget_usd",
        &input.user_daily_budget_usd,
        6,
        2,
        true,
    )?;
    if !(1..=600).contains(&input.user_runs_per_hour) {
        return Err(invalid("user_runs_per_hour", "range", "between 1 and 600"));
    }
    if !(1..=365).contains(&input.conversation_retention_days) {
        return Err(invalid(
            "conversation_retention_days",
            "range",
            "between 1 and 365",
        ));
    }
    let mut tx = app.pool.begin().await?;
    let user = require_admin(&mut tx, ctx).await?;
    let cur = settings_row(&mut tx, true).await?;
    if cur.6 != input.lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the settings changed elsewhere; reload them",
        )
        .with_details(json!({"current_lock_version": cur.6}))
        .into());
    }
    if let Some(p) = input.active_provider_id {
        let live: Option<bool> =
            sqlx::query_scalar("SELECT retired_at IS NULL FROM ai_providers WHERE id = $1")
                .bind(p)
                .fetch_optional(&mut *tx)
                .await?;
        if live != Some(true) {
            return Err(invalid(
                "active_provider_id",
                "unknown",
                "no such active provider",
            ));
        }
    }
    if input.enabled {
        let b = blockers(
            &mut tx,
            input.active_provider_id,
            input.monthly_budget_usd.is_some(),
        )
        .await?;
        if !b.is_empty() {
            return Err(ApiError::new(
                ErrorCode::InvalidValue,
                "the AI assistant cannot be enabled yet",
            )
            .with_details(
                json!({"path": "enabled", "kind": "enable_blocked", "enable_blockers": b}),
            )
            .into());
        }
    }
    if cur.0 != input.enabled || cur.1 != input.active_provider_id {
        auth::require_step_up(&mut tx, policy, &ctx).await?;
    }
    sqlx::query(
        "UPDATE ai_settings SET enabled = $1, active_provider_id = $2, monthly_budget_usd = $3::numeric,
                user_daily_budget_usd = $4::numeric, user_runs_per_hour = $5, conversation_retention_days = $6,
                lock_version = lock_version + 1, updated_by = $7, updated_at = now()",
    )
    .bind(input.enabled)
    .bind(input.active_provider_id)
    .bind(&input.monthly_budget_usd)
    .bind(&input.user_daily_budget_usd)
    .bind(input.user_runs_per_hour)
    .bind(input.conversation_retention_days)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    sync_client(&mut tx).await?;
    if cur.1 != input.active_provider_id
        && let Some(p) = input.active_provider_id
    {
        event(
            &mut tx,
            Some(p),
            user,
            "activated",
            json!({"previous": cur.1}),
        )
        .await?;
    }
    event(
        &mut tx,
        input.active_provider_id,
        user,
        "settings_updated",
        json!({"enabled": input.enabled, "active_provider_id": input.active_provider_id,
               "monthly_budget_usd": input.monthly_budget_usd, "user_daily_budget_usd": input.user_daily_budget_usd,
               "user_runs_per_hour": input.user_runs_per_hour,
               "conversation_retention_days": input.conversation_retention_days}),
    )
    .await?;
    let v = settings_view(&mut tx).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- usage and events

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UsageDay {
    pub day: chrono::NaiveDate,
    pub runs: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spent_usd: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UsageUser {
    pub user_id: Uuid,
    pub login: Option<String>,
    pub name: String,
    pub runs: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spent_usd: String,
}

/// `GET /admin/ai/usage?month=YYYY-MM`: counts and costs only, never
/// conversation content (P2).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Usage {
    pub month: MonthUsage,
    pub days: Vec<UsageDay>,
    pub users: Vec<UsageUser>,
}

pub async fn usage(app: &App, ctx: AuthContext, month: Option<&str>) -> anyhow::Result<Usage> {
    let mut tx = app.pool.begin().await?;
    require_admin(&mut tx, ctx).await?;
    let first: chrono::NaiveDate =
        match month {
            None => sqlx::query_scalar(
                "SELECT date_trunc('month', (now() AT TIME ZONE timezone))::date FROM org_settings",
            )
            .fetch_one(&mut *tx)
            .await?,
            Some(m) => chrono::NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d")
                .ok()
                .filter(|_| m.len() == 7)
                .ok_or_else(|| invalid("month", "format", "YYYY-MM"))?,
        };
    let (label, spent, reserved, budget): (String, String, String, Option<String>) =
        sqlx::query_as(
            "SELECT to_char($1::date, 'YYYY-MM'),
                coalesce((SELECT spent_usd FROM ai_budget_months WHERE month = $1), 0)::text,
                coalesce((SELECT reserved_usd FROM ai_budget_months WHERE month = $1), 0)::text,
                (SELECT monthly_budget_usd::text FROM ai_settings)",
        )
        .bind(first)
        .fetch_one(&mut *tx)
        .await?;
    type D = (chrono::NaiveDate, i64, i64, i64, String);
    let days: Vec<D> = sqlx::query_as(
        "SELECT day, sum(runs)::bigint, sum(input_tokens)::bigint, sum(output_tokens)::bigint, sum(spent_usd)::text
         FROM ai_usage_daily WHERE day >= $1 AND day < ($1 + interval '1 month')::date
         GROUP BY day ORDER BY day",
    )
    .bind(first)
    .fetch_all(&mut *tx)
    .await?;
    type U = (Uuid, Option<String>, String, i64, i64, i64, String);
    let users: Vec<U> = sqlx::query_as(
        "SELECT u.id, u.login, u.name, sum(d.runs)::bigint, sum(d.input_tokens)::bigint,
                sum(d.output_tokens)::bigint, sum(d.spent_usd)::text
         FROM ai_usage_daily d JOIN principals u ON u.id = d.user_id
         WHERE d.day >= $1 AND d.day < ($1 + interval '1 month')::date
         GROUP BY u.id, u.login, u.name ORDER BY sum(d.spent_usd) DESC, u.id",
    )
    .bind(first)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Usage {
        month: MonthUsage {
            month: label,
            spent_usd: spent,
            reserved_usd: reserved,
            budget_usd: budget,
        },
        days: days
            .into_iter()
            .map(|d| UsageDay {
                day: d.0,
                runs: d.1,
                input_tokens: d.2,
                output_tokens: d.3,
                spent_usd: d.4,
            })
            .collect(),
        users: users
            .into_iter()
            .map(|u| UsageUser {
                user_id: u.0,
                login: u.1,
                name: u.2,
                runs: u.3,
                input_tokens: u.4,
                output_tokens: u.5,
                spent_usd: u.6,
            })
            .collect(),
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderEvent {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub actor_id: Uuid,
    pub actor_name: String,
    /// `created`, `updated`, `retired`, `activated`, `settings_updated`, `test_run`.
    pub kind: String,
    pub provider_id: Option<Uuid>,
    #[schemars(with = "serde_json::Value")]
    pub payload: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderEvents {
    pub items: Vec<ProviderEvent>,
}

/// `GET /admin/ai/events`: the last 50.
pub async fn events(app: &App, ctx: AuthContext) -> anyhow::Result<ProviderEvents> {
    let mut conn = app.pool.acquire().await?;
    require_admin(&mut conn, ctx).await?;
    type R = (
        Uuid,
        DateTime<Utc>,
        Uuid,
        String,
        String,
        Option<Uuid>,
        Json,
    );
    let rows: Vec<R> = sqlx::query_as(
        "SELECT e.id, e.at, e.actor_id, p.name, e.kind, e.provider_id, e.payload
         FROM ai_provider_events e JOIN principals p ON p.id = e.actor_id
         ORDER BY e.at DESC, e.id DESC LIMIT 50",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(ProviderEvents {
        items: rows
            .into_iter()
            .map(|r| ProviderEvent {
                id: r.0,
                at: r.1,
                actor_id: r.2,
                actor_name: r.3,
                kind: r.4,
                provider_id: r.5,
                payload: r.6,
            })
            .collect(),
    })
}
