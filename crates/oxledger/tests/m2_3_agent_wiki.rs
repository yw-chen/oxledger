// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.3b, the reading half: what agents read of the wiki (§3.12
//! R-AG1, §4.1). T-M2-40 and T-M2-41 (reads), T-M2-42, 42a, 42b, 42c (reads),
//! T-M2-49, `wiki_list` / `wiki_get`, MCP `search` with `kinds`, `describe
//! agent_context`'s wiki status and the built-in AI's tool set. Needs
//! PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use common::test_pool;
use oxledger::agents;
use oxledger::api::{self, ApiConfig};
use oxledger::audience;
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ErrorCode, classify};
use oxledger::mcp::McpSearchParams;
use oxledger::registry;
use oxledger::service::SearchParams;
use oxledger::wiki::WikiEffect;
use oxledger::wiki::agent::{self as wa, WikiGetParams, WikiListParams};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use world::mcp_client::Mcp;
use world::*;

/// What the Dev role (alice, bob in WEB and AUTH; carol in WEB) holds of the
/// wiki: W(P) = H(P) = A(P).
const DEV_WIKI: [&str; 5] = [
    "view_wiki_pages",
    "view_wiki_edits",
    "edit_wiki_pages",
    "rename_wiki_pages",
    "delete_wiki_pages",
];

async fn role(pool: &PgPool, name: &str, visibility: &str, perms: &[&str]) -> Uuid {
    let r = id(
        pool,
        &format!(
            "INSERT INTO roles (name, issues_visibility) VALUES ('{name}','{visibility}') RETURNING id"
        ),
    )
    .await;
    grant(pool, r, perms).await;
    r
}

async fn user(pool: &PgPool, login: &str) -> Uuid {
    auth::create_user(pool, login, login, PW, false)
        .await
        .unwrap()
}

/// The world with the wiki module on in WEB and AUTH and Dev reading and
/// writing it.
async fn setup(pool: &PgPool) -> World {
    let w = World::build(pool).await;
    exec(
        pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(pool, w.dev, &DEV_WIKI).await;
    w
}

async fn run(w: &World, op: Value, pre: Option<Value>) -> CommandResult {
    let mut d = json!({"command_schema_version": 1, "ops": [op]});
    if let Some(p) = pre {
        d["preconditions"] = json!([p]);
    }
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
        .await
        .unwrap_or_else(|e| panic!("{d}: {e:#}"))
}

async fn page(
    w: &World,
    project: &str,
    title: &str,
    body: &str,
    parent: Option<&str>,
) -> WikiEffect {
    let mut op = json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
                        "target": {"project": project, "page": title}, "body": body});
    if let Some(p) = parent {
        op["parent"] = json!({"page": p});
    }
    run(w, op, None).await.wiki[0].clone()
}

async fn edit(w: &World, project: &str, e: &WikiEffect, body: &str) -> WikiEffect {
    let lv = count(
        &w.pool,
        &format!(
            "SELECT lock_version FROM wiki_pages WHERE id = '{}'",
            e.page_id
        ),
    )
    .await;
    run(
        w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_put",
               "target": {"project": project, "page_id": e.page_id}, "body": body}),
        Some(json!({"wiki_page": e.page_id, "lock_version": lv})),
    )
    .await
    .wiki[0]
        .clone()
}

/// alice delegates `projects` to the agent; its AuthContext.
async fn agent(w: &World, projects: &[Uuid]) -> AuthContext {
    let mut ctx = None;
    for p in projects {
        ctx = Some(w.delegate(*p).await.unwrap().context_id);
    }
    w.token(ctx.expect("a project")).await.1
}

fn get(project: &str, page: &str, version: Option<i32>) -> WikiGetParams {
    serde_json::from_value(json!({"project": project, "page": page, "version": version})).unwrap()
}

fn list(project: &str, under: Option<&str>, cursor: Option<&str>) -> WikiListParams {
    serde_json::from_value(json!({"project": project, "under": under, "cursor": cursor})).unwrap()
}

/// The error's code, message and details (no request id).
fn err_body(e: &anyhow::Error) -> Value {
    let a = classify(e).unwrap_or_else(|| panic!("not an API error: {e:#}"));
    json!({"code": a.code.as_str(), "message": a.message, "details": a.details})
}

