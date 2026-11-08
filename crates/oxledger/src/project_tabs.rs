// SPDX-License-Identifier: GPL-2.0-or-later
//! Project tabs (contract 02 §18, U44; S6b.6): one shared tab bar per
//! project. Built-in views can be renamed, reordered and hidden; saved
//! queries and external links can be added. Settings are project-public
//! content (R-T7); a saved query runs with the opener's visibility (R-T6).
//! People only, browser session (§18.4); agents neither read nor write.

use crate::auth::AuthContext;
use crate::commands;
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::perm::{self, MANAGE_PROJECT, VIEW_ISSUES};
use crate::query_syntax;
use crate::service::{App, read_tx};
use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

/// The built-in tabs and their default titles; migration 0015's
/// `project_tabs_default()` holds the same list (a test compares them).
pub const BUILTIN: [(&str, &str); 5] = [
    ("timeline", "時程"),
    ("milestones", "里程碑"),
    ("issues", "議題"),
    ("dev", "開發（GitHub）"),
    ("plan", "規劃（心智圖）"),
];
pub const DEFAULT_TAB: &str = "timeline";
pub const MAX_CUSTOM: usize = 10;
pub const MAX_TITLE_CHARS: usize = 20;
pub const MAX_URL_BYTES: usize = 2048;
pub const ACTION_TABS_UPDATE: &str = "project_tabs_update";
pub const ACTION_TABS_RESET: &str = "project_tabs_reset";
const EVENTS_PAGE: i64 = 50;
/// Kinds a project manager does not see (§18.5: the event itself tells
/// that a PR matched an issue here).
const ADMIN_ONLY_SQL: &str = "(e.kind IN ('pr_hidden', 'pr_unhidden')
     OR (e.kind = 'github_content_revoked' AND e.payload ? 'pr_ids'))";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tab {
    /// Built-in: `timeline`, `milestones`, `issues`, `dev`, `plan`;
    /// saved query `q_…`; link `l_…`.
    pub id: String,
    /// `builtin`, `query` or `link`.
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub hidden: bool,
    /// The normalized query (design 03 §11), `kind = query` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The normalized https URL, `kind = link` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct TabSettings {
    pub tabs: Vec<Tab>,
    pub default_tab: String,
    /// True until someone changes the tabs (a reset sets it back).
    pub is_default: bool,
    pub lock_version: i32,
    /// The caller may change them (`manage_project`, active project).
    pub can_manage: bool,
}

/// A tab as sent in a PUT: a new custom tab leaves `id` out.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabInput {
    #[serde(default)]
    pub id: Option<String>,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// `PUT /projects/{id}/tabs`: the whole list replaces the old one.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabsPut {
    pub lock_version: i32,
    pub tabs: Vec<TabInput>,
    pub default_tab: String,
}

/// `POST /projects/{id}/tabs/reset`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabsReset {
    pub lock_version: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SettingActor {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SettingEvent {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub actor: SettingActor,
    /// tabs: `{before, after}` (each `{tabs, default_tab, is_default}`).
    pub payload: Json,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SettingEvents {
    pub items: Vec<SettingEvent>,
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------- shared

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such project")
}

fn invalid(kind: &str, path: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"kind": kind, "path": path}))
        .into()
}

fn forbidden(msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ForbiddenOp, msg)
        .with_details(json!({"reason": "manage_project"}))
        .into()
}

struct Access {
    key: String,
    can_manage: bool,
    is_admin: bool,
}

