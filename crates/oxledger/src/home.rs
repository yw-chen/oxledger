// SPDX-License-Identifier: GPL-2.0-or-later
//! Personal preferences, the home page and interface roles (design 05
//! §2.8, §3.8, §6.12; M1.6, L4, U24).
//!
//! - `GET`/`PUT /me/preferences`: interface roles, theme, density, left-nav
//!   shortcuts, default views per project. Only for the signed-in person
//!   (people only; agents have no `/me`, and nobody can read anyone else's,
//!   T-M1-61).
//! - `GET`/`PUT /me/home`: the home widgets. `need_decision` is protected
//!   (U22): exactly one, it can be moved and resized, never removed
//!   (`protected_widget_missing`, T-M1-60).
//! - `POST /me/home/apply-template`: the interface roles' defaults, `replace`
//!   or `merge`; `GET /home/templates`: the templates as data (the "preview
//!   as role" is drawn by the browser with the viewer's own permissions).
//!
//! Interface roles change defaults only, never permissions. The row is
//! created on the first read (`INSERT … ON CONFLICT DO NOTHING`), not in the
//! login transaction. What a widget, shortcut or default view names (a view,
//! a project) is checked again on every read with the reader's visibility:
//! a view that was deleted, became unreadable or is bound to a project the
//! reader cannot see any more, and a project they left (or whose issues or
//! module they lost) all read as `available: false` the same way, with no
//! name and no count (not visible = does not exist). The widgets' data come
//! from the endpoints that already check visibility per item (§3.8 table).

use crate::auth::AuthContext;
use crate::commands;
use crate::db;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api};
use crate::service::{App, read_tx};
use crate::views;
use anyhow::Result;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Postgres};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// `user_preferences.interface_roles` (0026 CHECK).
pub const ROLES: [&str; 3] = ["pm", "designer", "engineer"];
/// `user_preferences.theme` (0026 CHECK).
pub const THEMES: [&str; 4] = ["dark", "light", "high_contrast", "system"];
/// `user_preferences.density` (0026 CHECK).
pub const DENSITIES: [&str; 2] = ["comfortable", "compact"];
/// Home widget types (§3.8).
pub const WIDGET_TYPES: [&str; 6] = [
    "need_decision",
    "view",
    "triage_count",
    "milestones",
    "my_time_week",
    "agent_summary",
];
/// The protected widget (U22).
pub const PROTECTED: &str = "need_decision";
/// Left-nav shortcut kinds: a view (with `id`), the triage inbox, the
/// project timeline, the project board, my timesheet.
pub const SHORTCUT_KINDS: [&str; 5] = ["view", "triage", "timeline", "board", "timesheet"];
/// Default views that are not a saved view: the project's built-in list,
/// board and timeline. `"*"` (every project without its own entry) only
/// takes these.
pub const DEFAULT_VIEW_BUILTINS: [&str; 3] = ["builtin:list", "builtin:board", "builtin:timeline"];
/// B-21.
pub const MAX_WIDGETS: usize = 12;
/// A template for several roles takes from each in turn, at most 8 (§3.8).
pub const TEMPLATE_MAX_WIDGETS: usize = 8;
pub const MAX_SHORTCUTS: usize = 20;
pub const MAX_DEFAULT_VIEWS: usize = 100;
/// `view` widget `limit` (§3.8) and its default.
pub const VIEW_LIMIT: (i32, i32) = (3, 20);
pub const DEFAULT_VIEW_LIMIT: i32 = 8;
/// A view id as sent (a uuid is 36, a built-in with a project id 58).
pub const MAX_VIEW_ID: usize = 80;

const ACTION_PREFS: &str = "me_preferences_put";
const ACTION_HOME: &str = "me_home_put";
const ACTION_APPLY: &str = "me_home_apply_template";

// ---------------------------------------------------------------- DTOs

/// A home widget as stored and as a template gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Stored {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    width: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<Uuid>,
}

impl Stored {
    /// Same widget for `merge` and for the template's round-robin.
    fn same(&self, o: &Stored) -> bool {
        self.kind == o.kind && self.view_id == o.view_id && self.project_id == o.project_id
    }
}

/// One home widget (§3.8).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Widget {
    pub id: String,
    /// need_decision, view, triage_count, milestones, my_time_week, agent_summary.
    #[serde(rename = "type")]
    pub widget_type: String,
    /// 1–3 columns.
    pub width: i32,
    /// `view`: a view id or `builtin:…`; run it with `POST /views/{view_id}/run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,
    /// `view`: rows to show, 3–20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    /// `milestones`: the project (`GET /projects/{id}/versions?include=stats`);
    /// absent until the person picks one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// `need_decision`: cannot be removed (U22).
    pub protected: bool,
    /// What it names can be read now. `false` when the view was deleted,
    /// became unreadable or is bound to a project the reader cannot see,
    /// or the milestones project is not readable any more — one answer for
    /// all of them; show "this view no longer exists".
    pub available: bool,
    /// `view`, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_name: Option<String>,
    /// `view`, when available: list or board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_kind: Option<String>,
    /// `milestones`, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
}

/// `GET /me/home`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HomeView {
    pub lock_version: i32,
    pub widgets: Vec<Widget>,
}