fn reason(e: &anyhow::Error) -> String {
    classify(e)
        .and_then(|a| a.details)
        .and_then(|d| d.get("reason").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

async fn wiki_state(w: &World, agent: AuthContext, key: &str) -> (String, Option<String>) {
    let d = agents::describe_agent_context(&w.app, agent).await.unwrap();
    let p = d
        .context
        .projects
        .iter()
        .find(|p| p.project == key)
        .unwrap_or_else(|| panic!("{key} in the context"));
    (p.wiki.state.clone(), p.wiki.reason.clone())
}

// ---------------------------------------------------------------- T-M2-40 (reads)

/// T-M2-40, reading: W(P) = A(P). `wiki_list` (by title, `under`, paging),
/// `wiki_get` (current, an older version, by id, through a redirect), and
/// what is not there: a project not delegated, a missing page and a deleted
/// one are all `not_found`.
#[tokio::test]
async fn t40_agent_reads_a_delegated_wiki() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let home = page(&w, "WEB", "Home", "start here, see WEB-1", None).await;
    let child = page(&w, "WEB", "Child", "child text", Some("Home")).await;
    let grand = page(&w, "WEB", "Grand", "deep", Some("Child")).await;
    let old = page(&w, "WEB", "Old name", "renamed page", None).await;
    run(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_move",
               "target": {"project": "WEB", "page_id": old.page_id}, "title": "New name"}),
        None,
    )
    .await;
    let gone = page(&w, "WEB", "Gone", "deleted", None).await;
    run(
        &w,
        json!({"op_id": Uuid::now_v7(), "kind": "wiki_delete",
               "target": {"project": "WEB", "page_id": gone.page_id}}),
        None,
    )
    .await;
    page(&w, "AUTH", "Secret", "auth only", None).await;
    let home2 = edit(&w, "WEB", &home, "start here, version two").await;
    assert_eq!(home2.version, 2);
    let a = agent(&w, &[w.web]).await;

    let l = wa::list(&w.app, a, list("WEB", None, None)).await.unwrap();
    let titles: Vec<&str> = l.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["Child", "Grand", "Home", "New name"]);
    assert!(!l.truncated && l.next_cursor.is_none());
    assert_eq!(l.project, "WEB");
    let under: Vec<String> = wa::list(&w.app, a, list("WEB", Some("Home"), None))
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.title)
        .collect();
    assert_eq!(under, ["Child", "Grand"], "every page below Home");
    let by_id = wa::list(
        &w.app,
        a,
        list("WEB", Some(&child.page_id.to_string()), None),
    )
    .await
    .unwrap();
    assert_eq!(by_id.items.len(), 1);
    assert_eq!(by_id.items[0].page_id, grand.page_id);
    let e = wa::list(&w.app, a, list("WEB", None, Some("nope")))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);

    let g = wa::get(&w.app, a, get("WEB", "Home", None)).await.unwrap();
    assert_eq!(g.page_id, home.page_id);
    assert_eq!(g.body, "start here, version two");
    assert_eq!(g.format, "markdown");
    assert_eq!((g.version, g.current_version), (2, 2));
    assert_eq!(g.lock_version, home2.lock_version);
    assert!(!g.protected && !g.redacted && g.parent.is_none());
    assert_eq!(
        g.children.iter().map(|c| c.page_id).collect::<Vec<_>>(),
        [child.page_id]
    );
    assert_eq!(
        g.updated_by.as_ref().map(|p| p.name.as_str()),
        Some("alice")
    );
    let v1 = wa::get(&w.app, a, get("WEB", "Home", Some(1)))
        .await
        .unwrap();
    assert_eq!(v1.body, "start here, see WEB-1");
    assert_eq!((v1.version, v1.current_version), (1, 2));
    let e = wa::get(&w.app, a, get("WEB", "Home", Some(9)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let c = wa::get(&w.app, a, get("WEB", &child.page_id.to_string(), None))
        .await
        .unwrap();
    assert_eq!(c.parent.map(|p| p.title), Some("Home".into()));
    let r = wa::get(&w.app, a, get("WEB", "Old_name", None))
        .await
        .unwrap();
    assert_eq!(r.page_id, old.page_id);
    assert_eq!(r.title, "New name");
    assert_eq!(r.redirected_from.as_deref(), Some("Old name"));

    // Not there for the agent: all the same not_found.
    let missing = wa::get(&w.app, a, get("WEB", "Nothing", None))
        .await
        .unwrap_err();
    let deleted = wa::get(&w.app, a, get("WEB", &gone.page_id.to_string(), None))
        .await
        .unwrap_err();
    let deleted_title = wa::get(&w.app, a, get("WEB", "Gone", None))
        .await
        .unwrap_err();
    let other = wa::get(&w.app, a, get("AUTH", "Secret", None))
        .await
        .unwrap_err();
    let no_project = wa::get(&w.app, a, get("NOPE", "Secret", None))
        .await
        .unwrap_err();
    for e in [&missing, &deleted, &deleted_title, &other, &no_project] {
        assert_eq!(code(e), ErrorCode::NotFound, "{e:#}");
    }
    assert_eq!(err_body(&other), err_body(&no_project));
    let e = wa::list(&w.app, a, list("AUTH", None, None))
        .await
        .unwrap_err();
    assert_eq!(err_body(&e), err_body(&no_project));

    // A person's caller is refused (the tools are the agent's).
    let e = wa::get(&w.app, w.alice_ctx, get("WEB", "Home", None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
}

/// `wiki_list` pages 200 at a time.
#[tokio::test]
async fn wiki_list_pages() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO wiki_pages (project_id, title, title_key, current_version)
             SELECT '{}', t, wiki_title_key(t), 1
             FROM generate_series(1, 205) n, LATERAL (SELECT 'P' || lpad(n::text, 3, '0') AS t) x",
            w.web
        ),
    )
    .await;
    let a = agent(&w, &[w.web]).await;
    let first = wa::list(&w.app, a, list("WEB", None, None)).await.unwrap();
    assert_eq!(first.items.len(), 200);
    assert!(first.truncated);
    let next = wa::list(&w.app, a, list("WEB", None, first.next_cursor.as_deref()))
        .await
        .unwrap();
    assert_eq!(next.items.len(), 5);
    assert!(!next.truncated && next.next_cursor.is_none());
    assert_eq!(next.items[0].title, "P201");
}

