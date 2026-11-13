// SPDX-License-Identifier: GPL-2.0-or-later
//! The AI team (design 05 §3.12, §4.4, §6.18): runtimes, role principals,
//! role templates, pause and resume, and the AI team page.
//!
//! A runtime is one place an agent actually runs; its roles (agent
//! principals) share one work context and its R (§3.12.2). A role's
//! permissions stay the sponsor's ∩ its delegation ∩ the policy (02 §16.1):
//! templates only choose modes inside that, and everything they do not list
//! is `off`.
//!
//! Visibility (§3.12.5): a role exists for the viewer when they are its
//! sponsor, or it has an unrevoked delegation in a project they can read.
//! Anything else answers exactly like a role that does not exist.

use crate::agents::{
    self, BUILTIN_FIELDS, ClosedAs, DelegationInput, DelegationView, IssuedCredential,
    NewPrincipal, PROPOSAL_OPS, PolicyInput, TIME_OPS, Target,
};
use crate::auth::{self, AuthContext, SessionPolicy};
use crate::commands;
use crate::db;
use crate::error::{ApiError, ErrorCode, api};
use crate::policy::PolicyMode;
use crate::service::App;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Design 05 §4.4: the version of the role templates below.
pub const TEMPLATE_VERSION: i32 = 1;
/// B-31: active roles per sponsor.
pub const MAX_ACTIVE_ROLES: i64 = 20;
/// Templates a person may pick (`builtin_chat` is the built-in AI's own).
pub const TEMPLATES: [&str; 7] = [
    "triage",
    "pm_assistant",
    "design_review",
    "qa",
    "release",
    "personal",
    "custom",
];

const ACTION_CREATE: &str = "ai_team_create";

/// B-31: refuses a new role when the sponsor already has
/// [`MAX_ACTIVE_ROLES`] live ones. Every path that creates a role calls it
/// with the sponsor serialized (`agents::serialize_sponsor`): `POST
/// /ai-team` and the 02 `POST /delegations` alias (verification D4).
pub(crate) async fn check_role_limit(
    conn: &mut PgConnection,
    user: Uuid,
    path: &str,
) -> Result<()> {
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_principals p JOIN agent_runtimes r ON r.id = p.runtime_id
         WHERE p.delegator_user_id = $1 AND p.retired_at IS NULL AND r.retired_at IS NULL",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    if active >= MAX_ACTIVE_ROLES {
        return Err(ApiError::new(
            ErrorCode::InvalidValue,
            format!("a sponsor has at most {MAX_ACTIVE_ROLES} active roles; retire one first"),
        )
        .with_details(json!({"path": path, "kind": "role_limit", "max": MAX_ACTIVE_ROLES}))
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------- templates

/// A template's default `role_key` (§4.4); `personal` and `custom` have none.
pub fn default_role_key(template: &str) -> Option<&'static str> {
    Some(match template {
        "triage" => "triage",
        "pm_assistant" => "pm",
        "design_review" => "design_review",
        "qa" => "qa",
        "release" => "release",
        _ => return None,
    })
}

/// One template, as a full allow-list over the delegation's ceiling.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TemplateSpec {
    pub template: String,
    pub template_version: i32,
    /// The ceiling: every proposal op.
    pub proposal_ops: Vec<String>,
    /// The ceiling: every built-in field, plus list custom fields for `triage`.
    pub writable_fields: Vec<String>,
    /// Every op and field of the ceiling with its mode; unlisted ones are `off`.
    pub policies: TemplatePolicies,
    /// Whether the template turns `read_own_time` on (it still needs the ack).
    pub read_own_time: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TemplatePolicies {
    pub commands: BTreeMap<String, PolicyMode>,
    pub fields: BTreeMap<String, PolicyMode>,
}

fn bad_template(t: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, format!("unknown template {t:?}"))
        .with_details(json!({"path": "template", "allowed": TEMPLATES}))
        .into()
}

/// The §4.4 table for `template` (version [`TEMPLATE_VERSION`]).
pub async fn template_spec(conn: &mut PgConnection, template: &str) -> Result<TemplateSpec> {
    use PolicyMode::{Direct, Off, Review};
    if !TEMPLATES.contains(&template) {
        return Err(bad_template(template));
    }
    let mut fields: Vec<String> = BUILTIN_FIELDS.iter().map(|s| s.to_string()).collect();
    // §4.4 triage: list custom fields (labels) are direct.
    let list_cfs: Vec<String> = if template == "triage" {
        sqlx::query_scalar(
            "SELECT 'cf:' || id FROM field_defs
             WHERE ftype = 'list' AND visible AND index_state <> 'dropping' ORDER BY id",
        )
        .fetch_all(&mut *conn)
        .await?
    } else {
        Vec::new()
    };
    fields.extend(list_cfs.iter().cloned());
    let cmd: &[(&str, PolicyMode)] = match template {
        "triage" => &[
            ("triage_decide", Direct),
            ("add_note", Direct),
            ("update_fields", Direct),
            ("duplicate_of", Direct),
        ],
        "pm_assistant" => &[
            ("add_note", Direct),
            ("update_fields", Review),
            ("create_issue", Review),
        ],
        "design_review" => &[("add_note", Direct)],
        // The "only back to a non-done status" whitelist has no store yet:
        // every transition goes to review, so a person sees each one.
        "qa" => &[("add_note", Direct), ("transition", Review)],
        "release" => &[
            ("add_note", Direct),
            ("update_fields", Review),
            ("create_version", Review),
            ("update_version", Review),
        ],
        _ => &[],
    };
    let fld: &[(&str, PolicyMode)] = match template {
        "triage" => &[("priority_id", Direct), ("assigned_to_id", Review)],
        // create_issue's initial subject and description (the minimal issue, §4.4).
        "pm_assistant" => &[("subject", Review), ("description", Review)],
        "release" => &[("fixed_version_id", Review)],
        _ => &[],
    };
    let mut commands: BTreeMap<String, PolicyMode> = BTreeMap::new();
    for op in PROPOSAL_OPS {
        let mode = if template == "personal" {
            if TIME_OPS.contains(&op) {
                Review
            } else {
                Direct
            }
        } else {
            cmd.iter()
                .find(|(k, _)| *k == op)
                .map(|x| x.1)
                .unwrap_or(Off)
        };
        commands.insert(op.to_string(), mode);
    }
    let mut field_modes: BTreeMap<String, PolicyMode> = BTreeMap::new();
    for f in &fields {
        let mode = if template == "personal" || list_cfs.contains(f) {
            Direct
        } else {
            fld.iter()
                .find(|(k, _)| *k == f.as_str())
                .map(|x| x.1)
                .unwrap_or(Off)
        };
        field_modes.insert(f.clone(), mode);
    }
    Ok(TemplateSpec {
        template: template.to_string(),
        template_version: TEMPLATE_VERSION,
        proposal_ops: PROPOSAL_OPS.iter().map(|s| s.to_string()).collect(),
        writable_fields: fields,
        policies: TemplatePolicies {
            commands,
            fields: field_modes,
        },
        read_own_time: template == "personal",
    })
}

/// `GET /ai-team/templates`: every template the wizard offers.
pub async fn templates(pool: &PgPool, ctx: &AuthContext) -> Result<Vec<TemplateSpec>> {
    ctx.require_human_session()?;
    let mut conn = pool.acquire().await?;
    let mut out = Vec::new();
    for t in TEMPLATES {
        out.push(template_spec(&mut conn, t).await?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- pause checks

fn paused_error(scope: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::ForbiddenOp,
        if scope == "principal" {
            "this agent is paused; ask the person who paused it"
        } else {
            "this agent is paused in this project; ask a project manager or its sponsor"
        },
    )
    .with_details(json!({"reason": "agent_paused", "scope": scope}))
    .into()
}

/// Design 05 §3.12.5: writes (direct or proposed) and approvals of a paused
/// role, or of its paused delegation, are refused with `agent_paused`.
pub(crate) async fn check_not_paused(
    conn: &mut PgConnection,
    agent: Uuid,
    delegation: Uuid,
) -> Result<()> {
    // FOR SHARE (M1.9 verification D2): a pause that commits while this
    // write is running waits for it, and a write that starts after the
    // pause's commit sees it. Pause and resume update these rows, so the
    // two serialize; writes do not block each other (shared locks).
    // Lock order: principals, then the delegation (§3.12 lock order).
    let principal: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT paused_at FROM agent_principals WHERE agent_principal_id = $1 FOR SHARE",
    )
    .bind(agent)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    if principal.is_some() {
        return Err(paused_error("principal"));
    }
    let project: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT paused_at FROM delegations WHERE id = $1 FOR SHARE")
            .bind(delegation)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();
    if project.is_some() {
        return Err(paused_error("project"));
    }
    fault("ai_team.after_pause_check").await
}

#[cfg(feature = "test-support")]
async fn fault(point: &str) -> Result<()> {
    crate::changesets::faults::hit(point).await
}

#[cfg(not(feature = "test-support"))]
async fn fault(_: &str) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- who sees what

/// What the viewer may see of one role (§3.12.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Sponsor,
    Admin,
    Member,
}

