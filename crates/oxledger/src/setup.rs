// SPDX-License-Identifier: GPL-2.0-or-later
//! Site setup from the command line (deploy/README.md), until there is an
//! administration UI: the default configuration (roles, trackers, statuses,
//! workflow, as Redmine's "load the default configuration"), projects and
//! memberships.
//!
//! Plain SQL is enough here: the triggers of migrations 0007, 0010, 0015,
//! 0022, 0023 and 0034 create every project's setting rows and move the
//! security epochs (a new member, a module, a permission), exactly as for
//! any other write of these tables.

use crate::db::ISSUE_MODULE;
use crate::perm::*;
use crate::time_entries::TIME_MODULE;
use crate::wiki::WIKI_MODULE;
use anyhow::{Context, Result, bail};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// The modules `project add` turns on unless told otherwise (design 06
/// B-9: new projects have the wiki on).
pub const DEFAULT_MODULES: [&str; 3] = [ISSUE_MODULE, TIME_MODULE, WIKI_MODULE];

/// The statuses: name, category, English alias (search, R-D15 aliases).
const STATUSES: [(&str, &str, &str); 6] = [
    ("新建", "todo", "new"),
    ("進行中", "doing", "in progress"),
    ("回饋", "doing", "feedback"),
    ("已解決", "done", "resolved"),
    ("已關閉", "done", "closed"),
    ("已拒絕", "done", "rejected"),
];

/// The trackers, all starting at 新建: name, English alias.
const TRACKERS: [(&str, &str); 3] = [("缺陷", "bug"), ("功能", "feature"), ("支援", "support")];

pub const MANAGER: &str = "管理者";
pub const DEVELOPER: &str = "開發者";
pub const REPORTER: &str = "回報者";

/// What each role may do. Every member role reads issues with the
/// `default` visibility or wider, so a project with the default roles is
/// one an agent may be delegated on (contract 02 §3.8: no `own` role).
fn role_permissions(role: &str) -> &'static [&'static str] {
    const MANAGER_PERMS: &[&str] = &[
        VIEW_ISSUES,
        ADD_ISSUES,
        EDIT_ISSUES,
        ADD_ISSUE_NOTES,
        SET_NOTES_PRIVATE,
        VIEW_PRIVATE_NOTES,
        SET_ISSUES_PRIVATE,
        MANAGE_ISSUE_RELATIONS,
        MANAGE_VERSIONS,
        MANAGE_PROJECT,
        MANAGE_PUBLIC_VIEWS,
        TRIAGE_ISSUES,
        VIEW_TIME_ENTRIES,
        LOG_TIME,
        EDIT_TIME_ENTRIES,
        EDIT_OWN_TIME_ENTRIES,
        LOG_TIME_FOR_OTHER_USERS,
        VIEW_WIKI_PAGES,
        VIEW_WIKI_EDITS,
        EDIT_WIKI_PAGES,
        RENAME_WIKI_PAGES,
        DELETE_WIKI_PAGES,
        PROTECT_WIKI_PAGES,
        MANAGE_WIKI,
        IMPORT_ISSUES,
        IMPORT_TIME_ENTRIES,
    ];
    const DEVELOPER_PERMS: &[&str] = &[
        VIEW_ISSUES,
        ADD_ISSUES,
        EDIT_ISSUES,
        ADD_ISSUE_NOTES,
        SET_NOTES_PRIVATE,
        VIEW_PRIVATE_NOTES,
        SET_ISSUES_PRIVATE,
        MANAGE_ISSUE_RELATIONS,
        VIEW_TIME_ENTRIES,
        LOG_TIME,
        EDIT_OWN_TIME_ENTRIES,
        VIEW_WIKI_PAGES,
        VIEW_WIKI_EDITS,
        EDIT_WIKI_PAGES,
    ];
    const REPORTER_PERMS: &[&str] = &[
        VIEW_ISSUES,
        ADD_ISSUES,
        ADD_ISSUE_NOTES,
        VIEW_TIME_ENTRIES,
        LOG_TIME,
        VIEW_WIKI_PAGES,
        VIEW_WIKI_EDITS,
    ];
    const NON_MEMBER_PERMS: &[&str] = &[
        VIEW_ISSUES,
        ADD_ISSUES,
        ADD_ISSUE_NOTES,
        VIEW_WIKI_PAGES,
        VIEW_WIKI_EDITS,
    ];
    const ANONYMOUS_PERMS: &[&str] = &[VIEW_ISSUES, VIEW_WIKI_PAGES, VIEW_WIKI_EDITS];
    match role {
        MANAGER => MANAGER_PERMS,
        DEVELOPER => DEVELOPER_PERMS,
        REPORTER => REPORTER_PERMS,
        "非成員" => NON_MEMBER_PERMS,
        "匿名" => ANONYMOUS_PERMS,
        _ => &[],
    }
}