// ---------------------------------------------------------------- T-M2-41 (reads)

/// T-M2-41, reading: W(P) ⊋ A(P) (dave reads the wiki, not the issues).
/// The agent reads; the wiki is not writable for it (`wiki_readers_differ`,
/// R-AG3 in M2.3b's write half).
#[tokio::test]
async fn t41_a_wider_wiki_is_readable() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let wiki_only = role(
        &pool,
        "WikiOnly",
        "all",
        &["view_wiki_pages", "view_wiki_edits"],
    )
    .await;
    let dave = user(&pool, "dave").await;
    member(&pool, w.web, dave, wiki_only).await;
    page(&w, "WEB", "Home", "hello", None).await;
    let a = agent(&w, &[w.web]).await;
    assert_eq!(
        wa::get(&w.app, a, get("WEB", "Home", Some(1)))
            .await
            .unwrap()
            .body,
        "hello"
    );
    let mut conn = pool.acquire().await.unwrap();
    let d = audience::wiki_delegable(&mut conn, w.web).await.unwrap();
    assert!(d.pages && d.history && !d.equal);
    assert_eq!(
        wiki_state(&w, a, "WEB").await,
        ("readable".into(), Some("not_delegated".into())),
        "no wiki op is delegated yet"
    );
}

// ---------------------------------------------------------------- T-M2-42