struct RoleRow {
    id: Uuid,
    handle: String,
    role_key: String,
    template: String,
    template_version: i32,
    duties: String,
    sponsor: Uuid,
    sponsor_name: String,
    runtime: Uuid,
    paused: bool,
    retired: bool,
    runtime_retired: bool,
}

type RoleTuple = (
    Uuid,
    String,
    String,
    String,
    i32,
    String,
    Uuid,
    String,
    Uuid,
    bool,
    bool,
    bool,
);

const ROLE_SELECT: &str =
    "SELECT p.agent_principal_id, p.handle, p.role_key, coalesce(p.role_template, 'personal'),
        coalesce(p.template_version, 1), p.duties, p.delegator_user_id, s.name, p.runtime_id,
        p.paused_at IS NOT NULL, p.retired_at IS NOT NULL, r.retired_at IS NOT NULL
 FROM agent_principals p JOIN principals s ON s.id = p.delegator_user_id
 JOIN agent_runtimes r ON r.id = p.runtime_id";

fn role_row(t: RoleTuple) -> RoleRow {
    RoleRow {
        id: t.0,
        handle: t.1,
        role_key: t.2,
        template: t.3,
        template_version: t.4,
        duties: t.5,
        sponsor: t.6,
        sponsor_name: t.7,
        runtime: t.8,
        paused: t.9,
        retired: t.10,
        runtime_retired: t.11,
    }
}

struct Viewer {
    user: Uuid,
    is_admin: bool,
    projects: BTreeSet<Uuid>,
}

/// The viewer and the projects that count for them (§3.12.5 "觀看者看得到
/// 該專案的成員"): a site admin every readable project; anyone else the
/// projects they read **and** are a member of (themselves or a group). A
/// public project read through its non-member role does not count
/// (verification D8; the contract says "成員").
async fn viewer(conn: &mut PgConnection, ctx: AuthContext) -> Result<Viewer> {
    let (user, _) = ctx.require_human_session()?;
    let me = db::effective(&mut *conn, ctx).await?;
    let scope = db::load_scope(&mut *conn, ctx).await?;
    let mut projects: BTreeSet<Uuid> = scope.project_ids().collect();
    if !me.is_admin {
        let member: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT project_id FROM members
             WHERE principal_id IN (SELECT $1 UNION SELECT group_id FROM group_members WHERE user_id = $1)",
        )
        .bind(user)
        .fetch_all(&mut *conn)
        .await?;
        let member: BTreeSet<Uuid> = member.into_iter().collect();
        projects.retain(|p| member.contains(p));
    }
    Ok(Viewer {
        user,
        is_admin: me.is_admin,
        projects,
    })
}

/// Projects where `agent` has an unrevoked delegation.
async fn delegated_projects(conn: &mut PgConnection, agent: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT project_id FROM delegations WHERE agent_principal_id = $1 AND revoked_at IS NULL
         ORDER BY project_id",
    )
    .bind(agent)
    .fetch_all(conn)
    .await?)
}

async fn seen(conn: &mut PgConnection, v: &Viewer, r: &RoleRow) -> Result<Option<Seen>> {
    if r.sponsor == v.user {
        return Ok(Some(Seen::Sponsor));
    }
    // §3.12.1 incident handling (verification D8): a site admin sees every
    // external role, delegated or not.
    if v.is_admin {
        return Ok(Some(Seen::Admin));
    }
    let projects = delegated_projects(&mut *conn, r.id).await?;
    if !projects.iter().any(|p| v.projects.contains(p)) {
        return Ok(None);
    }
    Ok(Some(Seen::Member))
}

fn no_role() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such agent")
}

/// The role `id` as the viewer sees it, or `not_found` (also when it is
/// invisible to them).
async fn load_seen(conn: &mut PgConnection, v: &Viewer, id: Uuid) -> Result<(RoleRow, Seen)> {
    // M1.10: the built-in AI shows under the same visibility rules as any
    // role (design 05 §3.12.5); its actions are only pause and resume.
    let sql = format!("{ROLE_SELECT} WHERE p.agent_principal_id = $1");
    let row: Option<RoleTuple> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row.map(role_row) else {
        return Err(no_role());
    };
    match seen(&mut *conn, v, &row).await? {
        Some(s) => Ok((row, s)),
        None => Err(no_role()),
    }
}

fn not_sponsor() -> anyhow::Error {
    ApiError::new(
        ErrorCode::ForbiddenOp,
        "only the agent's sponsor can do this",
    )
    .with_details(json!({"reason": "not_sponsor"}))
    .into()
}

// ---------------------------------------------------------------- list and detail

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoleRef {
    pub key: String,
    pub template: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SponsorRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct UndoRate {
    pub undone: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TeamStats {
    pub writes_today: i64,
    pub pending_reviews: i64,
    /// Audience blocks in 7 days: the sponsor and site administrators only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_7d: Option<i64>,
    pub undo_rate_7d: UndoRate,
    pub chain_reviews_7d: i64,
}

/// One row of `GET /ai-team` (§3.12.5).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TeamMember {
    pub id: Uuid,
    pub handle: String,
    pub role: RoleRef,
    /// The sponsor's free text: only for the sponsor and site administrators (V9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duties: Option<String>,
    pub sponsor: SponsorRef,
    /// `active`, `paused`, `quarantined` or `retired`.
    pub status: String,
    pub stats: TeamStats,
    /// Always `only_visible`: the numbers count what the viewer can read.
    pub stats_scope: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TeamList {
    pub items: Vec<TeamMember>,
    /// Always null in M1 (a sponsor has at most 20 active roles).
    pub next_cursor: Option<String>,
}

async fn status_of(conn: &mut PgConnection, r: &RoleRow) -> Result<String> {
    if r.retired || r.runtime_retired {
        return Ok("retired".into());
    }
    if r.paused {
        return Ok("paused".into());
    }
    let ctx: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE runtime_id = $1 AND state = 'active'",
    )
    .bind(r.runtime)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(c) = ctx
        && agents::context_changed(&mut *conn, c).await?
    {
        return Ok("quarantined".into());
    }
    Ok("active".into())
}

async fn stats(
    conn: &mut PgConnection,
    ctx: AuthContext,
    r: &RoleRow,
    seen: Seen,
) -> Result<TeamStats> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let today: DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('day', now())")
        .fetch_one(&mut *conn)
        .await?;
    let week = now - Duration::days(7);
    let execs =
        crate::undo::visible_stats(&mut *conn, ctx, r.sponsor, r.id, week.min(today), now).await?;
    let writes_today = execs.iter().filter(|e| e.1 >= today).count() as i64;
    let in_week: Vec<_> = execs.iter().filter(|e| e.1 >= week).collect();
    let undone = in_week.iter().filter(|e| e.2).count() as i64;
    let pending_reviews = crate::changesets::count_readable(&mut *conn, ctx, r.id, None).await?;
    let chain_reviews_7d =
        crate::changesets::count_readable(&mut *conn, ctx, r.id, Some(week)).await?;
    let blocked_7d = if seen == Seen::Member {
        None
    } else {
        Some(
            sqlx::query_scalar(
                "SELECT count(*) FROM agent_write_log
                 WHERE agent_principal_id = $1 AND kind = 'audience_blocked' AND at >= $2",
            )
            .bind(r.id)
            .bind(week)
            .fetch_one(&mut *conn)
            .await?,
        )
    };
    Ok(TeamStats {
        writes_today,
        pending_reviews,
        blocked_7d,
        undo_rate_7d: UndoRate {
            undone,
            total: in_week.len() as i64,
        },
        chain_reviews_7d,
    })
}