/// Readable: visible with `view_issues`, active or closed; otherwise
/// `not_found` (unknown and invisible look the same).
async fn access(conn: &mut PgConnection, caller: AuthContext, project: Uuid) -> Result<Access> {
    let actor = perm::actor(&mut *conn, caller).await?;
    let pp = perm::project_perms(&mut *conn, &actor, project)
        .await?
        .ok_or_else(not_found)?;
    if pp.status == "archived" || !pp.allowed_any(VIEW_ISSUES) {
        return Err(not_found());
    }
    let key: String = sqlx::query_scalar("SELECT key FROM projects WHERE id = $1")
        .bind(project)
        .fetch_one(&mut *conn)
        .await?;
    let me = db::effective(&mut *conn, caller).await?;
    Ok(Access {
        key,
        can_manage: pp.status == "active" && pp.allowed_any(MANAGE_PROJECT),
        is_admin: me.is_admin && me.agent.is_none(),
    })
}

async fn settings_row(
    conn: &mut PgConnection,
    project: Uuid,
    lock: bool,
) -> Result<(Vec<Tab>, String, bool, i32)> {
    let sql = if lock {
        "SELECT tabs, default_tab, is_default, lock_version FROM project_tab_settings
         WHERE project_id = $1 FOR UPDATE"
    } else {
        "SELECT tabs, default_tab, is_default, lock_version FROM project_tab_settings
         WHERE project_id = $1"
    };
    let (tabs, d, is_default, lv): (Json, String, bool, i32) = sqlx::query_as(sql)
        .bind(project)
        .fetch_one(&mut *conn)
        .await?;
    Ok((serde_json::from_value(tabs)?, d, is_default, lv))
}

fn snapshot(tabs: &[Tab], default_tab: &str, is_default: bool) -> Json {
    json!({"tabs": tabs, "default_tab": default_tab, "is_default": is_default})
}

fn hash(v: &Json) -> [u8; 32] {
    Sha256::digest([b"oxledger/tabs/v1\n".as_slice(), v.to_string().as_bytes()].concat()).into()
}

fn new_id(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bytes = *Uuid::new_v4().as_bytes();
    let mut s = String::from(prefix);
    let mut acc: u64 = u64::from_be_bytes(bytes[..8].try_into().unwrap());
    for _ in 0..10 {
        s.push(ALPHABET[(acc & 31) as usize] as char);
        acc >>= 5;
    }
    s
}

/// `GET /projects/{id}/tabs`.
pub async fn get(app: &App, caller: AuthContext, project: &str) -> Result<TabSettings> {
    caller.require_human_session()?;
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let (tabs, default_tab, is_default, lock_version) =
        settings_row(&mut tx, project, false).await?;
    tx.commit().await?;
    Ok(TabSettings {
        tabs,
        default_tab,
        is_default,
        lock_version,
        can_manage: a.can_manage,
    })
}

// ---------------------------------------------------------------- validation