/// T-M2-42: W(P) ⊊ A(P) (erin reads the issues, not the wiki). Every wiki
/// tool for the project is `forbidden_op` / `wiki_not_delegable`, the same
/// answer for a page that exists and one that does not; `describe` says
/// why; the wiki search leaves the project out.
#[tokio::test]
async fn t42_a_narrower_wiki_is_not_delegable() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let issues_only = role(&pool, "IssuesOnly", "all", &["view_issues"]).await;
    let erin = user(&pool, "erin").await;
    member(&pool, w.web, erin, issues_only).await;
    page(&w, "WEB", "Home", "canary-web", None).await;
    page(&w, "AUTH", "Home", "canary-auth", None).await;
    let a = agent(&w, &[w.web, w.auth_p]).await;
    let exists = wa::get(&w.app, a, get("WEB", "Home", None))
        .await
        .unwrap_err();
    let missing = wa::get(&w.app, a, get("WEB", "Nothing", None))
        .await
        .unwrap_err();
    let old = wa::get(&w.app, a, get("WEB", "Home", Some(1)))
        .await
        .unwrap_err();
    let listed = wa::list(&w.app, a, list("WEB", None, None))
        .await
        .unwrap_err();
    assert_eq!(code(&exists), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&exists), "wiki_not_delegable");
    for e in [&missing, &old, &listed] {
        assert_eq!(err_body(e), err_body(&exists));
    }
    // AUTH (W = A) still reads.
    assert_eq!(
        wa::get(&w.app, a, get("AUTH", "Home", None))
            .await
            .unwrap()
            .body,
        "canary-auth"
    );
    assert_eq!(
        wiki_state(&w, a, "WEB").await,
        ("unavailable".into(), Some("wiki_not_delegable".into()))
    );
    assert_eq!(wiki_state(&w, a, "AUTH").await.0, "readable");

    // The wiki search: AUTH's page only, even naming WEB.
    w.app.wiki.sync_all(&pool).await.unwrap();
    let s = mcp_search(&w, a, json!({"kinds": ["wiki"], "q": "canary"})).await;
    let hits: Vec<&str> = s["wiki"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["project"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(hits, ["AUTH"]);
    let s = mcp_search(
        &w,
        a,
        json!({"kinds": ["wiki"], "q": "canary", "project": "WEB"}),
    )
    .await;
    assert_eq!(s["wiki"]["items"], json!([]));
}

/// T-M2-42a: the delegator has view_wiki_edits, but H(P) ⊊ A(P) (erin reads
/// issues and pages, not their history): `version` is
/// `wiki_history_not_delegable`; without it the page reads as usual.
#[tokio::test]
async fn t42a_history_narrower_than_issues() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let no_history = role(
        &pool,
        "NoHistory",
        "all",
        &["view_issues", "view_wiki_pages"],
    )
    .await;
    let erin = user(&pool, "erin").await;
    member(&pool, w.web, erin, no_history).await;
    page(&w, "WEB", "Home", "now", None).await;
    let a = agent(&w, &[w.web]).await;
    assert_eq!(
        wa::get(&w.app, a, get("WEB", "Home", None))
            .await
            .unwrap()
            .body,
        "now"
    );
    let e = wa::get(&w.app, a, get("WEB", "Home", Some(1)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "wiki_history_not_delegable");
    let missing = wa::get(&w.app, a, get("WEB", "Nothing", Some(1)))
        .await
        .unwrap_err();
    assert_eq!(
        err_body(&missing),
        err_body(&e),
        "before the page is looked up"
    );
}

/// A delegator without view_wiki_edits reads no older version (H(P) aside).
#[tokio::test]
async fn delegator_without_history_reads_no_version() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'view_wiki_edits'",
            w.dev
        ),
    )
    .await;
    page(&w, "WEB", "Home", "now", None).await;
    let a = agent(&w, &[w.web]).await;
    let e = wa::get(&w.app, a, get("WEB", "Home", Some(1)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "permission");
}

/// T-M2-42b: issues off, wiki on. `holders()` would give the wiki no
/// readers; `wiki_audience()` counts them (and A(P) is empty, so W ⊇ A).
/// Such a project cannot be delegated at all (the delegator must read its
/// issues), so the agent never reaches it.
#[tokio::test]
async fn t42b_wiki_without_issues() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'issue_tracking'",
            w.auth_p
        ),
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    let a = audience::project_audience(&mut conn, w.auth_p)
        .await
        .unwrap();
    assert!(a.users.is_empty() && !a.anonymous);
    let wr = audience::wiki_audience(&mut conn, w.auth_p, "view_wiki_pages")
        .await
        .unwrap();
    assert_eq!(wr.users, [w.alice, w.bob].into_iter().collect());
    let d = audience::wiki_delegable(&mut conn, w.auth_p).await.unwrap();
    assert!(d.pages && d.history && !d.equal);
    drop(conn);
    let e = w.delegate(w.auth_p).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