/// One widget of `PUT /me/home`. The read-only fields of `GET /me/home`
/// (`protected`, `available`, names) may be sent back and are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetInput {
    /// Keep the id of an existing widget; a new one may leave it out.
    /// 1–40 letters, digits, `_` and `-`.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub widget_type: String,
    /// 1–3; default 2 for need_decision, else 1.
    #[serde(default)]
    pub width: Option<i32>,
    #[serde(default)]
    pub view_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i32>,
    /// `milestones`: a project id or key.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub protected: Option<bool>,
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub view_name: Option<String>,
    #[serde(default)]
    pub view_kind: Option<String>,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
}

/// `PUT /me/home`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HomePut {
    pub lock_version: i32,
    pub widgets: Vec<WidgetInput>,
}

/// A left-nav shortcut as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Shortcut {
    /// view, triage, timeline, board, timesheet.
    pub kind: String,
    /// `view`: a view id or `builtin:…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A left-nav shortcut as read.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShortcutView {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `view`: the view can be opened now (same rule as a widget).
    pub available: bool,
    /// `view`, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A shortcut of `PUT /me/preferences` (`available` and `name` ignored).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShortcutInput {
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
}

/// `GET /me/preferences`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreferencesView {
    /// pm, designer, engineer (only defaults, never permissions).
    pub interface_roles: Vec<String>,
    /// false: ask "what do you mainly do?" at sign-in.
    pub onboarded: bool,
    /// dark, light, high_contrast, system.
    pub theme: String,
    /// comfortable, compact.
    pub density: String,
    pub nav_shortcuts: Vec<ShortcutView>,
    /// Project id (or `*`: every other project) → the default view. Entries
    /// whose project or view the reader cannot see any more stay, with
    /// `available: false` (send them back unchanged to keep them).
    pub default_views: BTreeMap<String, DefaultView>,
    pub lock_version: i32,
}

/// One default view as read.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DefaultView {
    /// A view id or `builtin:list` / `builtin:board` / `builtin:timeline`.
    pub view_id: String,
    /// The project (for `*`, none) and the view can be read now; one
    /// answer for "deleted" and "not visible".
    pub available: bool,
    /// A saved view's name, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One default view of `PUT /me/preferences`: the view id, or the object
/// `GET` returned (`available` and `name` ignored).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DefaultViewInput {
    Id(String),
    Read(DefaultViewRead),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultViewRead {
    pub view_id: String,
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
}

impl From<&str> for DefaultViewInput {
    fn from(s: &str) -> Self {
        Self::Id(s.to_string())
    }
}

impl From<String> for DefaultViewInput {
    fn from(s: String) -> Self {
        Self::Id(s)
    }
}

impl DefaultViewInput {
    fn view_id(&self) -> &str {
        match self {
            Self::Id(s) => s,
            Self::Read(r) => &r.view_id,
        }
    }
}

/// `PUT /me/preferences`: fields left out stay as they are.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreferencesPut {
    pub lock_version: i32,
    #[serde(default)]
    pub interface_roles: Option<Vec<String>>,
    /// true marks the onboarding as done; false asks again at next sign-in.
    #[serde(default)]
    pub onboarded: Option<bool>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub density: Option<String>,
    /// The whole list, in order.
    #[serde(default)]
    pub nav_shortcuts: Option<Vec<ShortcutInput>>,
    /// The whole map: project id or `*` → a view id (or the object `GET`
    /// returned).
    #[serde(default)]
    pub default_views: Option<BTreeMap<String, DefaultViewInput>>,
}

/// `POST /me/home/apply-template`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyTemplate {
    /// Interface roles in order (empty: the general template).
    pub roles: Vec<String>,
    /// replace or merge.
    pub mode: String,
}

/// The result of `apply-template`: the home and the preferences (the
/// template also sets shortcuts and the `*` default view).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Applied {
    pub home: HomeView,
    pub preferences: PreferencesView,
}

/// `GET /home/templates?roles=`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Template {
    pub roles: Vec<String>,
    pub widgets: Vec<Widget>,
    pub nav_shortcuts: Vec<Shortcut>,
    pub default_views: BTreeMap<String, String>,
}

// ---------------------------------------------------------------- templates

fn w(id: &str, kind: &str, width: i32) -> Stored {
    Stored {
        id: id.into(),
        kind: kind.into(),
        width,
        view_id: None,
        limit: None,
        project_id: None,
    }
}

fn wv(id: &str, view: &str) -> Stored {
    Stored {
        view_id: Some(view.into()),
        limit: Some(DEFAULT_VIEW_LIMIT),
        ..w(id, "view", 1)
    }
}

fn sc(kind: &str, id: Option<&str>) -> Shortcut {
    Shortcut {
        kind: kind.into(),
        id: id.map(String::from),
    }
}

/// One role's defaults (§3.8 table); `None` is the general template
/// ("skip": no role chosen).
fn role_template(role: Option<&str>) -> (Vec<Stored>, Vec<Shortcut>, &'static str) {
    let decide = w("w_decide", PROTECTED, 2);
    let assigned = wv("w_assigned", "builtin:assigned_to_me");
    let involving = wv("w_involving", "builtin:involving_me");
    let assigned_sc = sc("view", Some("builtin:assigned_to_me"));
    match role {
        Some("pm") => (
            vec![
                decide,
                w("w_triage", "triage_count", 1),
                w("w_milestones", "milestones", 1),
                involving,
            ],
            vec![sc("triage", None), sc("timeline", None)],
            "builtin:timeline",
        ),
        Some("designer") => (
            vec![decide, assigned, involving],
            vec![assigned_sc],
            "builtin:list",
        ),
        Some("engineer") => (
            vec![
                decide,
                assigned,
                w("w_time", "my_time_week", 1),
                w("w_agent", "agent_summary", 1),
            ],
            vec![sc("board", None), assigned_sc],
            "builtin:board",
        ),
        _ => (
            vec![decide, assigned, involving],
            vec![assigned_sc],
            "builtin:list",
        ),
    }
}