/// The roles: name, built-in kind, issue visibility, position.
const ROLES: [(&str, Option<&str>, &str, i32); 5] = [
    (MANAGER, None, "all", 1),
    (DEVELOPER, None, "default", 2),
    (REPORTER, None, "default", 3),
    ("非成員", Some("non_member"), "default", 10),
    ("匿名", Some("anonymous"), "default", 11),
];

/// The workflow (old status, new status) each member role may take, in
/// every tracker. Managers move between any two statuses; developers work
/// an issue up to 已解決 and reopen it, but do not close or reject.
/// Reporters have none: changing a status needs edit_issues, which they
/// lack (they file issues and add notes).
fn transitions(role: &str) -> Vec<(&'static str, &'static str)> {
    let all: Vec<&str> = STATUSES.iter().map(|s| s.0).collect();
    match role {
        MANAGER => all
            .iter()
            .flat_map(|o| all.iter().filter(move |n| n != &o).map(move |n| (*o, *n)))
            .collect(),
        DEVELOPER => {
            let open = ["新建", "進行中", "回饋", "已解決"];
            open.iter()
                .flat_map(|o| open.iter().filter(move |n| n != &o).map(move |n| (*o, *n)))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Loads the default roles, trackers, statuses and workflow into a site
/// that has none of them. Refuses (changing nothing) when any role,
/// tracker or status exists already, so it cannot mix with a
/// configuration someone made.
pub async fn init_defaults(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    // One setup at a time: two runs side by side would both see empty tables.
    sqlx::query("LOCK TABLE roles, trackers, issue_statuses IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let existing: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM roles) + (SELECT count(*) FROM trackers)
              + (SELECT count(*) FROM issue_statuses)",
    )
    .fetch_one(&mut *tx)
    .await?;
    if existing > 0 {
        bail!(
            "this site already has roles, trackers or statuses; init-defaults only runs on a site \
             without any"
        );
    }
    for (pos, (name, category, alias)) in (1..).zip(STATUSES) {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO issue_statuses (name, category, position) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(name)
        .bind(category)
        .bind(pos)
        .fetch_one(&mut *tx)
        .await?;
        alias_row(&mut tx, "status", id, alias).await?;
    }
    for (pos, (name, alias)) in (1..).zip(TRACKERS) {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO trackers (name, position, default_status_id)
             SELECT $1, $2, id FROM issue_statuses WHERE name = '新建' RETURNING id",
        )
        .bind(name)
        .bind(pos)
        .fetch_one(&mut *tx)
        .await?;
        alias_row(&mut tx, "tracker", id, alias).await?;
    }
    for (name, builtin, visibility, pos) in ROLES {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO roles (name, builtin, issues_visibility, position) VALUES ($1, $2, $3, $4)
             RETURNING id",
        )
        .bind(name)
        .bind(builtin)
        .bind(visibility)
        .bind(pos)
        .fetch_one(&mut *tx)
        .await?;
        let perms: Vec<String> = role_permissions(name)
            .iter()
            .map(|p| p.to_string())
            .collect();
        sqlx::query(
            "INSERT INTO role_permissions (role_id, permission) SELECT $1, unnest($2::text[])",
        )
        .bind(id)
        .bind(&perms)
        .execute(&mut *tx)
        .await?;
        let (olds, news): (Vec<&str>, Vec<&str>) = transitions(name).into_iter().unzip();
        sqlx::query(
            "INSERT INTO workflow_transitions (tracker_id, role_id, old_status_id, new_status_id)
             SELECT t.id, $1, o.id, n.id
             FROM trackers t, unnest($2::text[], $3::text[]) AS w(old_name, new_name)
             JOIN issue_statuses o ON o.name = w.old_name
             JOIN issue_statuses n ON n.name = w.new_name",
        )
        .bind(id)
        .bind(&olds)
        .bind(&news)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn alias_row(conn: &mut PgConnection, kind: &str, id: Uuid, label: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, lang) VALUES ($1, $2, $3, 'en')",
    )
    .bind(kind)
    .bind(id)
    .bind(label)
    .execute(conn)
    .await?;
    Ok(())
}

/// A new project with every tracker and `modules` turned on. `key` is the
/// issue key prefix (2–10 of A–Z, 0–9, starting with a letter).
pub async fn add_project(
    pool: &PgPool,
    key: &str,
    name: &str,
    is_public: bool,
    modules: &[&str],
) -> Result<Uuid> {
    let key = key.trim();
    if !(2..=10).contains(&key.len())
        || !key.starts_with(|c: char| c.is_ascii_uppercase())
        || !key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        bail!("project key must be 2–10 of A–Z and 0–9, starting with a letter (e.g. WEB, APP2)");
    }
    let name = name.trim();
    if name.is_empty() {
        bail!("project name is empty");
    }
    for m in modules {
        if !DEFAULT_MODULES.contains(m) {
            bail!(
                "unknown module {m}; modules are {}",
                DEFAULT_MODULES.join(", ")
            );
        }
    }
    let mut modules: Vec<&str> = modules.to_vec();
    modules.sort_unstable();
    modules.dedup();
    if modules.is_empty() {
        bail!("name at least one module ({})", DEFAULT_MODULES.join(", "));
    }
    let mut tx = pool.begin().await?;
    let trackers: i64 = sqlx::query_scalar("SELECT count(*) FROM trackers")
        .fetch_one(&mut *tx)
        .await?;
    if trackers == 0 {
        bail!("the site has no trackers yet; run `oxledger setup init-defaults` first");
    }
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE key = $1)")
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
    if taken {
        bail!("a project with key {key} exists already");
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (key, name, is_public) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(key)
    .bind(name)
    .bind(is_public)
    .fetch_one(&mut *tx)
    .await
    .context("create project")?;
    let modules: Vec<String> = modules.iter().map(|m| m.to_string()).collect();
    sqlx::query("INSERT INTO enabled_modules (project_id, module) SELECT $1, unnest($2::text[])")
        .bind(id)
        .bind(&modules)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO project_trackers (project_id, tracker_id) SELECT $1, id FROM trackers",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Makes the user `login` a member of project `key` with `roles` (by
/// name; built-in roles are not member roles). A member already in the
/// project gets the roles added.
pub async fn add_member(pool: &PgPool, key: &str, login: &str, roles: &[&str]) -> Result<()> {
    if roles.is_empty() {
        bail!("name at least one role");
    }
    let mut tx = pool.begin().await?;
    let project: Option<Uuid> = sqlx::query_scalar("SELECT id FROM projects WHERE key = $1")
        .bind(key.trim())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(project) = project else {
        bail!("no project with key {key}");
    };
    let user: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM principals WHERE kind = 'user' AND login = $1")
            .bind(login.trim())
            .fetch_optional(&mut *tx)
            .await?;
    let Some(user) = user else {
        bail!("no user with login {login}");
    };
    let mut role_ids = Vec::new();
    for r in roles {
        let id: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM roles WHERE name = $1 AND builtin IS NULL")
                .bind(r.trim())
                .fetch_optional(&mut *tx)
                .await?;
        match id {
            Some(id) => role_ids.push(id),
            None => bail!("no member role named {r}; see `oxledger role list`"),
        }
    }
    let member: Uuid = sqlx::query_scalar(
        // DO UPDATE (a no-op) returns the row also when it exists, even one
        // a concurrent transaction committed after this one's snapshot.
        "INSERT INTO members (project_id, principal_id) VALUES ($1, $2)
         ON CONFLICT (project_id, principal_id) DO UPDATE SET project_id = EXCLUDED.project_id
         RETURNING id",
    )
    .bind(project)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO member_roles (member_id, role_id) SELECT $1, unnest($2::uuid[])
         ON CONFLICT DO NOTHING",
    )
    .bind(member)
    .bind(&role_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `key  name  public|private  status  modules`, one project per line.
pub async fn list_projects(pool: &PgPool) -> Result<Vec<String>> {
    let rows: Vec<(String, String, bool, String, Vec<String>)> = sqlx::query_as(
        "SELECT p.key, p.name, p.is_public, p.status,
                ARRAY(SELECT m.module FROM enabled_modules m WHERE m.project_id = p.id ORDER BY m.module)
         FROM projects p ORDER BY p.key",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(k, n, public, status, m)| {
            format!(
                "{k}\t{n}\t{}\t{status}\t{}",
                if public { "public" } else { "private" },
                m.join(",")
            )
        })
        .collect())
}