/// T-M2-42c (regression): an `own` role holding view_wiki_pages. `holders()`
/// (visibility all/default only) would leave frank out and call W = A;
/// `wiki_audience()` counts him: W ⊋ A, readable, not writable.
#[tokio::test]
async fn t42c_own_role_with_the_wiki() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let own = role(&pool, "OwnWiki", "own", &["view_wiki_pages"]).await;
    let frank = user(&pool, "frank").await;
    member(&pool, w.web, frank, own).await;
    page(&w, "WEB", "Home", "hello", None).await;
    let a = agent(&w, &[w.web]).await;
    let mut conn = pool.acquire().await.unwrap();
    let wr = audience::wiki_audience(&mut conn, w.web, "view_wiki_pages")
        .await
        .unwrap();
    assert!(wr.users.contains(&frank));
    assert!(
        !audience::project_audience(&mut conn, w.web)
            .await
            .unwrap()
            .users
            .contains(&frank)
    );
    let d = audience::wiki_delegable(&mut conn, w.web).await.unwrap();
    assert!(d.pages && !d.equal);
    drop(conn);
    assert_eq!(
        wa::get(&w.app, a, get("WEB", "Home", None))
            .await
            .unwrap()
            .body,
        "hello"
    );
}

// ---------------------------------------------------------------- T-M2-49

/// T-M2-49: view_wiki_pages granted to a role in use moves the project's
/// epoch; the context that read the project is `context_unavailable`.
#[tokio::test]
async fn t49_a_permission_change_closes_the_context() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    let later = role(&pool, "Later", "all", &[]).await;
    let gina = user(&pool, "gina").await;
    member(&pool, w.web, gina, later).await;
    page(&w, "WEB", "Home", "hello", None).await;
    let a = agent(&w, &[w.web]).await;
    wa::get(&w.app, a, get("WEB", "Home", None)).await.unwrap();
    let before = epoch(&pool, w.web).await;
    grant(&pool, later, &["view_wiki_pages"]).await;
    assert!(epoch(&pool, w.web).await > before);
    let e = wa::get(&w.app, a, get("WEB", "Home", None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
    let e = wa::list(&w.app, a, list("WEB", None, None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable);
}

// ---------------------------------------------------------------- search kinds

async fn mcp_search(w: &World, agent: AuthContext, args: Value) -> Value {
    let p: McpSearchParams = serde_json::from_value(args).unwrap();
    serde_json::to_value(p.run(&w.app, agent).await.unwrap()).unwrap()
}

/// MCP `search` `kinds`: issues by default (no `wiki`), wiki only, both;
/// the wiki results are the delegated wikis R-AG1 allows, never the
/// delegator's others (AUTH is readable by alice but not delegated).
#[tokio::test]
async fn search_kinds() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    w.issue(w.web, "zebra issue").await;
    page(&w, "WEB", "Zebra", "zebra page", None).await;
    page(&w, "AUTH", "Zebra", "zebra auth page", None).await;
    w.app.index.sync_all(&pool).await.unwrap();
    w.app.wiki.sync_all(&pool).await.unwrap();
    let a = agent(&w, &[w.web]).await;

    let plain = mcp_search(&w, a, json!({"q": "zebra"})).await;
    assert!(plain.get("wiki").is_none());
    assert_eq!(plain["items"].as_array().unwrap().len(), 1);
    let wiki = mcp_search(&w, a, json!({"q": "zebra", "kinds": ["wiki"]})).await;
    assert!(wiki.get("items").is_none(), "no issue group");
    let items = wiki["wiki"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["project"]["key"], "WEB");
    assert_eq!(items[0]["title"], "Zebra");
    assert_eq!(wiki["wiki"]["available"], true);
    let both = mcp_search(&w, a, json!({"q": "zebra", "kinds": ["issue", "wiki"]})).await;
    assert_eq!(both["items"].as_array().unwrap().len(), 1);
    assert_eq!(both["wiki"]["items"].as_array().unwrap().len(), 1);

    for bad in [json!({"kinds": []}), json!({"kinds": ["page"]})] {
        let p: McpSearchParams = serde_json::from_value(bad).unwrap();
        let e = p.run(&w.app, a).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidArgument);
    }
    // The issue parameters stay strict under the wrapper.
    assert!(serde_json::from_value::<McpSearchParams>(json!({"q": "x", "bogus": 1})).is_err());
    assert!(serde_json::from_value::<SearchParams>(json!({"kinds": ["wiki"]})).is_err());

    // A person through the same entry point gets their own wikis.
    let mine: McpSearchParams =
        serde_json::from_value(json!({"q": "zebra", "kinds": ["wiki"]})).unwrap();
    let mine = serde_json::to_value(mine.run(&w.app, w.alice_ctx).await.unwrap()).unwrap();
    assert_eq!(mine["wiki"]["items"].as_array().unwrap().len(), 2);
}