async fn member(
    conn: &mut PgConnection,
    ctx: AuthContext,
    r: &RoleRow,
    seen: Seen,
) -> Result<TeamMember> {
    Ok(TeamMember {
        id: r.id,
        handle: r.handle.clone(),
        role: RoleRef {
            key: r.role_key.clone(),
            template: r.template.clone(),
        },
        duties: (seen != Seen::Member).then(|| r.duties.clone()),
        sponsor: SponsorRef {
            id: r.sponsor,
            name: r.sponsor_name.clone(),
        },
        status: status_of(&mut *conn, r).await?,
        stats: stats(&mut *conn, ctx, r, seen).await?,
        stats_scope: "only_visible".into(),
    })
}

/// `GET /ai-team?project=&handle=`: the roles the viewer may see (§3.12.5).
/// With `project`, only roles with an unrevoked delegation there (an
/// unreadable project lists nothing, like a missing one).
pub async fn list(
    app: &App,
    ctx: AuthContext,
    project: Option<Uuid>,
    handle: Option<&str>,
) -> Result<TeamList> {
    let mut tx = crate::service::read_tx(app).await?;
    let v = viewer(&mut tx, ctx).await?;
    let sql = format!(
        "{ROLE_SELECT}
         WHERE ($1::text IS NULL OR p.handle = $1)
           AND (p.delegator_user_id = $2 OR $5
                OR EXISTS (SELECT 1 FROM delegations d WHERE d.agent_principal_id = p.agent_principal_id
                             AND d.revoked_at IS NULL AND d.project_id = ANY($3)))
           AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM delegations d
                 WHERE d.agent_principal_id = p.agent_principal_id AND d.revoked_at IS NULL
                   AND d.project_id = $4 AND d.project_id = ANY($3)))
         ORDER BY p.handle"
    );
    let projects: Vec<Uuid> = v.projects.iter().copied().collect();
    let rows: Vec<RoleTuple> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(handle)
        .bind(v.user)
        .bind(&projects)
        .bind(project)
        .bind(v.is_admin)
        .fetch_all(&mut *tx)
        .await?;
    let mut items = Vec::new();
    for r in rows.into_iter().map(role_row) {
        let Some(s) = seen(&mut tx, &v, &r).await? else {
            continue;
        };
        items.push(member(&mut tx, ctx, &r, s).await?);
    }
    tx.commit().await?;
    Ok(TeamList {
        items,
        next_cursor: None,
    })
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProjectPolicy {
    pub delegation_id: Uuid,
    pub project_id: Uuid,
    pub project: String,
    pub expires_at: DateTime<Utc>,
    pub read_own_time: bool,
    pub paused_at: Option<DateTime<Utc>>,
    pub paused_reason: Option<String>,
    /// Who paused it here: `sponsor`, `site_admin` or `project` (a manager
    /// of the project). Only on this policy row, i.e. for the viewers who
    /// may see the project's policy.
    pub paused_by: Option<String>,
    pub commands: BTreeMap<String, PolicyMode>,
    pub fields: BTreeMap<String, PolicyMode>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SafetySummary {
    pub project_id: Uuid,
    pub project: String,
    /// Commands not `off` in this project (no fields, no duties).
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RuntimeInfo {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    pub lock_version: i32,
    /// The other active roles sharing the work context and its R.
    pub shared_with: Vec<String>,
    /// Keys of R (the shared source projects).
    pub sources: Vec<String>,
    /// Roles whose active delegation reads your own time (appendix D V10).
    pub time_readers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PauseInfo {
    /// Always `principal` (project pauses are on the policy rows).
    pub scope: String,
    pub paused_at: DateTime<Utc>,
    /// `sponsor` or `site_admin`: the sponsor and site administrators only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TemplateUpdate {
    pub current_version: i32,
    pub latest_version: i32,
    pub outdated: bool,
}

/// `GET /ai-team/{id}`: the fields depend on the viewer (§3.12.5).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TeamDetail {
    #[serde(flatten)]
    pub member: TeamMember,
    /// Sponsor and site admins: every delegation; a project manager: the
    /// projects they manage and read; others: empty.
    pub policies: Vec<ProjectPolicy>,
    /// The projects the viewer can read, with the commands allowed there.
    pub safety_summary: Vec<SafetySummary>,
    /// Sponsor and site admins only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_update: Option<TemplateUpdate>,
    /// What this viewer may do: `pause`, `resume`, `retire`, `credentials`.
    pub actions: Vec<String>,
}

async fn manages(conn: &mut PgConnection, ctx: AuthContext, project: Uuid) -> Result<bool> {
    let actor = crate::perm::actor(&mut *conn, ctx).await?;
    Ok(crate::perm::project_perms(&mut *conn, &actor, project)
        .await?
        .is_some_and(|pp| pp.allowed_any(crate::perm::MANAGE_PROJECT)))
}

type DelegTuple = (
    Uuid,
    Uuid,
    String,
    DateTime<Utc>,
    bool,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
);

async fn delegations_of(conn: &mut PgConnection, agent: Uuid) -> Result<Vec<DelegTuple>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.project_id, p.key, d.expires_at, d.read_own_time, d.paused_at, d.paused_reason,
                CASE WHEN d.paused_by IS NULL THEN NULL
                     WHEN d.paused_by = d.delegator_user_id THEN 'sponsor'
                     WHEN (SELECT is_admin FROM principals WHERE id = d.paused_by) THEN 'site_admin'
                     ELSE 'project' END
         FROM delegations d JOIN projects p ON p.id = d.project_id
         WHERE d.agent_principal_id = $1 AND d.revoked_at IS NULL ORDER BY p.key, d.id",
    )
    .bind(agent)
    .fetch_all(conn)
    .await?)
}

async fn modes(
    conn: &mut PgConnection,
    delegation: Uuid,
) -> Result<(BTreeMap<String, PolicyMode>, BTreeMap<String, PolicyMode>)> {
    let c: Vec<(String, PolicyMode)> = sqlx::query_as(
        "SELECT command_kind, mode FROM delegation_command_policies WHERE delegation_id = $1 ORDER BY command_kind",
    )
    .bind(delegation)
    .fetch_all(&mut *conn)
    .await?;
    let f: Vec<(String, PolicyMode)> = sqlx::query_as(
        "SELECT field_key, mode FROM delegation_field_policies WHERE delegation_id = $1 ORDER BY field_key",
    )
    .bind(delegation)
    .fetch_all(&mut *conn)
    .await?;
    Ok((c.into_iter().collect(), f.into_iter().collect()))
}

pub(crate) async fn runtime_info(
    conn: &mut PgConnection,
    runtime: Uuid,
    except: Option<Uuid>,
) -> Result<RuntimeInfo> {
    let (kind, label, lock_version): (String, String, i32) =
        sqlx::query_as("SELECT kind, label, lock_version FROM agent_runtimes WHERE id = $1")
            .bind(runtime)
            .fetch_one(&mut *conn)
            .await?;
    let shared_with: Vec<String> = sqlx::query_scalar(
        "SELECT handle FROM agent_principals WHERE runtime_id = $1 AND retired_at IS NULL
           AND ($2::uuid IS NULL OR agent_principal_id <> $2) ORDER BY handle",
    )
    .bind(runtime)
    .bind(except)
    .fetch_all(&mut *conn)
    .await?;
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT p.key FROM agent_context_source_projects r
         JOIN agent_work_contexts w ON w.id = r.context_id AND w.state = 'active'
         JOIN projects p ON p.id = r.project_id
         WHERE w.runtime_id = $1 ORDER BY p.key",
    )
    .bind(runtime)
    .fetch_all(&mut *conn)
    .await?;
    let time_readers: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT ap.handle FROM delegations d
         JOIN agent_principals ap ON ap.agent_principal_id = d.agent_principal_id
         JOIN agent_work_contexts w ON w.id = d.context_id AND w.state = 'active'
         WHERE d.runtime_id = $1 AND d.read_own_time AND d.revoked_at IS NULL
           AND d.expires_at > clock_timestamp() AND ap.retired_at IS NULL
           AND ($2::uuid IS NULL OR ap.agent_principal_id <> $2)
         ORDER BY ap.handle",
    )
    .bind(runtime)
    .bind(except)
    .fetch_all(&mut *conn)
    .await?;
    Ok(RuntimeInfo {
        id: runtime,
        kind,
        label,
        lock_version,
        shared_with,
        sources,
        time_readers,
    })
}