/// R-T1–R-T5 over the whole new list; `old` gives the custom ids that may
/// come back (§18.5: unknown ids are `invalid_value`).
async fn validate(
    conn: &mut PgConnection,
    project_key: &str,
    own_origin: &str,
    old: &[Tab],
    input: &TabsPut,
) -> Result<Vec<Tab>> {
    let mut out: Vec<Tab> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut title_keys: Vec<String> = Vec::new();
    let mut custom = 0usize;
    // The whole list first, so an over-long list is `tab_limit` and never
    // reaches the table's CHECK (5–15 entries).
    if input.tabs.len() > BUILTIN.len() + MAX_CUSTOM {
        return Err(invalid(
            "tab_limit",
            "tabs",
            format!("at most {MAX_CUSTOM} saved queries and links"),
        ));
    }
    for (n, t) in input.tabs.iter().enumerate() {
        let path = format!("tabs[{n}]");
        let id = match (t.kind.as_str(), &t.id) {
            ("builtin", Some(id)) if BUILTIN.iter().any(|(b, _)| b == id) => {
                if t.query.is_some() || t.url.is_some() {
                    return Err(invalid(
                        "not_allowed",
                        &path,
                        "a built-in tab has no query or url",
                    ));
                }
                id.clone()
            }
            ("builtin", _) => {
                return Err(invalid(
                    "unknown_id",
                    &format!("{path}.id"),
                    "not a built-in tab id",
                ));
            }
            (k @ ("query" | "link"), id) => {
                custom += 1;
                if custom > MAX_CUSTOM {
                    return Err(invalid(
                        "tab_limit",
                        "tabs",
                        format!("at most {MAX_CUSTOM} saved queries and links"),
                    ));
                }
                match id {
                    None => new_id(if k == "query" { "q_" } else { "l_" }),
                    Some(id) if old.iter().any(|o| o.id == *id && o.kind == k) => id.clone(),
                    Some(_) => {
                        return Err(invalid(
                            "unknown_id",
                            &format!("{path}.id"),
                            "leave id out for a new tab; keep the id of an existing one",
                        ));
                    }
                }
            }
            _ => {
                return Err(invalid(
                    "unknown_kind",
                    &format!("{path}.kind"),
                    "kind is builtin, query or link",
                ));
            }
        };
        if seen_ids.contains(&id) {
            return Err(invalid(
                "duplicate_id",
                &format!("{path}.id"),
                "a tab id appears twice",
            ));
        }
        seen_ids.push(id.clone());

        // R-T2: trim (Unicode White_Space) → NFC; 1–20; no control chars; casefold unique.
        let (title, key): (String, String) =
            sqlx::query_as("SELECT normalize($1, NFC), casefold(normalize($1, NFC))")
                .bind(t.title.trim())
                .fetch_one(&mut *conn)
                .await?;
        let chars = title.chars().count();
        if chars == 0 || chars > MAX_TITLE_CHARS || title.chars().any(char::is_control) {
            return Err(invalid(
                "tab_title_invalid",
                &format!("{path}.title"),
                format!("a tab title is 1 to {MAX_TITLE_CHARS} characters, no control characters"),
            ));
        }
        if title_keys.contains(&key) {
            return Err(invalid(
                "tab_title_taken",
                &format!("{path}.title"),
                "another tab has this title",
            ));
        }
        title_keys.push(key);

        let (query, url) = match t.kind.as_str() {
            "query" => {
                if t.url.is_some() {
                    return Err(invalid("not_allowed", &path, "a saved query has no url"));
                }
                (
                    Some(normalize_query(
                        project_key,
                        t.query.as_deref().unwrap_or(""),
                        &path,
                    )?),
                    None,
                )
            }
            "link" => {
                if t.query.is_some() {
                    return Err(invalid("not_allowed", &path, "a link has no query"));
                }
                (
                    None,
                    Some(normalize_url(
                        t.url.as_deref().unwrap_or(""),
                        own_origin,
                        &path,
                    )?),
                )
            }
            _ => (None, None),
        };
        out.push(Tab {
            id,
            kind: t.kind.clone(),
            title,
            hidden: t.hidden,
            query,
            url,
        });
    }
    for (b, _) in BUILTIN {
        if !out.iter().any(|t| t.id == b) {
            return Err(invalid(
                "builtin_missing",
                "tabs",
                format!("the built-in tab {b} must be in the list (hide it instead)"),
            ));
        }
    }
    // R-T5.
    match out.iter().find(|t| t.id == input.default_tab) {
        Some(t) if !t.hidden && t.kind != "link" => {}
        _ => {
            return Err(invalid(
                "tab_default_invalid",
                "default_tab",
                "the default tab must be in the list, shown, and not a link",
            ));
        }
    }
    Ok(out)
}

/// R-T3: exactly one `project:` equal to this project; added when missing.
pub fn normalize_query(project_key: &str, text: &str, path: &str) -> Result<String> {
    let mut q = query_syntax::parse(text).map_err(|e| {
        anyhow::Error::from(
            ApiError::new(ErrorCode::InvalidValue, format!("query: {}", e.message)).with_details(
                json!({"kind": "tab_query_syntax", "path": format!("{path}.query"), "position": e.position}),
            ),
        )
    })?;
    match q.projects.as_slice() {
        [] => q.projects.push(project_key.to_string()),
        [p] if p.eq_ignore_ascii_case(project_key) => q.projects = vec![project_key.to_string()],
        _ => {
            return Err(invalid(
                "tab_query_scope",
                &format!("{path}.query"),
                format!("a saved query of this project searches project:{project_key} only"),
            ));
        }
    }
    Ok(q.normalized())
}