/// The roles' templates taken in turn (the given order), duplicates once,
/// at most [`TEMPLATE_MAX_WIDGETS`] widgets; the `*` default view is the
/// first role's.
fn build_template(roles: &[String]) -> (Vec<Stored>, Vec<Shortcut>, &'static str) {
    if roles.is_empty() {
        return role_template(None);
    }
    let parts: Vec<_> = roles.iter().map(|r| role_template(Some(r))).collect();
    let mut widgets: Vec<Stored> = Vec::new();
    let mut shortcuts: Vec<Shortcut> = Vec::new();
    let longest = parts
        .iter()
        .map(|p| p.0.len().max(p.1.len()))
        .max()
        .unwrap_or(0);
    for i in 0..longest {
        for p in &parts {
            if let Some(x) = p.0.get(i)
                && widgets.len() < TEMPLATE_MAX_WIDGETS
                && !widgets.iter().any(|y| y.same(x))
            {
                widgets.push(x.clone());
            }
            if let Some(s) = p.1.get(i)
                && !shortcuts.contains(s)
            {
                shortcuts.push(s.clone());
            }
        }
    }
    (widgets, shortcuts, parts[0].2)
}

fn invalid(path: &str, kind: &str, msg: impl Into<String>) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": path, "kind": kind}))
        .into()
}

/// Roles as given, checked and deduplicated (order kept).
fn parse_roles(roles: &[String], path: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for (i, r) in roles.iter().enumerate() {
        let r = r.trim();
        if !ROLES.contains(&r) {
            return Err(invalid(
                &format!("{path}[{i}]"),
                "unknown_role",
                "interface roles are pm, designer and engineer",
            ));
        }
        if !out.iter().any(|x| x == r) {
            out.push(r.to_string());
        }
    }
    Ok(out)
}

fn template_view(roles: Vec<String>) -> Template {
    let (widgets, nav_shortcuts, default) = build_template(&roles);
    Template {
        roles,
        widgets: widgets
            .into_iter()
            .map(|s| Widget {
                protected: s.kind == PROTECTED,
                available: true,
                id: s.id,
                widget_type: s.kind,
                width: s.width,
                view_id: s.view_id,
                limit: s.limit,
                project_id: s.project_id,
                view_name: None,
                view_kind: None,
                project_key: None,
                project_name: None,
            })
            .collect(),
        nav_shortcuts,
        default_views: BTreeMap::from([("*".to_string(), default.to_string())]),
    }
}

/// `GET /home/templates?roles=pm,designer`: pure data, changes nothing.
pub fn templates(caller: AuthContext, roles: Option<&str>) -> Result<Template> {
    commands::executor(&caller)?;
    let given: Vec<String> = roles
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let roles = parse_roles(&given, "roles").map_err(|e| {
        // A query parameter: invalid_argument, like other bad query values.
        match crate::error::classify(&e) {
            Some(a) => ApiError::new(ErrorCode::InvalidArgument, a.message.clone())
                .with_details(json!({"path": "roles", "kind": "unknown_role"}))
                .into(),
            None => e,
        }
    })?;
    Ok(template_view(roles))
}

// ---------------------------------------------------------------- the row

struct Row {
    roles: Vec<String>,
    onboarded: bool,
    theme: String,
    density: String,
    widgets: Vec<Stored>,
    shortcuts: Vec<Shortcut>,
    default_views: BTreeMap<String, String>,
    lock_version: i32,
    home_lock_version: i32,
}

type RawRow = (
    Vec<String>,
    bool,
    String,
    String,
    Json,
    Json,
    Json,
    i32,
    i32,
);