/// `GET /ai-team/{id}`.
pub async fn detail(app: &App, ctx: AuthContext, id: Uuid) -> Result<TeamDetail> {
    let mut tx = crate::service::read_tx(app).await?;
    let v = viewer(&mut tx, ctx).await?;
    let (r, s) = load_seen(&mut tx, &v, id).await?;
    let m = member(&mut tx, ctx, &r, s).await?;
    let mut policies = Vec::new();
    let mut safety = Vec::new();
    let mut can_pause_project = false;
    for (did, pid, key, expires_at, rot, paused_at, paused_reason, paused_by) in
        delegations_of(&mut tx, id).await?
    {
        if !v.projects.contains(&pid) {
            continue; // not even its name (看不到＝不存在)
        }
        let (commands, fields) = modes(&mut tx, did).await?;
        safety.push(SafetySummary {
            project_id: pid,
            project: key.clone(),
            allowed_commands: commands
                .iter()
                .filter(|(_, m)| **m != PolicyMode::Off)
                .map(|(c, _)| c.clone())
                .collect(),
        });
        let manager = s == Seen::Member && manages(&mut tx, ctx, pid).await?;
        can_pause_project |= manager;
        if s != Seen::Member || manager {
            policies.push(ProjectPolicy {
                delegation_id: did,
                project_id: pid,
                project: key,
                expires_at,
                read_own_time: rot,
                paused_at,
                paused_reason,
                paused_by,
                commands,
                fields,
            });
        }
    }
    let full = s != Seen::Member;
    let runtime = if full {
        Some(runtime_info(&mut tx, r.runtime, Some(r.id)).await?)
    } else {
        None
    };
    let pause = if r.paused {
        let (scope, at, reason): (String, DateTime<Utc>, Option<String>) = sqlx::query_as(
            "SELECT pause_scope, paused_at, paused_reason FROM agent_principals WHERE agent_principal_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        Some(PauseInfo {
            scope: "principal".into(),
            paused_at: at,
            paused_by: full.then_some(scope),
            reason: if full { reason } else { None },
        })
    } else {
        None
    };
    let mut actions = Vec::new();
    let live = !r.retired && !r.runtime_retired;
    if live && (full || can_pause_project) {
        actions.push("pause".to_string());
        actions.push("resume".to_string());
    }
    // The built-in AI is set up from the chat column: no retire, no tokens.
    if live && s == Seen::Sponsor && r.template != "builtin_chat" {
        actions.push("retire".to_string());
        actions.push("credentials".to_string());
    }
    let template_update = full.then_some(TemplateUpdate {
        current_version: r.template_version,
        latest_version: TEMPLATE_VERSION,
        outdated: r.template_version < TEMPLATE_VERSION,
    });
    tx.commit().await?;
    Ok(TeamDetail {
        member: m,
        policies,
        safety_summary: safety,
        runtime,
        pause,
        template_update,
        actions,
    })
}

/// `GET /ai-team/{id}/activity`: the role's writes, each filtered to what
/// the viewer can read now (the 02 §16.10 function).
pub async fn activity(
    app: &App,
    ctx: AuthContext,
    id: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<crate::undo::Activity> {
    let q = crate::undo::ActivityQuery {
        from,
        to,
        ..Default::default()
    };
    activity_page(app, ctx, id, &q).await
}

/// [`activity`] with an issue filter, a cursor and a page size (G24).
pub async fn activity_page(
    app: &App,
    ctx: AuthContext,
    id: Uuid,
    q: &crate::undo::ActivityQuery,
) -> Result<crate::undo::Activity> {
    let (sponsor, view) = {
        let mut tx = crate::service::read_tx(app).await?;
        let v = viewer(&mut tx, ctx).await?;
        let (r, s) = load_seen(&mut tx, &v, id).await?;
        tx.commit().await?;
        (
            r.sponsor,
            match s {
                Seen::Sponsor => crate::undo::ActivityView::Sponsor,
                Seen::Admin => crate::undo::ActivityView::Admin,
                Seen::Member => crate::undo::ActivityView::Member,
            },
        )
    };
    crate::undo::activity_of(app, ctx, id, sponsor, q, view).await
}

// ---------------------------------------------------------------- create a role

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRuntime {
    pub label: String,
    /// "does not share files, memory or credentials with my other runtimes".
    pub isolation_ack: bool,
}

/// `{"id": …}` for an existing runtime or `{"new": {…}}`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeChoice {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub new: Option<NewRuntime>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleDelegation {
    pub project_id: Uuid,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
    #[serde(default)]
    pub max_ops: Option<i32>,
    /// Changes to the template's modes (the confirmation table); keys must
    /// be inside the ceiling.
    #[serde(default)]
    pub policies: Option<PolicyInput>,
}