/// Design 02 §3.2: a legacy API key keeps the M0 reads; `kinds` with
/// "wiki" is refused for it (as wiki_get is). Parameters the asked-for
/// kinds do not read are refused, not ignored.
#[tokio::test]
async fn search_kinds_legacy_key_and_unused_parameters() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    page(&w, "WEB", "Zebra", "zebra page", None).await;
    w.app.index.sync_all(&pool).await.unwrap();
    w.app.wiki.sync_all(&pool).await.unwrap();
    let raw = "oxl_legacy_wiki_key";
    sqlx::query("INSERT INTO api_keys (token_sha256, principal_id) VALUES ($1, $2)")
        .bind(auth::hash_api_key(raw))
        .bind(w.alice)
        .execute(&pool)
        .await
        .unwrap();
    let legacy = auth::authenticate_bearer(&pool, raw)
        .await
        .unwrap()
        .expect("valid key");
    for kinds in [json!(["wiki"]), json!(["issue", "wiki"])] {
        let p: McpSearchParams =
            serde_json::from_value(json!({"q": "zebra", "kinds": kinds})).unwrap();
        let e = p.run(&w.app, legacy).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::ForbiddenOp);
        assert_eq!(reason(&e), "method_not_allowed");
    }
    let p: McpSearchParams = serde_json::from_value(json!({"q": "zebra"})).unwrap();
    p.run(&w.app, legacy).await.unwrap();

    let a = agent(&w, &[w.web]).await;
    for (args, path) in [
        (
            json!({"q": "z", "kinds": ["wiki"], "cursor": "x"}),
            "cursor",
        ),
        (
            json!({"q": "z", "kinds": ["wiki"], "tracker": "Bug"}),
            "tracker",
        ),
        (json!({"q": "z", "kinds": ["wiki"], "limit": 5}), "limit"),
        (json!({"q": "z", "wiki_cursor": "w20"}), "wiki_cursor"),
    ] {
        let p: McpSearchParams = serde_json::from_value(args.clone()).unwrap();
        let e = p.run(&w.app, a).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidArgument, "{args}");
        assert_eq!(err_body(&e)["details"]["path"], path, "{args}");
    }
    // Shared parameters (and null) are fine with wiki alone.
    let p: McpSearchParams = serde_json::from_value(
        json!({"q": "zebra", "kinds": ["wiki"], "project": "WEB", "cursor": null}),
    )
    .unwrap();
    let r = serde_json::to_value(p.run(&w.app, a).await.unwrap()).unwrap();
    assert_eq!(r["wiki"]["items"].as_array().unwrap().len(), 1);
}