/// The first read creates the row from the general template (§2.8).
async fn ensure(conn: &mut PgConnection, user: Uuid) -> Result<()> {
    let (widgets, shortcuts, default) = build_template(&[]);
    sqlx::query(
        "INSERT INTO user_preferences (user_id, home, nav_shortcuts, default_views)
         VALUES ($1, $2, $3, $4) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user)
    .bind(json!({"widgets": widgets}))
    .bind(serde_json::to_value(&shortcuts)?)
    .bind(json!({"*": default}))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn load_row(conn: &mut PgConnection, user: Uuid, lock: bool) -> Result<Row> {
    let sql = format!(
        "SELECT interface_roles, onboarded_at IS NOT NULL, theme, density, home, nav_shortcuts,
                default_views, lock_version, home_lock_version
         FROM user_preferences WHERE user_id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    let r: RawRow = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Row {
        roles: r.0,
        onboarded: r.1,
        theme: r.2,
        density: r.3,
        widgets: serde_json::from_value(r.4["widgets"].clone())?,
        shortcuts: serde_json::from_value(r.5)?,
        default_views: serde_json::from_value(r.6)?,
        lock_version: r.7,
        home_lock_version: r.8,
    })
}

/// Projects `caller` can see (for default views).
async fn visible_projects(conn: &mut PgConnection, caller: AuthContext) -> Result<BTreeSet<Uuid>> {
    Ok(db::load_scope(&mut *conn, caller)
        .await?
        .project_ids()
        .collect())
}

async fn project_names(conn: &mut PgConnection, id: Uuid) -> Result<(String, String)> {
    Ok(
        sqlx::query_as("SELECT key, name FROM projects WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// A widget as `caller` reads it now (every reference checked again).
async fn render_widget(conn: &mut PgConnection, caller: AuthContext, s: &Stored) -> Result<Widget> {
    let mut out = Widget {
        id: s.id.clone(),
        widget_type: s.kind.clone(),
        width: s.width,
        view_id: s.view_id.clone(),
        limit: s.limit,
        project_id: s.project_id,
        protected: s.kind == PROTECTED,
        available: true,
        view_name: None,
        view_kind: None,
        project_key: None,
        project_name: None,
    };
    match s.kind.as_str() {
        "view" => {
            match views::usable(&mut *conn, caller, s.view_id.as_deref().unwrap_or("")).await? {
                Some(u) => {
                    out.view_name = Some(u.name);
                    out.view_kind = Some(u.kind);
                }
                None => out.available = false,
            }
        }
        "milestones" => {
            if let Some(p) = s.project_id {
                if crate::versions::readable(&mut *conn, caller, p).await? {
                    let (key, name) = project_names(&mut *conn, p).await?;
                    out.project_key = Some(key);
                    out.project_name = Some(name);
                } else {
                    out.available = false;
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

async fn render_home(conn: &mut PgConnection, caller: AuthContext, r: &Row) -> Result<HomeView> {
    let mut widgets = Vec::with_capacity(r.widgets.len());
    for s in &r.widgets {
        widgets.push(render_widget(&mut *conn, caller, s).await?);
    }
    Ok(HomeView {
        lock_version: r.home_lock_version,
        widgets,
    })
}

async fn render_prefs(
    conn: &mut PgConnection,
    caller: AuthContext,
    r: &Row,
) -> Result<PreferencesView> {
    let mut nav = Vec::with_capacity(r.shortcuts.len());
    for s in &r.shortcuts {
        let (available, name) = if s.kind == "view" {
            match views::usable(&mut *conn, caller, s.id.as_deref().unwrap_or("")).await? {
                Some(u) => (true, Some(u.name)),
                None => (false, None),
            }
        } else {
            (true, None)
        };
        nav.push(ShortcutView {
            kind: s.kind.clone(),
            id: s.id.clone(),
            available,
            name,
        });
    }
    let projects = visible_projects(&mut *conn, caller).await?;
    let mut defaults = BTreeMap::new();
    for (k, v) in &r.default_views {
        let project_ok = k == "*" || Uuid::parse_str(k).is_ok_and(|p| projects.contains(&p));
        let (available, name) = if !project_ok {
            (false, None)
        } else if DEFAULT_VIEW_BUILTINS.contains(&v.as_str()) {
            (true, None)
        } else {
            match views::usable(&mut *conn, caller, v).await? {
                Some(u) => (true, Some(u.name)),
                None => (false, None),
            }
        };
        defaults.insert(
            k.clone(),
            DefaultView {
                view_id: v.clone(),
                available,
                name,
            },
        );
    }
    Ok(PreferencesView {
        interface_roles: r.roles.clone(),
        onboarded: r.onboarded,
        theme: r.theme.clone(),
        density: r.density.clone(),
        nav_shortcuts: nav,
        default_views: defaults,
        lock_version: r.lock_version,
    })
}

// ---------------------------------------------------------------- reads

async fn ensure_committed(app: &App, user: Uuid) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    ensure(&mut tx, user).await?;
    tx.commit().await?;
    Ok(())
}

/// `GET /me/preferences`: creates the row on the first read (not a user
/// write: no Idempotency-Key).
pub async fn preferences(app: &App, caller: AuthContext) -> Result<PreferencesView> {
    let user = commands::executor(&caller)?.user_id;
    ensure_committed(app, user).await?;
    let mut tx = read_tx(app).await?;
    let r = load_row(&mut tx, user, false).await?;
    let v = render_prefs(&mut tx, caller, &r).await?;
    tx.commit().await?;
    Ok(v)
}

/// `GET /me/home`.
pub async fn home(app: &App, caller: AuthContext) -> Result<HomeView> {
    let user = commands::executor(&caller)?.user_id;
    ensure_committed(app, user).await?;
    let mut tx = read_tx(app).await?;
    let r = load_row(&mut tx, user, false).await?;
    let v = render_home(&mut tx, caller, &r).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------- writes

async fn begin(app: &App) -> Result<sqlx::Transaction<'static, Postgres>> {
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
            "your settings are busy; send the same request again",
        );
    }
    e
}

fn hash(v: &impl Serialize) -> Result<[u8; 32]> {
    Ok(Sha256::digest(serde_json::to_vec(v)?).into())
}

/// Step 0 of a write (§6.8): `Some(user)` to go on, or `None` when the key
/// was used for the same request (the caller answers with the current
/// state); a different request under the key is `idempotency_key_reused`.
async fn open(
    conn: &mut PgConnection,
    caller: AuthContext,
    operation_id: Uuid,
    action: &str,
    input: &impl Serialize,
) -> Result<Option<(Uuid, Uuid)>> {
    let who = commands::executor(&caller)?;
    let fp = commands::fingerprint(action, &hash(input)?);
    let Some(request_id) =
        commands::open_request(&mut *conn, who.user_id, operation_id, action, &fp).await?
    else {
        commands::stored_result(&mut *conn, who.user_id, operation_id, action, &fp).await?;
        return Ok(None);
    };
    commands::lock_executor(&mut *conn, &who).await?;
    Ok(Some((who.user_id, request_id)))
}

fn view_id_too_long(path: &str, v: &str) -> Result<()> {
    if v.len() > MAX_VIEW_ID {
        return Err(invalid(
            path,
            "invalid_view_id",
            format!("a view id is at most {MAX_VIEW_ID} characters"),
        ));
    }
    Ok(())
}

fn widget_id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn new_widget_id(taken: &BTreeSet<String>) -> String {
    loop {
        // The random tail of a v7 id (the head is the clock).
        let id = format!("w_{}", &Uuid::now_v7().simple().to_string()[24..]);
        if !taken.contains(&id) {
            return id;
        }
    }
}

/// Checks a whole widget list. A reference (view, project) that is new or
/// changed must be readable now (`unknown_view` / `unknown_project`, the
/// same for "does not exist" and "cannot see"); one the stored widget of
/// the same id already had is kept as it is, so a home whose view went
/// away can still be rearranged (it reads `available: false`).
async fn check_widgets(
    conn: &mut PgConnection,
    caller: AuthContext,
    input: &[WidgetInput],
    current: &[Stored],
) -> Result<Vec<Stored>> {
    if input.len() > MAX_WIDGETS {
        return Err(invalid(
            "widgets",
            "too_many_widgets",
            format!("at most {MAX_WIDGETS} widgets"),
        ));
    }
    let mut ids: BTreeSet<String> = input.iter().filter_map(|w| w.id.clone()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(input.len());
    let mut protected = 0;
    for (i, x) in input.iter().enumerate() {
        let path = format!("widgets[{i}]");
        let kind = x.widget_type.trim();
        if !WIDGET_TYPES.contains(&kind) {
            return Err(invalid(
                &format!("{path}.type"),
                "unknown_widget_type",
                format!("widget types are {WIDGET_TYPES:?}"),
            ));
        }
        let id = match &x.id {
            Some(id) => {
                if !widget_id_ok(id) {
                    return Err(invalid(
                        &format!("{path}.id"),
                        "invalid_widget_id",
                        "a widget id is 1–40 letters, digits, _ and -",
                    ));
                }
                id.clone()
            }
            None => {
                let id = new_widget_id(&ids);
                ids.insert(id.clone());
                id
            }
        };
        if !seen.insert(id.clone()) {
            return Err(invalid(
                &format!("{path}.id"),
                "duplicate_widget_id",
                "widget ids are unique",
            ));
        }
        if kind == PROTECTED {
            protected += 1;
            if protected > 1 {
                return Err(invalid(
                    &path,
                    "duplicate_protected_widget",
                    "the home has exactly one need_decision widget",
                ));
            }
        }
        let width = x.width.unwrap_or(if kind == PROTECTED { 2 } else { 1 });
        if !(1..=3).contains(&width) {
            return Err(invalid(
                &format!("{path}.width"),
                "width_out_of_range",
                "width is 1–3",
            ));
        }
        let unexpected = |field: &str| {
            invalid(
                &format!("{path}.{field}"),
                "unexpected_property",
                format!("{kind} widgets have no {field}"),
            )
        };
        if kind != "view" && x.view_id.is_some() {
            return Err(unexpected("view_id"));
        }
        if kind != "view" && x.limit.is_some() {
            return Err(unexpected("limit"));
        }
        if kind != "milestones" && x.project_id.is_some() {
            return Err(unexpected("project_id"));
        }
        let before = current.iter().find(|c| c.id == id && c.kind == kind);
        let mut s = Stored {
            id,
            kind: kind.to_string(),
            width,
            view_id: None,
            limit: None,
            project_id: None,
        };
        if kind == "view" {
            let Some(v) = x
                .view_id
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                return Err(invalid(
                    &format!("{path}.view_id"),
                    "view_required",
                    "a view widget names a view",
                ));
            };
            let limit = x.limit.unwrap_or(DEFAULT_VIEW_LIMIT);
            if !(VIEW_LIMIT.0..=VIEW_LIMIT.1).contains(&limit) {
                return Err(invalid(
                    &format!("{path}.limit"),
                    "limit_out_of_range",
                    "limit is 3–20",
                ));
            }
            view_id_too_long(&format!("{path}.view_id"), v)?;
            // Kept only when this widget already named this very view.
            let kept = before.is_some_and(|b| b.view_id.as_deref() == Some(v));
            let id = if kept {
                v.to_string()
            } else {
                match views::usable(&mut *conn, caller, v).await? {
                    Some(u) => u.id,
                    None => {
                        return Err(invalid(
                            &format!("{path}.view_id"),
                            "unknown_view",
                            "no such view",
                        ));
                    }
                }
            };
            s.view_id = Some(id);
            s.limit = Some(limit);
        }
        if kind == "milestones"
            && let Some(p) = x
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
        {
            let unknown = || {
                invalid(
                    &format!("{path}.project_id"),
                    "unknown_project",
                    "no such project",
                )
            };
            let kept = before
                .and_then(|b| b.project_id)
                .filter(|b| Uuid::parse_str(p).is_ok_and(|u| u == *b));
            let pid = match kept {
                Some(b) => b,
                None => {
                    let pid = describe::visible_project(&mut *conn, caller, p)
                        .await
                        .map_err(|_| unknown())?;
                    if !crate::versions::readable(&mut *conn, caller, pid).await? {
                        return Err(unknown());
                    }
                    pid
                }
            };
            s.project_id = Some(pid);
        }
        out.push(s);
    }
    if protected == 0 {
        return Err(invalid(
            "widgets",
            "protected_widget_missing",
            "the need_decision widget cannot be removed; move or resize it instead",
        ));
    }
    Ok(out)
}

/// Checks shortcuts: new or changed `view` ids must be readable now.
async fn check_shortcuts(
    conn: &mut PgConnection,
    caller: AuthContext,
    input: &[ShortcutInput],
    current: &[Shortcut],
) -> Result<Vec<Shortcut>> {
    if input.len() > MAX_SHORTCUTS {
        return Err(invalid(
            "nav_shortcuts",
            "too_many_shortcuts",
            format!("at most {MAX_SHORTCUTS} shortcuts"),
        ));
    }
    let mut out: Vec<Shortcut> = Vec::with_capacity(input.len());
    for (i, x) in input.iter().enumerate() {
        let path = format!("nav_shortcuts[{i}]");
        let kind = x.kind.trim();
        if !SHORTCUT_KINDS.contains(&kind) {
            return Err(invalid(
                &format!("{path}.kind"),
                "unknown_shortcut_kind",
                format!("shortcut kinds are {SHORTCUT_KINDS:?}"),
            ));
        }
        let id = x.id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let s = match (kind, id) {
            ("view", None) => {
                return Err(invalid(
                    &format!("{path}.id"),
                    "view_required",
                    "a view shortcut names a view",
                ));
            }
            ("view", Some(v)) => {
                view_id_too_long(&format!("{path}.id"), v)?;
                let s = sc("view", Some(v));
                if current.contains(&s) {
                    s
                } else {
                    match views::usable(&mut *conn, caller, v).await? {
                        Some(u) => sc("view", Some(&u.id)),
                        None => {
                            return Err(invalid(
                                &format!("{path}.id"),
                                "unknown_view",
                                "no such view",
                            ));
                        }
                    }
                }
            }
            (_, Some(_)) => {
                return Err(invalid(
                    &format!("{path}.id"),
                    "unexpected_property",
                    format!("{kind} shortcuts have no id"),
                ));
            }
            (_, None) => sc(kind, None),
        };
        if out.contains(&s) {
            return Err(invalid(&path, "duplicate_shortcut", "shortcuts are unique"));
        }
        out.push(s);
    }
    Ok(out)
}

/// Checks default views. Keys are project ids (any spelling of the uuid;
/// two spellings of one project are `duplicate_default_view`) or `*`. A
/// new or changed entry needs a project the caller can see
/// (`unknown_project`) and either a built-in (`builtin:list`, `board`,
/// `timeline`) or a view they can open that is bound to exactly that
/// project (`unknown_view`); `*` takes the built-ins or a view bound to no
/// project. Views are stored by their canonical id. An entry sent back
/// unchanged is kept even if it is no longer available.
async fn check_default_views(
    conn: &mut PgConnection,
    caller: AuthContext,
    input: &BTreeMap<String, DefaultViewInput>,
    current: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    if input.len() > MAX_DEFAULT_VIEWS {
        return Err(invalid(
            "default_views",
            "too_many_default_views",
            format!("at most {MAX_DEFAULT_VIEWS} default views"),
        ));
    }
    let projects = visible_projects(&mut *conn, caller).await?;
    let mut out = BTreeMap::new();
    for (k, v) in input {
        let path = format!("default_views.{k}");
        let v = v.view_id().trim();
        view_id_too_long(&path, v)?;
        let (key, project) = if k.trim() == "*" {
            ("*".to_string(), None)
        } else {
            match Uuid::parse_str(k.trim()) {
                Ok(p) => (p.to_string(), Some(p)),
                Err(_) => {
                    return Err(invalid(&path, "unknown_project", "no such project"));
                }
            }
        };
        if out.contains_key(&key) {
            return Err(invalid(
                &path,
                "duplicate_default_view",
                "one default view per project",
            ));
        }
        if current.get(&key).is_some_and(|c| c == v) {
            out.insert(key, v.to_string());
            continue;
        }
        if project.is_some_and(|p| !projects.contains(&p)) {
            return Err(invalid(&path, "unknown_project", "no such project"));
        }
        let stored = if DEFAULT_VIEW_BUILTINS.contains(&v) {
            v.to_string()
        } else {
            match views::usable(&mut *conn, caller, v).await? {
                Some(u) if u.project == project => u.id,
                Some(_) if project.is_none() => {
                    return Err(invalid(
                        &path,
                        "invalid_default_view",
                        "the default for every project is a built-in or a view bound to no project",
                    ));
                }
                _ => return Err(invalid(&path, "unknown_view", "no such view")),
            }
        };
        out.insert(key, stored);
    }
    Ok(out)
}

fn conflict(what: &str, current_lock_version: i32, current: Json) -> anyhow::Error {
    ApiError::new(
        ErrorCode::Conflict,
        format!("{what} changed elsewhere; here is the current version"),
    )
    .with_details(json!({"current_lock_version": current_lock_version, "current": current}))
    .into()
}

/// `PUT /me/preferences` (Idempotency-Key, `lock_version`).
pub async fn put_preferences(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &PreferencesPut,
) -> Result<PreferencesView> {
    let mut tx = begin(app).await?;
    let r = async {
        let Some((user, request_id)) =
            open(&mut tx, caller, operation_id, ACTION_PREFS, input).await?
        else {
            let user = commands::executor(&caller)?.user_id;
            ensure(&mut tx, user).await?;
            let r = load_row(&mut tx, user, false).await?;
            return render_prefs(&mut tx, caller, &r).await;
        };
        ensure(&mut tx, user).await?;
        let r = load_row(&mut tx, user, true).await?;
        if input.lock_version != r.lock_version {
            let cur = render_prefs(&mut tx, caller, &r).await?;
            return Err(conflict(
                "the preferences",
                r.lock_version,
                serde_json::to_value(cur)?,
            ));
        }
        let roles = match &input.interface_roles {
            Some(x) => parse_roles(x, "interface_roles")?,
            None => r.roles.clone(),
        };
        let theme = match input.theme.as_deref().map(str::trim) {
            Some(t) if !THEMES.contains(&t) => {
                return Err(invalid(
                    "theme",
                    "invalid_theme",
                    format!("theme is one of {THEMES:?}"),
                ));
            }
            Some(t) => t.to_string(),
            None => r.theme.clone(),
        };
        let density = match input.density.as_deref().map(str::trim) {
            Some(d) if !DENSITIES.contains(&d) => {
                return Err(invalid(
                    "density",
                    "invalid_density",
                    format!("density is one of {DENSITIES:?}"),
                ));
            }
            Some(d) => d.to_string(),
            None => r.density.clone(),
        };
        let shortcuts = match &input.nav_shortcuts {
            Some(x) => check_shortcuts(&mut tx, caller, x, &r.shortcuts).await?,
            None => r.shortcuts.clone(),
        };
        let defaults = match &input.default_views {
            Some(x) => check_default_views(&mut tx, caller, x, &r.default_views).await?,
            None => r.default_views.clone(),
        };
        let onboarded = input.onboarded.unwrap_or(r.onboarded);
        let changed = roles != r.roles
            || theme != r.theme
            || density != r.density
            || shortcuts != r.shortcuts
            || defaults != r.default_views
            || onboarded != r.onboarded;
        if changed {
            sqlx::query(
                "UPDATE user_preferences SET interface_roles = $2, theme = $3, density = $4,
                        nav_shortcuts = $5, default_views = $6,
                        onboarded_at = CASE WHEN $7 THEN COALESCE(onboarded_at, now()) END,
                        lock_version = lock_version + 1, updated_at = now()
                 WHERE user_id = $1",
            )
            .bind(user)
            .bind(&roles)
            .bind(&theme)
            .bind(&density)
            .bind(serde_json::to_value(&shortcuts)?)
            .bind(serde_json::to_value(&defaults)?)
            .bind(onboarded)
            .execute(&mut *tx)
            .await?;
        }
        commands::complete_request(&mut tx, request_id, &json!({})).await?;
        let r = load_row(&mut tx, user, false).await?;
        render_prefs(&mut tx, caller, &r).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// `PUT /me/home` (Idempotency-Key, `lock_version`): the whole widget list.
pub async fn put_home(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &HomePut,
) -> Result<HomeView> {
    let mut tx = begin(app).await?;
    let r = async {
        let Some((user, request_id)) =
            open(&mut tx, caller, operation_id, ACTION_HOME, input).await?
        else {
            let user = commands::executor(&caller)?.user_id;
            ensure(&mut tx, user).await?;
            let r = load_row(&mut tx, user, false).await?;
            return render_home(&mut tx, caller, &r).await;
        };
        ensure(&mut tx, user).await?;
        let r = load_row(&mut tx, user, true).await?;
        if input.lock_version != r.home_lock_version {
            let cur = render_home(&mut tx, caller, &r).await?;
            return Err(conflict(
                "the home",
                r.home_lock_version,
                serde_json::to_value(cur)?,
            ));
        }
        let widgets = check_widgets(&mut tx, caller, &input.widgets, &r.widgets).await?;
        if widgets != r.widgets {
            sqlx::query(
                "UPDATE user_preferences SET home = $2, home_lock_version = home_lock_version + 1,
                        updated_at = now()
                 WHERE user_id = $1",
            )
            .bind(user)
            .bind(json!({"widgets": widgets}))
            .execute(&mut *tx)
            .await?;
        }
        commands::complete_request(&mut tx, request_id, &json!({})).await?;
        let r = load_row(&mut tx, user, false).await?;
        render_home(&mut tx, caller, &r).await
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

/// The widgets after applying a template (§3.8): `replace` takes the
/// template, keeping the person's own `need_decision` (its id and width);
/// `merge` adds the template's widgets that are not there yet, up to
/// [`MAX_WIDGETS`]. Ids that clash with kept widgets are renewed.
fn applied_widgets(current: &[Stored], template: &[Stored], replace: bool) -> Vec<Stored> {
    let mut out: Vec<Stored> = if replace {
        let own = current.iter().find(|c| c.kind == PROTECTED);
        template
            .iter()
            .map(|t| match own {
                Some(o) if t.kind == PROTECTED => o.clone(),
                _ => t.clone(),
            })
            .collect()
    } else {
        current.to_vec()
    };
    if !replace {
        for t in template {
            if out.len() >= MAX_WIDGETS {
                break;
            }
            if !out.iter().any(|c| c.same(t)) {
                out.push(t.clone());
            }
        }
    }
    if !out.iter().any(|c| c.kind == PROTECTED) {
        // Never without the protected widget (a stored home always has it;
        // this keeps a broken row from staying broken).
        out.insert(0, w("w_decide", PROTECTED, 2));
        out.truncate(MAX_WIDGETS);
    }
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for x in &mut out {
        if !taken.insert(x.id.clone()) {
            let id = new_widget_id(&taken);
            taken.insert(id.clone());
            x.id = id;
        }
    }
    out
}

/// `POST /me/home/apply-template` (Idempotency-Key): the roles' defaults
/// for the home, the left-nav shortcuts and the `*` default view. `replace`
/// takes them (`need_decision` always stays; per-project default views
/// stay); `merge` only adds what is missing.
pub async fn apply_template(
    app: &App,
    caller: AuthContext,
    operation_id: Uuid,
    input: &ApplyTemplate,
) -> Result<Applied> {
    let mut tx = begin(app).await?;
    let r = async {
        let replace = match input.mode.trim() {
            "replace" => true,
            "merge" => false,
            _ => return Err(invalid("mode", "invalid_mode", "mode is replace or merge")),
        };
        let roles = parse_roles(&input.roles, "roles")?;
        let done = open(&mut tx, caller, operation_id, ACTION_APPLY, input).await?;
        let user = commands::executor(&caller)?.user_id;
        ensure(&mut tx, user).await?;
        if let Some((_, request_id)) = done {
            let r = load_row(&mut tx, user, true).await?;
            let (tw, ts, td) = build_template(&roles);
            let widgets = applied_widgets(&r.widgets, &tw, replace);
            let mut shortcuts = if replace {
                Vec::new()
            } else {
                r.shortcuts.clone()
            };
            for s in ts {
                if shortcuts.len() < MAX_SHORTCUTS && !shortcuts.contains(&s) {
                    shortcuts.push(s);
                }
            }
            let mut defaults = r.default_views.clone();
            if replace || !defaults.contains_key("*") {
                defaults.insert("*".into(), td.into());
            }
            if widgets != r.widgets {
                sqlx::query(
                    "UPDATE user_preferences SET home = $2,
                            home_lock_version = home_lock_version + 1, updated_at = now()
                     WHERE user_id = $1",
                )
                .bind(user)
                .bind(json!({"widgets": widgets}))
                .execute(&mut *tx)
                .await?;
            }
            if shortcuts != r.shortcuts || defaults != r.default_views {
                sqlx::query(
                    "UPDATE user_preferences SET nav_shortcuts = $2, default_views = $3,
                            lock_version = lock_version + 1, updated_at = now()
                     WHERE user_id = $1",
                )
                .bind(user)
                .bind(serde_json::to_value(&shortcuts)?)
                .bind(serde_json::to_value(&defaults)?)
                .execute(&mut *tx)
                .await?;
            }
            commands::complete_request(&mut tx, request_id, &json!({})).await?;
        }
        let r = load_row(&mut tx, user, false).await?;
        Ok(Applied {
            home: render_home(&mut tx, caller, &r).await?,
            preferences: render_prefs(&mut tx, caller, &r).await?,
        })
    }
    .await
    .map_err(busy)?;
    tx.commit().await?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_round_robin() {
        let (w, s, d) = build_template(&["pm".into(), "engineer".into()]);
        let kinds: Vec<_> = w
            .iter()
            .map(|x| (x.kind.as_str(), x.view_id.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("need_decision", None),
                ("triage_count", None),
                ("view", Some("builtin:assigned_to_me")),
                ("milestones", None),
                ("my_time_week", None),
                ("view", Some("builtin:involving_me")),
                ("agent_summary", None),
            ]
        );
        assert_eq!(d, "builtin:timeline");
        assert_eq!(s.len(), 4);
        let (w, _, _) = build_template(&ROLES.map(String::from));
        assert!(w.len() <= TEMPLATE_MAX_WIDGETS);
        assert_eq!(w.iter().filter(|x| x.kind == PROTECTED).count(), 1);
        for r in [None, Some("pm"), Some("designer"), Some("engineer")] {
            let (w, _, _) = role_template(r);
            assert_eq!(w[0].kind, PROTECTED);
        }
    }

    #[test]
    fn replace_keeps_own_need_decision() {
        let mine = vec![w("mine", PROTECTED, 3), w("w_triage", "agent_summary", 1)];
        let (t, _, _) = build_template(&["pm".into()]);
        let out = applied_widgets(&mine, &t, true);
        assert_eq!(out[0], mine[0]);
        assert_eq!(out.len(), t.len());
        let merged = applied_widgets(&mine, &t, false);
        // agent_summary kept; the template's triage_count gets a new id.
        assert_eq!(merged.len(), 2 + 3);
        let ids: BTreeSet<_> = merged.iter().map(|x| x.id.clone()).collect();
        assert_eq!(ids.len(), merged.len());
    }
}