/// `POST /ai-team` (§3.12.3).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRoleInput {
    pub runtime: RuntimeChoice,
    pub client_id: Uuid,
    /// Defaults to the template's (§4.4); required for `personal` and `custom`.
    #[serde(default)]
    pub role_key: Option<String>,
    pub template: String,
    #[serde(default)]
    pub duties: String,
    pub delegations: Vec<RoleDelegation>,
    #[serde(default)]
    pub read_own_time: bool,
    #[serde(default)]
    pub read_own_time_ack: bool,
    /// Token label (default: the handle).
    #[serde(default)]
    pub token_label: Option<String>,
    #[serde(default)]
    pub token_expires_in_days: Option<i64>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PrincipalRef {
    pub id: Uuid,
    pub handle: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RuntimeRef {
    pub id: Uuid,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SharedRuntimeWarning {
    /// `shared_runtime`.
    pub kind: String,
    pub roles: Vec<String>,
    pub sources: Vec<String>,
    pub time_readers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CreatedRole {
    pub principal: PrincipalRef,
    pub runtime: RuntimeRef,
    /// Shown once; `null` on a replay of the same Idempotency-Key.
    pub token: Option<String>,
    pub credential_id: Uuid,
    pub token_expires_at: DateTime<Utc>,
    pub delegations: Vec<DelegationView>,
    pub warnings: Vec<SharedRuntimeWarning>,
}

fn invalid(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

fn check_role_key(k: &str) -> Result<()> {
    let ok = k.len() <= 30
        && k.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(invalid(
            "role_key",
            "format",
            "role_key: a lowercase letter, then up to 29 lowercase letters, digits or _",
        ));
    }
    Ok(())
}

fn check_label(l: &str) -> Result<String> {
    let l = l.trim();
    let n = l.chars().count();
    if n == 0 || n > 60 {
        return Err(invalid(
            "label",
            "length",
            "a runtime name is 1–60 characters",
        ));
    }
    Ok(l.to_string())
}

/// `POST /ai-team`: a role in a runtime, its delegations and its first
/// token, in one transaction. Needs a step-up and an Idempotency-Key; a
/// replay answers the same ids with `token: null` (the token is shown once).
pub async fn create(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    operation_id: Uuid,
    input: &CreateRoleInput,
) -> Result<CreatedRole> {
    let (user, session) = ctx.require_human_session()?;
    // Shape first.
    if input.template == "builtin_chat" || !TEMPLATES.contains(&input.template.as_str()) {
        return Err(bad_template(&input.template));
    }
    let role_key = match (&input.role_key, default_role_key(&input.template)) {
        (Some(k), _) => k.trim().to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => {
            return Err(
                ApiError::new(ErrorCode::RequiredFieldMissing, "choose a role_key")
                    .with_details(json!({"path": "role_key"}))
                    .into(),
            );
        }
    };
    check_role_key(&role_key)?;
    if input.duties.chars().count() > 500 {
        return Err(invalid(
            "duties",
            "length",
            "duties are at most 500 characters",
        ));
    }
    if input.delegations.is_empty() {
        return Err(invalid(
            "delegations",
            "empty",
            "give the role at least one project",
        ));
    }
    let new_label = match (&input.runtime.id, &input.runtime.new) {
        (Some(_), None) => None,
        (None, Some(n)) => {
            if !n.isolation_ack {
                return Err(invalid(
                    "runtime.new.isolation_ack",
                    "ack_required",
                    "confirm that this runtime shares no files, memory or credentials with your other runtimes",
                ));
            }
            Some(check_label(&n.label)?)
        }
        _ => {
            return Err(
                ApiError::new(ErrorCode::InvalidRequest, "runtime: give either id or new")
                    .with_details(json!({"path": "runtime"}))
                    .into(),
            );
        }
    };
    let token_lifetime = agents::token_lifetime(input.token_expires_in_days)?;
    let fp = commands::fingerprint(
        ACTION_CREATE,
        &Sha256::digest(serde_json::to_vec(&serde_json::to_value(input)?)?),
    );

    let mut tx = app.pool.begin().await?;
    let Some(request) =
        commands::open_request(&mut tx, user, operation_id, ACTION_CREATE, &fp).await?
    else {
        let r = commands::stored_result(&mut tx, user, operation_id, ACTION_CREATE, &fp).await?;
        let out = replayed(&mut tx, &r).await?;
        tx.commit().await?;
        return Ok(out);
    };
    agents::serialize_sponsor(&mut tx, user).await?;
    check_role_limit(&mut tx, user, "role_key").await?;
    agents::external_client(&mut tx, input.client_id).await?;

    // The runtime.
    let (runtime, label, existing) = match (input.runtime.id, new_label) {
        (Some(id), _) => {
            let row: Option<(String, String, bool)> = sqlx::query_as(
                "SELECT label, kind, retired_at IS NULL FROM agent_runtimes
                 WHERE id = $1 AND sponsor_user_id = $2 FOR UPDATE",
            )
            .bind(id)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
            match row {
                Some((label, kind, true)) if kind == "external" => (id, label, true),
                _ => return Err(api(ErrorCode::NotFound, "no such runtime")),
            }
        }
        (None, Some(label)) => {
            if let Some((rt, l)) = agents::runtime_of_client(&mut tx, user, input.client_id).await?
            {
                return Err(agents::runtime_client_taken(rt, &l));
            }
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM agent_runtimes WHERE sponsor_user_id = $1 AND label = $2)",
            )
            .bind(user)
            .bind(&label)
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                return Err(invalid(
                    "runtime.new.label",
                    "label_taken",
                    "you already have a runtime with this name",
                ));
            }
            let id = agents::insert_runtime(&mut tx, user, session, &label).await?;
            (id, label, false)
        }
        (None, None) => unreachable!("checked above"),
    };
    if existing {
        // A runtime whose context was closed takes no new role (§5).
        let (active, any): (bool, bool) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM agent_work_contexts WHERE runtime_id = $1 AND state = 'active'),
                    EXISTS (SELECT 1 FROM agent_work_contexts WHERE runtime_id = $1)",
        )
        .bind(runtime)
        .fetch_one(&mut *tx)
        .await?;
        if any && !active {
            return Err(agents::runtime_context_closed());
        }
    }
    let key_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agent_principals WHERE runtime_id = $1 AND role_key = $2)",
    )
    .bind(runtime)
    .bind(&role_key)
    .fetch_one(&mut *tx)
    .await?;
    if key_taken {
        return Err(invalid(
            "role_key",
            "role_key_taken",
            "this runtime already has a role with this key",
        ));
    }
    // Warnings are about the roles already there (before this one).
    let warnings = if existing {
        let info = runtime_info(&mut tx, runtime, None).await?;
        vec![SharedRuntimeWarning {
            kind: "shared_runtime".into(),
            roles: info.shared_with,
            sources: info.sources,
            time_readers: info.time_readers,
        }]
    } else {
        Vec::new()
    };
    let spec = template_spec(&mut tx, &input.template).await?;
    let agent = agents::insert_principal(
        &mut tx,
        &NewPrincipal {
            user,
            runtime,
            client: input.client_id,
            role_key: &role_key,
            template: &input.template,
            template_version: TEMPLATE_VERSION,
            duties: input.duties.trim(),
        },
    )
    .await?;
    let mut views = Vec::new();
    for d in &input.delegations {
        let mut chosen = PolicyInput {
            commands: spec.policies.commands.clone(),
            fields: spec.policies.fields.clone(),
        };
        if let Some(p) = &d.policies {
            for (k, m) in &p.commands {
                chosen.commands.insert(k.clone(), *m);
            }
            for (k, m) in &p.fields {
                chosen.fields.insert(k.clone(), *m);
            }
        }
        let di = DelegationInput {
            agent_client_id: input.client_id,
            project_id: d.project_id,
            proposal_ops: spec.proposal_ops.clone(),
            writable_fields: spec.writable_fields.clone(),
            max_ops: d.max_ops,
            expires_in_days: d.expires_in_days,
            policies: Some(chosen),
            read_own_time: input.read_own_time,
            read_own_time_ack: input.read_own_time_ack,
        };
        let shape = agents::check_spec(&di)?;
        agents::check_fields(&mut tx, &di.writable_fields).await?;
        views.push(
            agents::delegate_in(
                &mut tx,
                policy,
                &ctx,
                Target {
                    agent,
                    runtime,
                    client: input.client_id,
                },
                &di,
                shape,
                ClosedAs::Runtime,
            )
            .await?,
        );
    }
    let context = views[0].context_id;
    let handle: String =
        sqlx::query_scalar("SELECT handle FROM agent_principals WHERE agent_principal_id = $1")
            .bind(agent)
            .fetch_one(&mut *tx)
            .await?;
    let cred: IssuedCredential = agents::insert_credential(
        &mut tx,
        user,
        context,
        runtime,
        agent,
        input.token_label.as_deref().unwrap_or(&handle),
        token_lifetime,
    )
    .await?;
    auth::audit(
        &mut tx,
        "ai_team_role_created",
        Some(user),
        Some(agent),
        json!({"runtime_id": runtime, "role_key": role_key, "template": input.template,
               "template_version": TEMPLATE_VERSION}),
    )
    .await?;
    let out = CreatedRole {
        principal: PrincipalRef { id: agent, handle },
        runtime: RuntimeRef { id: runtime, label },
        token: Some(cred.token.clone()),
        credential_id: cred.id,
        token_expires_at: cred.expires_at,
        delegations: views,
        warnings,
    };
    // Ids only (never the token, 02 §7.5).
    let mut stored = serde_json::to_value(&out)?;
    stored["token"] = Json::Null;
    stored["delegations"] = json!(out.delegations.iter().map(|d| d.id).collect::<Vec<_>>());
    commands::complete_request(&mut tx, request, &stored).await?;
    tx.commit().await?;
    Ok(out)
}

async fn replayed(conn: &mut PgConnection, r: &Json) -> Result<CreatedRole> {
    let ids: Vec<Uuid> = serde_json::from_value(r["delegations"].clone()).unwrap_or_default();
    let mut delegations = Vec::new();
    for id in ids {
        delegations.push(agents::delegation_view(&mut *conn, id).await?);
    }
    Ok(CreatedRole {
        principal: serde_json::from_value::<PrincipalRefIn>(r["principal"].clone())?.into(),
        runtime: serde_json::from_value::<RuntimeRefIn>(r["runtime"].clone())?.into(),
        token: None,
        credential_id: serde_json::from_value(r["credential_id"].clone())?,
        token_expires_at: serde_json::from_value(r["token_expires_at"].clone())?,
        delegations,
        warnings: serde_json::from_value::<Vec<WarningIn>>(r["warnings"].clone())
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect(),
    })
}

#[derive(Deserialize)]
struct PrincipalRefIn {
    id: Uuid,
    handle: String,
}
impl From<PrincipalRefIn> for PrincipalRef {
    fn from(p: PrincipalRefIn) -> Self {
        PrincipalRef {
            id: p.id,
            handle: p.handle,
        }
    }
}
#[derive(Deserialize)]
struct RuntimeRefIn {
    id: Uuid,
    label: String,
}
impl From<RuntimeRefIn> for RuntimeRef {
    fn from(p: RuntimeRefIn) -> Self {
        RuntimeRef {
            id: p.id,
            label: p.label,
        }
    }
}
#[derive(Deserialize)]
struct WarningIn {
    kind: String,
    roles: Vec<String>,
    sources: Vec<String>,
    time_readers: Vec<String>,
}
impl From<WarningIn> for SharedRuntimeWarning {
    fn from(w: WarningIn) -> Self {
        SharedRuntimeWarning {
            kind: w.kind,
            roles: w.roles,
            sources: w.sources,
            time_readers: w.time_readers,
        }
    }
}