/// `name  [built-in]`, one role per line, in position order.
pub async fn list_roles(pool: &PgPool) -> Result<Vec<String>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, builtin FROM roles ORDER BY position, name")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(n, b)| match b {
            Some(b) => format!("{n}\t(built-in: {b})"),
            None => n,
        })
        .collect())
}

/// The password a CLI reads from stdin: without the line end, and without
/// the UTF-8 BOM Windows PowerShell puts before what it pipes to a native
/// program (nobody means it as part of a password).
pub fn password_from_input(s: &str) -> &str {
    s.strip_prefix('\u{feff}')
        .unwrap_or(s)
        .trim_end_matches(['\r', '\n'])
}

/// Every comma-separated item of `args`, trimmed, without empties and
/// repeats, in order. PowerShell passes an unquoted `a,b` as two
/// arguments, so a list may come as one argument or several.
pub fn split_list(args: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in args {
        for item in a.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|o| o == item) {
                out.push(item.to_string());
            }
        }
    }
    out
}

/// `project add`'s flags after the key and name: `--public`, and
/// `--modules` followed by a list ([`split_list`]: every argument up to
/// the next flag). Without `--modules`, [`DEFAULT_MODULES`].
pub fn parse_project_flags(rest: &[&str]) -> Result<(bool, Vec<String>)> {
    const USAGE: &str = "usage: oxledger project add <KEY> <name> [--public] \
                         [--modules issue_tracking,time_tracking,wiki]";
    let mut public = false;
    let mut modules: Option<Vec<String>> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--public" => {
                public = true;
                i += 1;
            }
            "--modules" => {
                let end = rest[i + 1..]
                    .iter()
                    .position(|a| a.starts_with("--"))
                    .map_or(rest.len(), |p| i + 1 + p);
                let list = split_list(&rest[i + 1..end]);
                if list.is_empty() {
                    bail!("--modules needs a comma-separated list; {USAGE}");
                }
                modules = Some(list);
                i = end;
            }
            _ => bail!("{USAGE}"),
        }
    }
    Ok((
        public,
        modules.unwrap_or_else(|| DEFAULT_MODULES.iter().map(|m| m.to_string()).collect()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_input_loses_the_bom_and_line_end_only() {
        assert_eq!(
            password_from_input("\u{feff}secret pass 1\r\n"),
            "secret pass 1"
        );
        assert_eq!(password_from_input("secret pass 1\n"), "secret pass 1");
        assert_eq!(password_from_input("\u{feff}"), "");
        assert_eq!(password_from_input(" a b \u{feff}\n\n"), " a b \u{feff}");
        assert_eq!(password_from_input("\u{feff}\u{feff}x"), "\u{feff}x");
    }

    #[test]
    fn lists_come_as_one_argument_or_several() {
        assert_eq!(split_list(&["開發者,回報者"]), ["開發者", "回報者"]);
        assert_eq!(split_list(&["開發者", "回報者"]), ["開發者", "回報者"]);
        assert_eq!(split_list(&[" a , ,b", "a", "c,"]), ["a", "b", "c"]);
        assert!(split_list(&[","]).is_empty());
    }

    #[test]
    fn project_flags() {
        let all: Vec<String> = DEFAULT_MODULES.iter().map(|m| m.to_string()).collect();
        assert_eq!(parse_project_flags(&[]).unwrap(), (false, all.clone()));
        assert_eq!(parse_project_flags(&["--public"]).unwrap(), (true, all));
        assert_eq!(
            parse_project_flags(&["--modules", "issue_tracking", "wiki", "--public"]).unwrap(),
            (true, vec!["issue_tracking".to_string(), "wiki".to_string()])
        );
        assert_eq!(
            parse_project_flags(&["--modules", "wiki,wiki"]).unwrap().1,
            ["wiki"]
        );
        assert!(parse_project_flags(&["--modules"]).is_err());
        assert!(parse_project_flags(&["--modules", ","]).is_err());
        assert!(parse_project_flags(&["--modules", "--public"]).is_err());
        assert!(parse_project_flags(&["--bogus"]).is_err());
        assert!(parse_project_flags(&["wiki"]).is_err());
    }
}