/// R-T4: https, no userinfo, at most 2048 bytes, WHATWG-normalized, not
/// this server's own origin. The server never fetches it.
pub fn normalize_url(raw: &str, own_origin: &str, path: &str) -> Result<String> {
    let bad = |m: &str| invalid("tab_url_invalid", &format!("{path}.url"), m.to_string());
    if raw.len() > MAX_URL_BYTES {
        return Err(bad("the url is longer than 2048 bytes"));
    }
    let u = url::Url::parse(raw.trim()).map_err(|_| bad("not a url"))?;
    if u.scheme() != "https" {
        return Err(bad("only https links"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("no user name or password in the link"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(bad("the link needs a host"));
    }
    let own = url::Url::parse(own_origin).ok().map(|o| o.origin());
    if own.is_some_and(|o| o == u.origin()) {
        return Err(bad("a link may not point at this server"));
    }
    let s = u.to_string();
    if s.len() > MAX_URL_BYTES {
        return Err(bad("the url is longer than 2048 bytes"));
    }
    Ok(s)
}

// ---------------------------------------------------------------- writes

enum Change<'a> {
    Put(&'a TabsPut),
    Reset(i32),
}

/// PUT and reset (§18.5): operation_requests → executor locks → project
/// `FOR SHARE` (active) → settings `FOR UPDATE` → lock_version → R-T1–R-T5
/// → UPDATE → event.
async fn write(
    app: &App,
    caller: AuthContext,
    own_origin: &str,
    operation_id: Uuid,
    project: &str,
    change: Change<'_>,
) -> Result<TabSettings> {
    let who = commands::executor(&caller)?;
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let (action, h, lock_version) = match &change {
        Change::Put(p) => (
            ACTION_TABS_UPDATE,
            hash(
                &json!({"project": project, "lock_version": p.lock_version, "default_tab": p.default_tab,
                         "tabs": p.tabs.iter().map(|t| json!({"id": t.id, "kind": t.kind, "title": t.title,
                             "hidden": t.hidden, "query": t.query, "url": t.url})).collect::<Vec<_>>()}),
            ),
            p.lock_version,
        ),
        Change::Reset(lv) => (
            ACTION_TABS_RESET,
            hash(&json!({"project": project, "lock_version": lv, "reset": true})),
            *lv,
        ),
    };
    let fp = commands::fingerprint(action, &h);
    let Some(request_id) =
        commands::open_request(&mut tx, who.user_id, operation_id, action, &fp).await?
    else {
        // T-PT14: the first result, as the settings are now.
        commands::stored_result(&mut tx, who.user_id, operation_id, action, &fp).await?;
        let a = access(&mut tx, caller, project).await?;
        let (tabs, default_tab, is_default, lock_version) =
            settings_row(&mut tx, project, false).await?;
        tx.commit().await?;
        return Ok(TabSettings {
            tabs,
            default_tab,
            is_default,
            lock_version,
            can_manage: a.can_manage,
        });
    };
    commands::lock_executor(&mut tx, &who).await?;
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR SHARE")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    let a = access(&mut tx, caller, project).await?;
    if !a.can_manage {
        return Err(forbidden(
            "changing the project tabs needs manage_project in an active project",
        ));
    }
    let (old_tabs, old_default, old_is_default, lv) = settings_row(&mut tx, project, true).await?;
    if lv != lock_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the tabs changed; here are the current ones",
        )
        .with_details(json!({
            "current_lock_version": lv,
            "current": snapshot(&old_tabs, &old_default, old_is_default),
        }))
        .into());
    }
    let (tabs, default_tab, is_default, kind) = match change {
        Change::Put(p) => (
            validate(&mut tx, &a.key, own_origin, &old_tabs, p).await?,
            p.default_tab.clone(),
            false,
            "tabs_updated",
        ),
        Change::Reset(_) => {
            let t: Json = sqlx::query_scalar("SELECT project_tabs_default()")
                .fetch_one(&mut *tx)
                .await?;
            (
                serde_json::from_value(t)?,
                DEFAULT_TAB.to_string(),
                true,
                "tabs_reset",
            )
        }
    };
    let lock_version: i32 = sqlx::query_scalar(
        "UPDATE project_tab_settings
         SET tabs = $2, default_tab = $3, is_default = $4, lock_version = lock_version + 1,
             updated_by = $5, updated_at = now()
         WHERE project_id = $1 RETURNING lock_version",
    )
    .bind(project)
    .bind(serde_json::to_value(&tabs)?)
    .bind(&default_tab)
    .bind(is_default)
    .bind(who.user_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(project)
    .bind(kind)
    .bind(who.user_id)
    .bind(request_id)
    .bind(json!({
        "before": snapshot(&old_tabs, &old_default, old_is_default),
        "after": snapshot(&tabs, &default_tab, is_default),
    }))
    .execute(&mut *tx)
    .await?;
    commands::complete_request(
        &mut tx,
        request_id,
        &json!({"project_id": project, "lock_version": lock_version}),
    )
    .await?;
    tx.commit().await?;
    Ok(TabSettings {
        tabs,
        default_tab,
        is_default,
        lock_version,
        can_manage: true,
    })
}

/// `PUT /projects/{id}/tabs` (Idempotency-Key). `own_origin` is the SPA
/// origin (R-T4).
pub async fn put(
    app: &App,
    caller: AuthContext,
    own_origin: &str,
    operation_id: Uuid,
    project: &str,
    input: &TabsPut,
) -> Result<TabSettings> {
    write(
        app,
        caller,
        own_origin,
        operation_id,
        project,
        Change::Put(input),
    )
    .await
}

/// `POST /projects/{id}/tabs/reset` (Idempotency-Key).
pub async fn reset(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    project: &str,
    input: &TabsReset,
) -> Result<TabSettings> {
    write(
        app,
        caller,
        "",
        operation_id,
        project,
        Change::Reset(input.lock_version),
    )
    .await
}

// ---------------------------------------------------------------- history

/// `GET /projects/{id}/setting-events?cursor=`: `manage_project` (or a
/// system administrator). The PR events of §18.5 are left out for anyone
/// but an administrator, in the SQL itself, so pages and cursors look the
/// same as if they did not exist (T-PT17).
pub async fn events(
    app: &App,
    caller: AuthContext,
    project: &str,
    cursor: Option<&str>,
) -> Result<SettingEvents> {
    caller.require_human_session()?;
    let after: Option<(DateTime<Utc>, Uuid)> = cursor
        .map(|c| {
            let bad = || api(ErrorCode::InvalidCursor, "not a setting-events cursor");
            let b = crate::auth::decode_bytes(c).ok_or_else(bad)?;
            let v: Json = serde_json::from_slice(&b).map_err(|_| bad())?;
            let at = v["at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .ok_or_else(bad)?
                .with_timezone(&Utc);
            let id = v["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(bad)?;
            Ok::<_, anyhow::Error>((at, id))
        })
        .transpose()?;
    let mut tx = read_tx(app).await?;
    let project = describe::visible_project(&mut tx, caller, project)
        .await
        .map_err(|_| not_found())?;
    let a = access(&mut tx, caller, project).await?;
    let actor = perm::actor(&mut tx, caller).await?;
    let manages = perm::project_perms(&mut tx, &actor, project)
        .await?
        .is_some_and(|pp| pp.allowed_any(MANAGE_PROJECT));
    if !manages && !a.is_admin {
        return Err(forbidden("the settings history needs manage_project"));
    }
    type Row = (Uuid, DateTime<Utc>, String, Uuid, String, Json);
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT e.id, e.at, e.kind, p.id, p.name, e.payload
         FROM project_setting_events e JOIN principals p ON p.id = e.actor_id
         WHERE e.project_id = $1 AND ($2 OR NOT {ADMIN_ONLY_SQL})
           AND ($3::timestamptz IS NULL OR (e.at, e.id) < ($3, $4))
         ORDER BY e.at DESC, e.id DESC LIMIT $5"
    )))
    .bind(project)
    .bind(a.is_admin)
    .bind(after.map(|x| x.0))
    .bind(after.map(|x| x.1))
    .bind(EVENTS_PAGE + 1)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let more = rows.len() as i64 > EVENTS_PAGE;
    let items: Vec<SettingEvent> = rows
        .into_iter()
        .take(EVENTS_PAGE as usize)
        .map(|(id, at, kind, aid, name, payload)| SettingEvent {
            id,
            at,
            kind,
            actor: SettingActor { id: aid, name },
            payload,
        })
        .collect();
    let next_cursor = more.then(|| {
        let last = items.last().unwrap();
        crate::auth::encode(
            json!({"at": last.at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true), "id": last.id})
                .to_string()
                .as_bytes(),
        )
    });
    Ok(SettingEvents { items, next_cursor })
}