// ---------------------------------------------------------------- pause, resume, retire

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PauseInput {
    /// `principal` (the whole role) or `project` (its delegation there).
    pub scope: String,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// 1–500 characters; required for `project`.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeInput {
    pub scope: String,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

fn bad_scope() -> anyhow::Error {
    invalid("scope", "scope", "scope is principal or project")
}

fn reason_of(r: &Option<String>, required: bool) -> Result<Option<String>> {
    match r.as_deref().map(str::trim) {
        None | Some("") if required => Err(ApiError::new(
            ErrorCode::RequiredFieldMissing,
            "say why you pause this agent here",
        )
        .with_details(json!({"path": "reason"}))
        .into()),
        None | Some("") => Ok(None),
        Some(s) if s.chars().count() > 500 => Err(invalid(
            "reason",
            "length",
            "a reason is at most 500 characters",
        )),
        Some(s) => Ok(Some(s.to_string())),
    }
}

/// The unrevoked delegations of `agent` in `project`, locked FOR UPDATE.
async fn project_delegations(
    conn: &mut PgConnection,
    agent: Uuid,
    project: Uuid,
) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM delegations WHERE agent_principal_id = $1 AND project_id = $2 AND revoked_at IS NULL
         ORDER BY id FOR UPDATE",
    )
    .bind(agent)
    .bind(project)
    .fetch_all(conn)
    .await?)
}

/// `POST /ai-team/{id}/pause` (§3.12.5). Takes effect at once (the next
/// write checks it); no step-up, so stopping a bot is never slowed down.
pub async fn pause(app: &App, ctx: AuthContext, id: Uuid, input: &PauseInput) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let v = viewer(&mut tx, ctx).await?;
    let (r, s) = load_seen(&mut tx, &v, id).await?;
    if r.retired || r.runtime_retired {
        return Ok(()); // nothing left to pause
    }
    match input.scope.as_str() {
        "principal" => {
            let reason = reason_of(&input.reason, false)?;
            if s == Seen::Member {
                return Err(not_sponsor());
            }
            let scope = if s == Seen::Sponsor {
                "sponsor"
            } else {
                "site_admin"
            };
            // Already paused: a site admin's pause outranks the sponsor's.
            sqlx::query(
                "UPDATE agent_principals SET paused_at = coalesce(paused_at, clock_timestamp()),
                        paused_by = CASE WHEN paused_at IS NULL OR ($3 = 'site_admin' AND pause_scope = 'sponsor')
                                         THEN $2 ELSE paused_by END,
                        paused_reason = CASE WHEN paused_at IS NULL OR ($3 = 'site_admin' AND pause_scope = 'sponsor')
                                             THEN $4 ELSE paused_reason END,
                        pause_scope = CASE WHEN pause_scope = 'site_admin' THEN pause_scope ELSE $3 END
                 WHERE agent_principal_id = $1",
            )
            .bind(id)
            .bind(v.user)
            .bind(scope)
            .bind(&reason)
            .execute(&mut *tx)
            .await?;
            if scope == "site_admin" {
                // Verification D3: new roles of this sponsor and client
                // start paused too (trigger agent_principals_inherit_admin_pause).
                sqlx::query(
                    "UPDATE agent_runtimes SET admin_paused_at = coalesce(admin_paused_at, clock_timestamp()),
                            admin_paused_by = coalesce(admin_paused_by, $2),
                            admin_paused_reason = coalesce(admin_paused_reason, $3)
                     WHERE id = $1",
                )
                .bind(r.runtime)
                .bind(v.user)
                .bind(&reason)
                .execute(&mut *tx)
                .await?;
            }
            auth::audit(
                &mut tx,
                "ai_team_paused",
                Some(v.user),
                Some(id),
                json!({"scope": "principal", "by": scope}),
            )
            .await?;
        }
        "project" => {
            let Some(project) = input.project_id else {
                return Err(
                    ApiError::new(ErrorCode::RequiredFieldMissing, "choose the project")
                        .with_details(json!({"path": "project_id"}))
                        .into(),
                );
            };
            let reason = reason_of(&input.reason, true)?;
            if !v.projects.contains(&project) {
                return Err(api(ErrorCode::NotFound, "no such project"));
            }
            if s == Seen::Member && !manages(&mut tx, ctx, project).await? {
                return Err(not_sponsor());
            }
            let ds = project_delegations(&mut tx, id, project).await?;
            if ds.is_empty() {
                return Err(api(
                    ErrorCode::NotFound,
                    "no delegation of this agent in the project",
                ));
            }
            sqlx::query(
                "UPDATE delegations SET paused_at = clock_timestamp(), paused_by = $2, paused_reason = $3
                 WHERE id = ANY($1) AND paused_at IS NULL",
            )
            .bind(&ds)
            .bind(v.user)
            .bind(&reason)
            .execute(&mut *tx)
            .await?;
            auth::audit(
                &mut tx,
                "ai_team_paused",
                Some(v.user),
                Some(id),
                json!({"scope": "project", "project_id": project}),
            )
            .await?;
        }
        _ => return Err(bad_scope()),
    }
    tx.commit().await?;
    Ok(())
}