/// R-AG1 starts from the delegator's own read (R-W1): alice delegates WEB
/// while her role there has no wiki right. The others still read the wiki,
/// so without that check the answer would be `wiki_not_delegable` (W ⊉ A)
/// instead of the `not_found` of a wiki she cannot read.
#[tokio::test]
async fn delegator_without_the_wiki_reads_nothing() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    page(&w, "WEB", "Zebra", "zebra page", None).await;
    w.app.wiki.sync_all(&pool).await.unwrap();
    let nowiki = role(&pool, "DevNoWiki", "all", &[]).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permissions (role_id, permission)
             SELECT '{nowiki}', permission FROM role_permissions
             WHERE role_id = '{}' AND permission NOT LIKE '%wiki%'",
            w.dev
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE member_roles SET role_id = '{nowiki}'
             WHERE member_id = (SELECT id FROM members WHERE project_id = '{}' AND principal_id = '{}')",
            w.web, w.alice
        ),
    )
    .await;
    let a = agent(&w, &[w.web]).await;
    let e = wa::get(&w.app, a, get("WEB", "Zebra", None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = wa::list(&w.app, a, list("WEB", None, None))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let r = mcp_search(&w, a, json!({"q": "zebra", "kinds": ["wiki"]})).await;
    assert!(r["wiki"]["items"].as_array().unwrap().is_empty());
    assert_eq!(
        wiki_state(&w, a, "WEB").await,
        ("unavailable".into(), Some("no_permission".into()))
    );
}

/// `describe agent_context`'s wiki status with the wiki module off.
#[tokio::test]
async fn describe_wiki_module_off() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'wiki'",
            w.auth_p
        ),
    )
    .await;
    let off = agent(&w, &[w.auth_p]).await;
    assert_eq!(
        wiki_state(&w, off, "AUTH").await,
        ("unavailable".into(), Some("wiki_module_off".into()))
    );
}

/// `describe agent_context`'s wiki status: writable (W = A, inside every
/// source's readers); readable for `audience` (WEB's readers are more than
/// AUTH's); a revoked delegation.
#[tokio::test]
async fn describe_wiki_states() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    // Both projects with wiki_put in the delegation (the wizard offers it
    // from M2.3b's writing half).
    let a = agent(&w, &[w.web, w.auth_p]).await;
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET proposal_ops = array_append(proposal_ops, 'wiki_put')
             WHERE context_id = '{}'",
            a.context_id().unwrap()
        ),
    )
    .await;
    assert_eq!(wiki_state(&w, a, "AUTH").await, ("writable".into(), None));
    assert_eq!(
        wiki_state(&w, a, "WEB").await,
        ("readable".into(), Some("audience".into()))
    );
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET revoked_at = now() WHERE context_id = '{}' AND project_id = '{}'",
            a.context_id().unwrap(),
            w.auth_p
        ),
    )
    .await;
    assert_eq!(
        wiki_state(&w, a, "AUTH").await,
        ("unavailable".into(), Some("delegation".into()))
    );
}

/// A closed project (closed before the delegation: closing moves the
/// project's epoch): the wiki is read only.
#[tokio::test]
async fn describe_wiki_closed_project() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            w.auth_p
        ),
    )
    .await;
    let a = agent(&w, &[w.auth_p]).await;
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET proposal_ops = array_append(proposal_ops, 'wiki_put')
             WHERE context_id = '{}'",
            a.context_id().unwrap()
        ),
    )
    .await;
    assert_eq!(
        wiki_state(&w, a, "AUTH").await,
        ("readable".into(), Some("protected_project".into()))
    );
}

// ---------------------------------------------------------------- MCP and the built-in AI

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: "https://spa.test".into(),
            session: SessionPolicy::default(),
        },
    )
}

/// The tools over MCP (registered, agent tokens only) and in the built-in
/// AI's set.
#[tokio::test]
async fn mcp_tools_and_the_builtin_set() {
    let pool = test_pool().await;
    let w = setup(&pool).await;
    page(&w, "WEB", "Home", "over mcp", None).await;
    let ctx = w.delegate(w.web).await.unwrap().context_id;
    let (token, _) = w.token(ctx).await;
    let mut m = Mcp::connect(router(&w), token).await;
    let g = m
        .call("wiki_get", json!({"project": "WEB", "page": "Home"}))
        .await
        .unwrap();
    assert_eq!(g["body"], "over mcp");
    let l = m
        .call("wiki_list", json!({"project": "WEB"}))
        .await
        .unwrap();
    assert_eq!(l["items"][0]["title"], "Home");
    // Unknown arguments are refused (an error result, not a page).
    assert!(
        m.call(
            "wiki_get",
            json!({"project": "WEB", "page": "Home", "extra": 1}),
        )
        .await
        .is_err()
    );
    for t in ["wiki_list", "wiki_get"] {
        assert!(registry::BUILTIN_TOOLS.contains(&t));
        assert!(registry::mcp_tools().iter().any(|d| d.name == t));
    }
}