/// Runs a saved query tab as an ordinary search with the opener's
/// visibility (R-T3, R-T6): no stored results, no counts.
pub async fn run_query(
    app: &App,
    caller: AuthContext,
    project: &str,
    tab_id: &str,
    cursor: Option<String>,
) -> Result<crate::service::SearchResult> {
    let t = get(app, caller, project).await?;
    let tab = t
        .tabs
        .iter()
        .find(|x| x.id == tab_id && x.kind == "query")
        .ok_or_else(|| api(ErrorCode::NotFound, "no such saved query tab"))?;
    let q = query_syntax::parse(tab.query.as_deref().unwrap_or(""))
        .map_err(|_| api(ErrorCode::InvalidQuery, "the saved query no longer parses"))?;
    let mut p = q.to_params();
    p.cursor = cursor;
    crate::service::search(app, caller, p).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let own = "https://ledger.example.com";
        for bad in [
            "http://x.com/a",
            "https://u:p@x.com/",
            "https://u@x.com/",
            "javascript:alert(1)",
            "https://ledger.example.com/issues",
            "HTTPS://LEDGER.example.com:443/",
            "not a url",
        ] {
            assert!(normalize_url(bad, own, "t").is_err(), "{bad}");
        }
        let long = format!("https://x.com/{}", "a".repeat(3000));
        assert!(normalize_url(&long, own, "t").is_err());
        assert_eq!(
            normalize_url("https://Docs.Example.com:443/a/../b?x=1", own, "t").unwrap(),
            "https://docs.example.com/b?x=1"
        );
    }

    #[test]
    fn queries() {
        assert_eq!(
            normalize_query("AUTH", "status:open  login", "t").unwrap(),
            "project:AUTH status:open login"
        );
        assert_eq!(
            normalize_query("AUTH", "project:auth", "t").unwrap(),
            "project:AUTH"
        );
        assert!(normalize_query("AUTH", "project:WEB status:open", "t").is_err());
        assert!(normalize_query("AUTH", "project:AUTH project:WEB", "t").is_err());
        assert!(normalize_query("AUTH", "status:", "t").is_err());
    }

    #[test]
    fn ids() {
        let a = new_id("q_");
        assert_eq!(a.len(), 12);
        assert!(
            a[2..]
                .chars()
                .all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c))
        );
        assert_ne!(a, new_id("q_"));
    }
}