/// `POST /ai-team/{id}/resume`: by whoever's layer it is (§3.12.5), after a
/// step-up and re-checking the shared context (epochs) and a usable
/// delegation.
pub async fn resume(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
    input: &ResumeInput,
) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let v = viewer(&mut tx, ctx).await?;
    let (r, s) = load_seen(&mut tx, &v, id).await?;
    if r.retired || r.runtime_retired {
        return Err(no_role());
    }
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    match input.scope.as_str() {
        "principal" => {
            let scope: Option<String> = sqlx::query_scalar(
                "SELECT pause_scope FROM agent_principals WHERE agent_principal_id = $1 FOR UPDATE",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let Some(scope) = scope else {
                return Ok(()); // not paused
            };
            let allowed = match scope.as_str() {
                "site_admin" => v.is_admin,
                _ => s == Seen::Sponsor,
            };
            if !allowed {
                return Err(ApiError::new(
                    ErrorCode::ForbiddenOp,
                    "only the layer that paused this agent can resume it",
                )
                .with_details(json!({"reason": "not_sponsor", "pause_scope": scope}))
                .into());
            }
            revalidate(&mut tx, r.runtime, id, None).await?;
            sqlx::query(
                "UPDATE agent_principals SET paused_at = NULL, paused_by = NULL, pause_scope = NULL,
                        paused_reason = NULL WHERE agent_principal_id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            if scope == "site_admin" {
                // Lift the inherited pause too (the runtime of this role and
                // the sponsor's runtimes that had a role of the same client),
                // unless another live role there is still paused by a site admin.
                sqlx::query(
                    "UPDATE agent_runtimes r SET admin_paused_at = NULL, admin_paused_by = NULL,
                            admin_paused_reason = NULL
                     WHERE r.sponsor_user_id = $2 AND r.admin_paused_at IS NOT NULL
                       AND (r.id = $3 OR EXISTS (SELECT 1 FROM agent_principals p
                                                 WHERE p.runtime_id = r.id AND p.agent_client_id =
                                                   (SELECT agent_client_id FROM agent_principals
                                                    WHERE agent_principal_id = $1)))
                       AND NOT EXISTS (SELECT 1 FROM agent_principals o
                                       WHERE o.runtime_id = r.id AND o.agent_principal_id <> $1
                                         AND o.retired_at IS NULL AND o.pause_scope = 'site_admin')",
                )
                .bind(id)
                .bind(r.sponsor)
                .bind(r.runtime)
                .execute(&mut *tx)
                .await?;
            }
        }
        "project" => {
            let Some(project) = input.project_id else {
                return Err(
                    ApiError::new(ErrorCode::RequiredFieldMissing, "choose the project")
                        .with_details(json!({"path": "project_id"}))
                        .into(),
                );
            };
            if !v.projects.contains(&project) {
                return Err(api(ErrorCode::NotFound, "no such project"));
            }
            if s != Seen::Sponsor && !manages(&mut tx, ctx, project).await? {
                return Err(not_sponsor());
            }
            let ds = project_delegations(&mut tx, id, project).await?;
            if ds.is_empty() {
                return Err(api(
                    ErrorCode::NotFound,
                    "no delegation of this agent in the project",
                ));
            }
            revalidate(&mut tx, r.runtime, id, Some(project)).await?;
            sqlx::query(
                "UPDATE delegations SET paused_at = NULL, paused_by = NULL, paused_reason = NULL
                 WHERE id = ANY($1)",
            )
            .bind(&ds)
            .execute(&mut *tx)
            .await?;
        }
        _ => return Err(bad_scope()),
    }
    auth::audit(
        &mut tx,
        "ai_team_resumed",
        Some(v.user),
        Some(id),
        json!({"scope": input.scope, "project_id": input.project_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Before a resume: the shared context is usable (no epoch change) and the
/// role has a usable delegation (in `project`, if given).
async fn revalidate(
    conn: &mut PgConnection,
    runtime: Uuid,
    agent: Uuid,
    project: Option<Uuid>,
) -> Result<()> {
    let ctx: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE runtime_id = $1 AND state = 'active' FOR SHARE",
    )
    .bind(runtime)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(c) = ctx else {
        return Err(agents::runtime_context_closed());
    };
    if agents::context_changed(&mut *conn, c).await? {
        return Err(agents::quarantined());
    }
    // §3.12.5 "恢復前重驗 epoch、委派、配額" (verification D9): a breaker
    // that suspended the shared context stays in force; lifting it is the
    // 02 §16.6 resume of the context, not this one.
    crate::quota::check_not_suspended(&mut *conn, c).await?;
    let usable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM delegations WHERE context_id = $1 AND agent_principal_id = $2
                          AND ($3::uuid IS NULL OR project_id = $3)
                          AND revoked_at IS NULL AND expires_at > clock_timestamp())",
    )
    .bind(c)
    .bind(agent)
    .bind(project)
    .fetch_one(&mut *conn)
    .await?;
    if !usable {
        return Err(api(
            ErrorCode::DelegationUnavailable,
            "this agent has no usable delegation",
        ));
    }
    Ok(())
}

/// Revokes the role's delegations and tokens and retires it (the caller
/// holds the runtime FOR UPDATE).
async fn retire_locked(conn: &mut PgConnection, agent: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE delegations SET revoked_at = now(), revoked_reason = $2
         WHERE agent_principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(agent)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE agent_credentials SET revoked_at = now() WHERE agent_principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(agent)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE agent_principals SET retired_at = coalesce(retired_at, clock_timestamp())
         WHERE agent_principal_id = $1",
    )
    .bind(agent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `POST /ai-team/{id}/retire`: the sponsor ends a role for good. Its
/// delegations and tokens are revoked; the row stays for the audit.
pub async fn retire(app: &App, policy: &SessionPolicy, ctx: AuthContext, id: Uuid) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let v = viewer(&mut tx, ctx).await?;
    let (r, s) = load_seen(&mut tx, &v, id).await?;
    if s != Seen::Sponsor {
        return Err(not_sponsor());
    }
    if r.template == "builtin_chat" {
        // M1.10: the built-in AI shows on the AI team page with pause and
        // resume only; it is retired with its user (§3.13.7).
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "the built-in AI is not retired from here",
        )
        .with_details(json!({"reason": "builtin_runtime"}))
        .into());
    }
    agents::serialize_sponsor(&mut tx, v.user).await?;
    sqlx::query("SELECT 1 FROM agent_runtimes WHERE id = $1 FOR UPDATE")
        .bind(r.runtime)
        .execute(&mut *tx)
        .await?;
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    if r.retired {
        return Ok(());
    }
    retire_locked(&mut tx, id, "role_retired").await?;
    auth::audit(
        &mut tx,
        "ai_team_role_retired",
        Some(v.user),
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialInput {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// `POST /ai-team/{id}/credentials`: a new token for the role in its
/// runtime's active context (sponsor, step-up). The built-in AI has none.
pub async fn issue_credential(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
    input: &CredentialInput,
) -> Result<IssuedCredential> {
    let lifetime = agents::token_lifetime(input.expires_in_days)?;
    let label = input.label.clone().unwrap_or_default();
    if label.chars().count() > 100 {
        return Err(invalid(
            "label",
            "length",
            "label is at most 100 characters",
        ));
    }
    let mut tx = app.pool.begin().await?;
    let v = viewer(&mut tx, ctx).await?;
    // The sponsor's own built-in AI (not on the team page, so `load_seen`
    // would not find it): §5 `builtin_no_token`. Anyone else's: not found.
    let own_builtin: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agent_principals p JOIN agent_runtimes r ON r.id = p.runtime_id
                        WHERE p.agent_principal_id = $1 AND p.delegator_user_id = $2 AND r.kind = 'builtin')",
    )
    .bind(id)
    .bind(v.user)
    .fetch_one(&mut *tx)
    .await?;
    if own_builtin {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "the built-in AI runs inside oxledger and has no tokens",
        )
        .with_details(json!({"reason": "builtin_no_token"}))
        .into());
    }
    let (r, s) = load_seen(&mut tx, &v, id).await?;
    if s != Seen::Sponsor {
        return Err(not_sponsor());
    }
    sqlx::query("SELECT 1 FROM agent_runtimes WHERE id = $1 FOR SHARE")
        .bind(r.runtime)
        .execute(&mut *tx)
        .await?;
    if r.retired || r.runtime_retired {
        return Err(no_role());
    }
    let context: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_work_contexts WHERE runtime_id = $1 AND state = 'active' FOR SHARE",
    )
    .bind(r.runtime)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(context) = context else {
        return Err(agents::runtime_context_closed());
    };
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    let label = if label.is_empty() {
        r.handle.clone()
    } else {
        label
    };
    let out = agents::insert_credential(&mut tx, v.user, context, r.runtime, id, &label, lifetime)
        .await?;
    tx.commit().await?;
    Ok(out)
}

/// `GET /ai-team/{principal}/credentials` (G14): the role's tokens in every
/// work context, newest first, without the token or its hash. The sponsor
/// and site administrators; a role the caller cannot see is `not_found`,
/// one they see only as a project member is `forbidden_op` `not_sponsor`
/// (as `POST`).
pub async fn list_credentials(
    app: &App,
    ctx: AuthContext,
    id: Uuid,
) -> Result<agents::CredentialList> {
    let mut tx = crate::service::read_tx(app).await?;
    let v = viewer(&mut tx, ctx).await?;
    let (_, s) = load_seen(&mut tx, &v, id).await?;
    if s == Seen::Member {
        return Err(not_sponsor());
    }
    let items = agents::credential_rows(
        &mut tx,
        "c.agent_principal_id = $1 AND $2::uuid IS NULL",
        id,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(agents::CredentialList { items })
}

// ---------------------------------------------------------------- runtimes

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RuntimeRole {
    pub id: Uuid,
    pub handle: String,
    pub role_key: String,
    pub retired: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RuntimeView {
    pub id: Uuid,
    /// `external` or `builtin`.
    pub kind: String,
    pub label: String,
    pub lock_version: i32,
    /// Created by the 0029 upgrade from a 02 agent.
    pub backfilled: bool,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
    /// The active work context, if any.
    pub context_id: Option<Uuid>,
    /// Writes of the active context are paused (02 §16.6): by the sponsor
    /// (`manual`) or by the incident breaker (`audience_blocked`, …). Lift
    /// with `POST /me/agent-runtimes/{id}/resume`.
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspension_reason: Option<String>,
    /// Source projects of the active context whose `security_epoch` changed
    /// since they were read (the context is quarantined; start a new one).
    /// What `ContextView.changed_projects` gave before `/agent-contexts`
    /// was removed (M2.0); empty without an active context.
    pub changed_projects: Vec<Uuid>,
    /// Keys of R in the active context.
    pub sources: Vec<String>,
    pub time_readers: Vec<String>,
    pub roles: Vec<RuntimeRole>,
}

/// `GET /me/agent-runtimes`.
pub async fn runtimes(app: &App, ctx: AuthContext) -> Result<Vec<RuntimeView>> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = crate::service::read_tx(app).await?;
    db::effective(&mut tx, ctx).await?;
    type Row = (
        Uuid,
        String,
        String,
        i32,
        bool,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        Option<Uuid>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.kind, r.label, r.lock_version, r.backfilled, r.created_at, r.retired_at,
                (SELECT w.id FROM agent_work_contexts w WHERE w.runtime_id = r.id AND w.state = 'active')
         FROM agent_runtimes r WHERE r.sponsor_user_id = $1 ORDER BY r.retired_at IS NOT NULL, r.created_at",
    )
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::new();
    for (id, kind, label, lock_version, backfilled, created_at, retired_at, context_id) in rows {
        let info = runtime_info(&mut tx, id, None).await?;
        let suspension: Option<(Option<DateTime<Utc>>, Option<String>)> = match context_id {
            Some(c) => {
                sqlx::query_as(
                    "SELECT suspended_at, suspension_reason FROM agent_context_write_controls
                     WHERE context_id = $1",
                )
                .bind(c)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        let (suspended_at, suspension_reason) = suspension.unwrap_or((None, None));
        let changed_projects = match context_id {
            Some(c) => agents::changed_sources(&mut tx, c, false).await?,
            None => Vec::new(),
        };
        let roles: Vec<(Uuid, String, String, bool)> = sqlx::query_as(
            "SELECT agent_principal_id, handle, role_key, retired_at IS NOT NULL FROM agent_principals
             WHERE runtime_id = $1 ORDER BY handle",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        out.push(RuntimeView {
            id,
            kind,
            label,
            lock_version,
            backfilled,
            created_at,
            retired_at,
            context_id,
            suspended_at,
            suspension_reason,
            changed_projects,
            sources: info.sources,
            time_readers: info.time_readers,
            roles: roles
                .into_iter()
                .map(|(id, handle, role_key, retired)| RuntimeRole {
                    id,
                    handle,
                    role_key,
                    retired,
                })
                .collect(),
        });
    }
    tx.commit().await?;
    Ok(out)
}

/// The caller's runtime FOR UPDATE: (kind, retired), or `not_found`.
async fn own_runtime(conn: &mut PgConnection, user: Uuid, id: Uuid) -> Result<(String, bool)> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT kind, retired_at IS NOT NULL FROM agent_runtimes WHERE id = $1 AND sponsor_user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(conn)
    .await?;
    row.ok_or_else(|| api(ErrorCode::NotFound, "no such runtime"))
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameRuntimeInput {
    pub label: String,
    pub lock_version: i32,
}

/// `PATCH /me/agent-runtimes/{id}`: a new name (`lock_version`).
pub async fn rename_runtime(
    app: &App,
    ctx: AuthContext,
    id: Uuid,
    input: &RenameRuntimeInput,
) -> Result<RuntimeView> {
    let (user, _) = ctx.require_human_session()?;
    let label = check_label(&input.label)?;
    let mut tx = app.pool.begin().await?;
    db::effective(&mut tx, ctx).await?;
    agents::serialize_sponsor(&mut tx, user).await?;
    let (_, retired) = own_runtime(&mut tx, user, id).await?;
    if retired {
        return Err(api(ErrorCode::NotFound, "no such runtime"));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agent_runtimes WHERE sponsor_user_id = $1 AND label = $2 AND id <> $3)",
    )
    .bind(user)
    .bind(&label)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(invalid(
            "label",
            "label_taken",
            "you already have a runtime with this name",
        ));
    }
    let n = sqlx::query(
        "UPDATE agent_runtimes SET label = $2, lock_version = lock_version + 1
         WHERE id = $1 AND lock_version = $3",
    )
    .bind(id)
    .bind(&label)
    .bind(input.lock_version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        let current: i32 =
            sqlx::query_scalar("SELECT lock_version FROM agent_runtimes WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        return Err(
            ApiError::new(ErrorCode::Conflict, "the runtime changed; reload it")
                .with_details(json!({"lock_version": current}))
                .into(),
        );
    }
    auth::audit(
        &mut tx,
        "agent_runtime_renamed",
        Some(user),
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    runtimes(app, ctx)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| api(ErrorCode::NotFound, "no such runtime"))
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNewContextInput {
    /// External runtimes: the "starts without what it read before" declaration.
    #[serde(default)]
    pub fresh_context_ack: bool,
    /// The built-in runtime only (M1.10, §3.13.7).
    #[serde(default)]
    pub carry_over_delegations: bool,
}

/// `POST /me/agent-runtimes/{id}/new-context` (§3.12.2): closes the
/// runtime's context with every role's delegations and tokens and opens an
/// empty one (R = ∅). Step-up.
pub async fn runtime_new_context(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
    input: &RuntimeNewContextInput,
) -> Result<agents::ContextView> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    agents::serialize_sponsor(&mut tx, user).await?;
    let (kind, retired) = own_runtime(&mut tx, user, id).await?;
    if retired {
        return Err(api(ErrorCode::NotFound, "no such runtime"));
    }
    if input.carry_over_delegations && kind != "builtin" {
        // §3.13.7: only the built-in runtime may carry delegations over.
        return Err(ApiError::new(
            ErrorCode::InvalidArgument,
            "carry_over_delegations is only for the built-in AI",
        )
        .with_details(json!({"path": "carry_over_delegations"}))
        .into());
    }
    if kind == "external" && !input.fresh_context_ack {
        return Err(invalid(
            "fresh_context_ack",
            "ack_required",
            "confirm that the agent starts a new work context without what it read before",
        ));
    }
    // The built-in runtime's new context needs no declaration (the server
    // knows what the AI read: its conversations become read-only).
    let carried = if kind == "builtin" && input.carry_over_delegations {
        crate::ai::chat::carried_delegations(&mut tx, id).await?
    } else {
        Vec::new()
    };
    let new = agents::runtime_new_context(&mut tx, policy, &ctx, id, None).await?;
    if kind == "builtin" {
        crate::ai::chat::seal_conversations(&mut tx, id).await?;
        crate::ai::chat::recreate_delegations(&mut tx, policy, &ctx, id, &carried).await?;
    }
    tx.commit().await?;
    agents::list_contexts_of(&app.pool, &ctx, &kind)
        .await?
        .into_iter()
        .find(|c| c.id == new)
        .ok_or_else(|| api(ErrorCode::NotFound, "no such work context"))
}

/// The active context of the caller's live runtime `id`, or `not_found`.
async fn active_context(app: &App, user: Uuid, id: Uuid) -> Result<Uuid> {
    let c: Option<Uuid> = sqlx::query_scalar(
        "SELECT w.id FROM agent_runtimes r
         JOIN agent_work_contexts w ON w.runtime_id = r.id AND w.state = 'active'
         WHERE r.id = $1 AND r.sponsor_user_id = $2 AND r.retired_at IS NULL",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&app.pool)
    .await?;
    c.ok_or_else(|| api(ErrorCode::NotFound, "no such runtime"))
}

/// `POST /me/agent-runtimes/{id}/suspend` (design 06 M2.0; replaces
/// `POST /agent-contexts/{id}/suspend`): pause the writes of the runtime's
/// active context (every role of the runtime, design 05 §2.11); reads go on.
pub async fn suspend_runtime(app: &App, ctx: AuthContext, id: Uuid) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let context = active_context(app, user, id).await?;
    agents::suspend_context(&app.pool, &ctx, context).await
}

/// `POST /me/agent-runtimes/{id}/resume` (design 06 M2.0; replaces
/// `POST /agent-contexts/{id}/resume`): lift a manual or breaker suspension
/// of the active context after the 02 §16.6 checks. Step-up.
pub async fn resume_runtime(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let context = active_context(app, user, id).await?;
    agents::resume_context(&app.pool, policy, &ctx, context).await
}

/// `POST /me/agent-runtimes/{id}/retire` (§3.12.2): an external runtime
/// closes its context, revokes every role's delegations and tokens and
/// retires its roles; the same agent software may then get a new runtime
/// (Q2). Step-up.
pub async fn retire_runtime(
    app: &App,
    policy: &SessionPolicy,
    ctx: AuthContext,
    id: Uuid,
) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    agents::serialize_sponsor(&mut tx, user).await?;
    let (kind, retired) = own_runtime(&mut tx, user, id).await?;
    if kind == "builtin" {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "the built-in AI's runtime is not retired from here",
        )
        .with_details(json!({"reason": "builtin_runtime"}))
        .into());
    }
    auth::require_step_up(&mut tx, policy, &ctx).await?;
    if retired {
        return Ok(());
    }
    agents::close_runtime_context(&mut tx, id, "runtime_retired").await?;
    let roles: Vec<Uuid> = sqlx::query_scalar(
        "SELECT agent_principal_id FROM agent_principals WHERE runtime_id = $1 ORDER BY agent_principal_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for r in roles {
        retire_locked(&mut tx, r, "runtime_retired").await?;
    }
    sqlx::query("UPDATE agent_runtimes SET retired_at = clock_timestamp() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut tx,
        "agent_runtime_retired",
        Some(user),
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
