// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.3a: the wiki search index and `GET /wiki/search` (§3.14,
//! §6.7, B-49): T-M2-36 (no wiki the caller cannot read ever matches, and
//! the response does not depend on it), T-M2-37 (`consistent_after` and
//! `search_lagging`), the index following renames, moves, deletions,
//! restores and D12 redactions, the blue-green rebuild, the consumer moving
//! over every outbox row (the outbox GC is not held back) and ranking
//! (a title match before a text-only match). Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ApiError, ErrorCode};
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::search::query_terms;
use oxledger::service::parse_token;
use oxledger::wiki::WikiEffect;
use oxledger::wiki_search::{self, WikiSearchParams, WikiSearchResult};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;
use world::*;

const ADMIN: [&str; 8] = [
    "view_wiki_pages",
    "view_wiki_edits",
    "edit_wiki_pages",
    "rename_wiki_pages",
    "delete_wiki_pages",
    "protect_wiki_pages",
    "manage_wiki",
    "manage_project",
];
const READER: [&str; 1] = ["view_wiki_pages"];
/// Every wiki permission but view_wiki_pages.
const BLIND: [&str; 7] = [
    "view_wiki_edits",
    "edit_wiki_pages",
    "rename_wiki_pages",
    "delete_wiki_pages",
    "protect_wiki_pages",
    "manage_wiki",
    "manage_project",
];

struct Env {
    w: World,
    /// Wiki administrator in WEB, AUTH, OPS and ARCH.
    alice: AuthContext,
    /// Reader of WEB's wiki (and of OPS, whose module ends up off); not a
    /// member of AUTH or ARCH.
    carol: AuthContext,
    /// Every wiki permission but view_wiki_pages in WEB.
    dave: AuthContext,
    /// No membership anywhere.
    erin: AuthContext,
    ops: Uuid,
    arch: Uuid,
}

async fn role(pool: &PgPool, name: &str, perms: &[&str]) -> Uuid {
    let r = id(
        pool,
        &format!(
            "INSERT INTO roles (name, issues_visibility) VALUES ('{name}','all') RETURNING id"
        ),
    )
    .await;
    grant(pool, r, perms).await;
    r
}

async fn add_role(pool: &PgPool, project: Uuid, user: Uuid, role: Uuid) {
    let m: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM members WHERE project_id = $1 AND principal_id = $2")
            .bind(project)
            .bind(user)
            .fetch_optional(pool)
            .await
            .unwrap();
    match m {
        Some(m) => {
            sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
                .bind(m)
                .bind(role)
                .execute(pool)
                .await
                .unwrap();
        }
        None => {
            member(pool, project, user, role).await;
        }
    }
}

/// WEB, AUTH, OPS and ARCH have the wiki module (OPS loses it and ARCH is
/// archived once the tests wrote their pages: [`close_off`]).
async fn setup(pool: &PgPool) -> Env {
    let w = World::build(pool).await;
    let ops = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('OPS','Ops') RETURNING id",
    )
    .await;
    let arch = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('ARCH','Arch') RETURNING id",
    )
    .await;
    exec(
        pool,
        "INSERT INTO enabled_modules (project_id, module)
         SELECT id, 'wiki' FROM projects WHERE key IN ('WEB','AUTH','OPS','ARCH')",
    )
    .await;
    let admin = role(pool, "WikiAdmin", &ADMIN).await;
    let reader = role(pool, "WikiReader", &READER).await;
    let blind = role(pool, "WikiBlind", &BLIND).await;
    for p in [w.web, w.auth_p, ops, arch] {
        add_role(pool, p, w.alice, admin).await;
    }
    add_role(pool, w.web, w.carol, reader).await;
    add_role(pool, ops, w.carol, reader).await;
    let dave = auth::create_user(pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(pool, w.web, dave, blind).await;
    auth::create_user(pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    Env {
        alice: w.alice_ctx,
        carol: sign_in(pool, &w.policy, "carol").await,
        dave: sign_in(pool, &w.policy, "dave").await,
        erin: sign_in(pool, &w.policy, "erin").await,
        ops,
        arch,
        w,
    }
}

/// OPS loses the wiki module, ARCH is archived.
async fn close_off(e: &Env) {
    exec(
        &e.w.pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'wiki';
             UPDATE projects SET status = 'archived' WHERE id = '{}'",
            e.ops, e.arch
        ),
    )
    .await;
}

fn op(kind: &str, target: Value, extra: Value) -> Value {
    let mut o = json!({"kind": kind, "op_id": Uuid::now_v7(), "target": target});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            o[k] = v;
        }
    }
    o
}

async fn run(e: &Env, op: Value) -> CommandResult {
    let d = json!({"command_schema_version": 1, "ops": [op]});
    commands::submit(&e.w.app, e.alice, Uuid::now_v7(), &d)
        .await
        .unwrap_or_else(|err| panic!("{d}: {err:#}"))
}

async fn create(e: &Env, project: &str, title: &str, body: &str) -> WikiEffect {
    run(
        e,
        op(
            "wiki_put",
            json!({"project": project, "page": title}),
            json!({"body": body}),
        ),
    )
    .await
    .wiki[0]
        .clone()
}

async fn lv(e: &Env, page: Uuid) -> i64 {
    count(
        &e.w.pool,
        &format!("SELECT lock_version FROM wiki_pages WHERE id = '{page}'"),
    )
    .await
}

async fn edit(e: &Env, page: Uuid, body: &str) -> CommandResult {
    let l = lv(e, page).await;
    let d = json!({"command_schema_version": 1,
        "ops": [op("wiki_put", json!({"project": "WEB", "page_id": page}), json!({"body": body}))],
        "preconditions": [{"wiki_page": page, "lock_version": l}]});
    commands::submit(&e.w.app, e.alice, Uuid::now_v7(), &d)
        .await
        .unwrap()
}

async fn on_page(e: &Env, kind: &str, page: Uuid, extra: Value) -> CommandResult {
    run(
        e,
        op(kind, json!({"project": "WEB", "page_id": page}), extra),
    )
    .await
}

async fn redact(e: &Env, target: Target) {
    redaction::redact(
        &e.w.app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        &target,
        &RedactInput {
            reason_code: "confidential".into(),
            reason: "test".into(),
            attachments: Vec::new(),
        },
    )
    .await
    .unwrap();
}

fn q(text: &str) -> WikiSearchParams {
    WikiSearchParams {
        q: Some(text.into()),
        ..Default::default()
    }
}

async fn search(e: &Env, who: AuthContext, p: WikiSearchParams) -> WikiSearchResult {
    wiki_search::search(&e.w.app, who, p)
        .await
        .unwrap_or_else(|err| panic!("wiki search: {err:#}"))
}

/// Titles of the hits, in order.
async fn titles(e: &Env, who: AuthContext, text: &str) -> Vec<String> {
    search(e, who, q(text))
        .await
        .items
        .into_iter()
        .map(|i| i.title)
        .collect()
}

/// The response as JSON without the cursor (sealed with a fresh nonce).
fn shape(r: &WikiSearchResult) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    v["next_cursor"] = json!(r.next_cursor.is_some());
    v
}

/// The wiki index alone (not the SQL re-check): pages whose indexed text
/// matches `text`, in `projects`.
fn indexed(e: &Env, text: &str, projects: &[Uuid]) -> Vec<Uuid> {
    let v = e.w.app.wiki.view().expect("the wiki index serves");
    let mut ids: Vec<Uuid> = v
        .search(&query_terms(text), projects, 100)
        .unwrap()
        .into_iter()
        .map(|h| h.page_id)
        .collect();
    ids.sort();
    ids
}

async fn sync(e: &Env) {
    e.w.app.wiki.sync_all(&e.w.pool).await.unwrap();
}

// ---------------------------------------------------------------- T-M2-36

/// T-M2-36 (Q-S1): a canary in every wiki carol cannot read (another
/// project, the module off, archived, a deleted page) never matches, and
/// the response with visible results is exactly the response without any
/// canary. `project=` naming an unreadable wiki and a missing project
/// answer the same. dave (every wiki permission but reading) and erin
/// (no membership) see nothing and no Wiki tab.
#[tokio::test]
async fn t_m2_36_unreadable_wikis_never_match() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    // 25 visible pages: more than one page of results.
    for i in 0..25 {
        create(&e, "WEB", &format!("Visible {i:02}"), "quokka handbook").await;
    }
    sync(&e).await;
    let before1 = search(&e, e.carol, q("quokka")).await;
    assert_eq!(before1.items.len(), 20);
    assert!(before1.next_cursor.is_some());
    assert!(before1.available);
    let mut p2 = q("quokka");
    p2.cursor = before1.next_cursor.clone();
    let before2 = search(&e, e.carol, p2).await;
    assert_eq!(before2.items.len(), 5);
    assert!(before2.next_cursor.is_none());

    // Canaries everywhere carol cannot read.
    for project in ["AUTH", "OPS", "ARCH"] {
        create(&e, project, "Canary", "quokka zqxcanary").await;
        create(&e, project, "Quokka zqxcanary", "zqxcanary").await;
    }
    let gone = create(&e, "WEB", "Gone Canary", "quokka zqxcanary").await;
    on_page(&e, "wiki_delete", gone.page_id, json!({})).await;
    close_off(&e).await;
    sync(&e).await;

    for canary in ["zqxcanary", "Canary", "canary quokka"] {
        let r = search(&e, e.carol, q(canary)).await;
        assert!(
            r.items.iter().all(|i| i.title.starts_with("Visible")),
            "{canary}: {:?}",
            r.items.iter().map(|i| &i.title).collect::<Vec<_>>()
        );
    }
    assert!(search(&e, e.carol, q("zqxcanary")).await.items.is_empty());
    // The same visible results, page by page, as before the canaries.
    let after1 = search(&e, e.carol, q("quokka")).await;
    assert_eq!(shape(&after1), shape(&before1));
    let mut p2 = q("quokka");
    p2.cursor = after1.next_cursor.clone();
    assert_eq!(shape(&search(&e, e.carol, p2).await), shape(&before2));

    // project=: unreadable (not a member, module off, archived), missing
    // by key or id, malformed: one and the same answer.
    let mut answers = Vec::new();
    for project in [
        "AUTH".to_string(),
        "OPS".to_string(),
        "ARCH".to_string(),
        e.w.auth_p.to_string(),
        "NOPE".to_string(),
        Uuid::now_v7().to_string(),
        "not a key!".to_string(),
    ] {
        for text in ["zqxcanary", "quokka"] {
            let mut p = q(text);
            p.project = Some(project.clone());
            let r = search(&e, e.carol, p).await;
            answers.push((project.clone(), text, shape(&r)));
        }
    }
    let reference = &answers[0].2;
    assert_eq!(reference["items"], json!([]));
    assert_eq!(reference["available"], json!(true));
    for (project, text, a) in &answers {
        assert_eq!(a, reference, "project={project} q={text}");
    }
    // project=WEB: carol's own wiki.
    let mut p = q("quokka");
    p.project = Some("web".into());
    assert_eq!(search(&e, e.carol, p).await.items.len(), 20);

    // No read permission at all: nothing, and no Wiki tab.
    for who in [e.dave, e.erin] {
        let r = search(&e, who, q("quokka")).await;
        assert!(r.items.is_empty());
        assert!(!r.available);
        let r = search(&e, who, WikiSearchParams::default()).await;
        assert!(!r.available);
        assert_eq!(r.consistency.status, "not_applicable");
    }
    // Without q: only `available`.
    let r = search(&e, e.carol, WikiSearchParams::default()).await;
    assert!(r.available && r.items.is_empty() && r.next_cursor.is_none());

    // alice reads AUTH: its canaries match; OPS (module off), ARCH
    // (archived) and the deleted page still do not.
    let mut seen: Vec<(String, String)> = search(&e, e.alice, q("zqxcanary"))
        .await
        .items
        .into_iter()
        .map(|i| (i.project.key, i.title))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("AUTH".to_string(), "Canary".to_string()),
            ("AUTH".to_string(), "Quokka zqxcanary".to_string())
        ]
    );
    // The module back on: OPS's pages are found again (the index kept
    // them; visibility is decided at query time).
    exec(
        &pool,
        &format!(
            "INSERT INTO enabled_modules (project_id, module) VALUES ('{}', 'wiki')",
            e.ops
        ),
    )
    .await;
    assert_eq!(
        titles(&e, e.carol, "zqxcanary").await,
        ["Quokka zqxcanary", "Canary"]
    );
}

/// Anonymous callers (the service; the REST route needs a session, like
/// POST /issues/search): public projects whose Anonymous role has
/// view_wiki_pages.
#[tokio::test]
async fn anonymous_reads_public_wikis_only() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    create(&e, "WEB", "Public Page", "wombat").await;
    create(&e, "AUTH", "Private Page", "wombat").await;
    sync(&e).await;
    let anon = AuthContext::anonymous();
    let r = search(&e, anon, q("wombat")).await;
    assert!(r.items.is_empty() && !r.available);
    let anonymous = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility, builtin) VALUES ('Anonymous','default','anonymous') RETURNING id",
    )
    .await;
    grant(&pool, anonymous, &["view_wiki_pages"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET is_public = true WHERE id = '{}'",
            e.w.web
        ),
    )
    .await;
    let r = search(&e, anon, q("wombat")).await;
    assert!(r.available);
    assert_eq!(
        r.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
        ["Public Page"]
    );
}

// ---------------------------------------------------------------- T-M2-37

/// T-M2-37: a wiki write's receipt token waits for the wiki index; while it
/// lags the search says `search_lagging` (no items) or, with on_lag error,
/// `search_not_caught_up`; once the index moves the same search finds the
/// page at once.
#[tokio::test]
async fn t_m2_37_consistent_after_waits_for_the_wiki_index() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    create(&e, "WEB", "Before", "earlier text").await;
    sync(&e).await;

    let r = run(
        &e,
        op(
            "wiki_put",
            json!({"project": "WEB", "page": "Fresh"}),
            json!({"body": "numbat migration"}),
        ),
    )
    .await;
    let token = r.search.consistent_after.clone();
    let (_, x) = parse_token(&token).expect("c1 token");
    let page = r.wiki[0].page_id;
    let with = |wait: u32| WikiSearchParams {
        q: Some("numbat".into()),
        consistent_after: Some(token.clone()),
        wait_ms: Some(wait),
        ..Default::default()
    };
    // The worker is stopped: lagging, no items, no cursor.
    for wait in [0, 150] {
        let started = std::time::Instant::now();
        let s = search(&e, e.carol, with(wait)).await;
        assert_eq!(s.outcome, "search_lagging");
        assert_eq!(s.consistency.status, "lagging");
        assert_eq!(s.consistency.retry_after_ms, Some(500));
        assert!(s.items.is_empty() && s.next_cursor.is_none());
        assert!(s.available);
        assert!(started.elapsed() >= Duration::from_millis(wait as u64));
    }
    let mut p = with(0);
    p.on_lag = Some("error".into());
    let err = wiki_search::search(&e.w.app, e.carol, p).await.unwrap_err();
    let api = err.downcast_ref::<ApiError>().unwrap();
    assert_eq!(api.code, ErrorCode::SearchNotCaughtUp);
    assert_eq!(api.retry_after_ms, Some(500));
    // Without the token: the index as it is (eventual), not an error.
    let s = search(&e, e.carol, q("numbat")).await;
    assert_eq!(
        (s.outcome.as_str(), s.consistency.status.as_str()),
        ("ok", "eventual")
    );
    assert!(s.items.is_empty());
    // The issue index moving does not satisfy a wiki token.
    common::poll_until(
        &format!("the issue frontier to pass {x}"),
        common::WAIT_TIMEOUT,
        async || {
            e.w.app.index.sync(&e.w.pool).await.unwrap();
            if x < e.w.app.index.frontier() {
                common::Break(())
            } else {
                common::Continue(format!("F={}", e.w.app.index.frontier()))
            }
        },
    )
    .await;
    assert_eq!(
        search(&e, e.carol, with(0)).await.outcome,
        "search_lagging",
        "only the wiki index's frontier counts"
    );

    // A waiting search passes as soon as the wiki index publishes F > X.
    let app = e.w.app.clone();
    let carol = e.carol;
    let p = with(2000);
    let waiting = tokio::spawn(async move { wiki_search::search(&app, carol, p).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    common::poll_until(
        &format!("the wiki frontier to pass {x}"),
        common::WAIT_TIMEOUT,
        async || {
            e.w.app.wiki.sync(&e.w.pool).await.unwrap();
            if x < e.w.app.wiki.frontier() {
                common::Break(())
            } else {
                common::Continue(format!("F={}", e.w.app.wiki.frontier()))
            }
        },
    )
    .await;
    let s = common::join("the waiting search", waiting).await.unwrap();
    assert_eq!(s.outcome, "ok");
    assert_eq!(s.consistency.status, "satisfied");
    assert_eq!(
        s.items.iter().map(|i| i.page_id).collect::<Vec<_>>(),
        [page]
    );
    let s = search(&e, e.carol, with(0)).await;
    assert_eq!(s.consistency.status, "satisfied");
    assert_eq!(s.consistency.waited_ms, Some(0));
    assert_eq!(s.items.len(), 1);

    // Token checks as for issues.
    let mut p = with(0);
    p.consistent_after = Some("c1.nope".into());
    let err = wiki_search::search(&e.w.app, e.carol, p).await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<ApiError>().unwrap().code,
        ErrorCode::InvalidConsistencyToken
    );
    let mut p = with(2001);
    p.consistent_after = None;
    let err = wiki_search::search(&e.w.app, e.carol, p).await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<ApiError>().unwrap().code,
        ErrorCode::InvalidArgument
    );
    let err = wiki_search::search(&e.w.app, e.carol, q(" ,. "))
        .await
        .unwrap_err();
    assert_eq!(
        err.downcast_ref::<ApiError>().unwrap().code,
        ErrorCode::InvalidQuery
    );
}

// ---------------------------------------------------------------- index follows writes

/// Rename, move, delete, restore, an edit, a redacted current version and
/// a redacted title: the index (not only the SQL re-check) follows.
#[tokio::test]
async fn the_index_follows_wiki_writes() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let web = [e.w.web];
    let parent = create(&e, "WEB", "Parent", "nothing here").await;
    let p = create(&e, "WEB", "Alpha", "pelican secret").await;
    sync(&e).await;
    assert_eq!(indexed(&e, "pelican", &web), [p.page_id]);
    assert_eq!(indexed(&e, "alpha", &web), [p.page_id]);
    assert_eq!(titles(&e, e.carol, "pelican").await, ["Alpha"]);

    // Rename: the new title is indexed, the old one gone.
    on_page(&e, "wiki_move", p.page_id, json!({"title": "Beta"})).await;
    sync(&e).await;
    assert_eq!(indexed(&e, "beta", &web), [p.page_id]);
    assert!(indexed(&e, "alpha", &web).is_empty());
    assert_eq!(titles(&e, e.carol, "beta").await, ["Beta"]);
    assert!(titles(&e, e.carol, "alpha").await.is_empty());

    // Move under a parent: still there.
    on_page(
        &e,
        "wiki_move",
        p.page_id,
        json!({"parent": {"page_id": parent.page_id}}),
    )
    .await;
    sync(&e).await;
    assert_eq!(indexed(&e, "pelican", &web), [p.page_id]);

    // An edit: new text in, old text out.
    edit(&e, p.page_id, "pelican harbour").await;
    sync(&e).await;
    assert_eq!(indexed(&e, "harbour", &web), [p.page_id]);
    assert!(indexed(&e, "secret", &web).is_empty());

    // Delete: out of the index; restore: back.
    on_page(&e, "wiki_delete", p.page_id, json!({})).await;
    sync(&e).await;
    assert!(indexed(&e, "pelican", &web).is_empty());
    assert!(titles(&e, e.carol, "pelican").await.is_empty());
    on_page(&e, "wiki_restore", p.page_id, json!({})).await;
    sync(&e).await;
    assert_eq!(indexed(&e, "pelican", &web), [p.page_id]);
    assert_eq!(titles(&e, e.carol, "pelican").await, ["Beta"]);

    // D12 on the current version: the body becomes [已遮蔽]. Before the
    // index catches up the SQL re-check already drops the hit (the current
    // text no longer matches); after it, the index holds the marker.
    let v: i64 = count(
        &pool,
        &format!(
            "SELECT current_version::bigint FROM wiki_pages WHERE id = '{}'",
            p.page_id
        ),
    )
    .await;
    redact(
        &e,
        Target::WikiVersion {
            project: "WEB".into(),
            page: "Beta".into(),
            version: v as i32,
        },
    )
    .await;
    assert_eq!(indexed(&e, "pelican", &web), [p.page_id], "not synced yet");
    assert!(titles(&e, e.carol, "pelican").await.is_empty());
    sync(&e).await;
    assert!(indexed(&e, "pelican", &web).is_empty());
    assert_eq!(indexed(&e, "已遮蔽", &web), [p.page_id]);
    let r = search(&e, e.carol, q("已遮蔽")).await;
    assert_eq!(r.items.len(), 1);
    assert!(
        r.items[0]
            .snippet
            .iter()
            .any(|s| s.hit && s.text.contains("遮蔽"))
    );

    // D12 on the title: every old title goes, the new one is indexed.
    redact(
        &e,
        Target::WikiTitle {
            project: "WEB".into(),
            page: "Beta".into(),
            new_title: "Gamma".into(),
        },
    )
    .await;
    sync(&e).await;
    assert!(indexed(&e, "beta", &web).is_empty());
    assert_eq!(indexed(&e, "gamma", &web), [p.page_id]);
    assert_eq!(titles(&e, e.carol, "gamma").await, ["Gamma"]);
}

// ---------------------------------------------------------------- rebuild, cursors, GC

/// A blue-green rebuild serves the same results; the wiki consumer moves
/// over every outbox row (issue rows too), so with the issue consumer at
/// the end as well the outbox GC removes the wiki rows (B-49).
#[tokio::test]
async fn rebuild_and_consumers_reach_the_frontier() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    for (t, b) in [
        ("Ferry Times", "ferry schedule harbour"),
        ("Harbour", "the ferry leaves at noon"),
        ("渡輪時刻", "渡輪 ferry 時刻表"),
    ] {
        create(&e, "WEB", t, b).await;
    }
    // An issue row after the wiki rows.
    e.w.issue(e.w.web, "kestrel issue").await;
    let newest: String = sqlx::query_scalar("SELECT max(xid)::text FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    common::wait_horizon_past(&pool, newest.parse().unwrap()).await;
    sync(&e).await;
    e.w.app.index.sync_all(&pool).await.unwrap();

    // Both consumers reached the last outbox row.
    let last: (String, i64) =
        sqlx::query_as("SELECT xid::text, id FROM outbox ORDER BY xid DESC, id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let cursors: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT consumer, last_xid::text, last_id, state FROM outbox_cursors ORDER BY consumer",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let gen1 = e.w.app.wiki.generation().unwrap();
    let wiki_row = cursors
        .iter()
        .find(|c| c.0 == format!("wiki@{gen1}"))
        .expect("wiki consumer");
    assert_eq!(wiki_row.3, "active");
    // The baseline may already cover every row (cursor = (F0, 0)), or the
    // rounds passed them: either way nothing before the last row is left.
    let wiki_c: (String, i64) = (wiki_row.1.clone(), wiki_row.2);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE (xid, id) > ($1::text::xid8, $2)")
            .bind(&wiki_c.0)
            .bind(wiki_c.1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        left == 0 || wiki_c.0.parse::<u64>().unwrap() > last.0.parse::<u64>().unwrap(),
        "wiki cursor {wiki_c:?} is behind the last row {last:?}"
    );
    let search_row = cursors
        .iter()
        .find(|c| c.0.starts_with("search@") && c.3 == "active")
        .expect("issue consumer");
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE (xid, id) > ($1::text::xid8, $2)")
            .bind(&search_row.1)
            .bind(search_row.2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "the issue consumer is at the last row");

    // Rebuild: same answers, a new generation; the old one is retained.
    let queries = ["ferry", "harbour", "渡輪", "ferry 時刻"];
    let mut before = Vec::new();
    for t in queries {
        before.push(shape(&search(&e, e.alice, q(t)).await));
    }
    let report = e.w.app.wiki.rebuild(&pool).await.unwrap();
    assert_eq!(report.previous, Some(gen1));
    assert_eq!(report.baseline_docs, 3);
    assert_eq!(e.w.app.wiki.generation(), Some(report.generation));
    for (t, b) in queries.iter().zip(&before) {
        assert_eq!(&shape(&search(&e, e.alice, q(t)).await), b, "{t}");
    }
    let states: Vec<(String, String)> = sqlx::query_as(
        "SELECT consumer, state FROM outbox_cursors WHERE consumer LIKE 'wiki@%' ORDER BY consumer",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        states,
        [
            (format!("wiki@{gen1}"), "retained".to_string()),
            (format!("wiki@{}", report.generation), "active".to_string())
        ]
    );
    // The issue index's consumers are untouched by the wiki switch.
    let active_search: i64 = count(
        &pool,
        "SELECT count(*) FROM outbox_cursors WHERE consumer LIKE 'search@%' AND state = 'active'",
    )
    .await;
    assert_eq!(active_search, 1);

    // GC: once the retained generation's window is over, the rows below
    // every live cursor go, wiki rows included.
    exec(
        &pool,
        "UPDATE outbox_cursors SET retained_until = now() - interval '1 second'
         WHERE consumer LIKE 'wiki@%' AND state = 'retained';
         UPDATE outbox SET created_at = now() - interval '2 hours'",
    )
    .await;
    let wiki_rows = count(
        &pool,
        "SELECT count(*) FROM outbox WHERE topic = 'wiki_page'",
    )
    .await;
    assert!(wiki_rows >= 3);
    let (deleted, _) = e.w.app.wiki.gc(&pool).await.unwrap();
    assert!(deleted as i64 >= wiki_rows, "deleted {deleted}");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM outbox WHERE topic = 'wiki_page'"
        )
        .await,
        0,
        "the wiki consumer holds back no wiki row"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM outbox_cursors WHERE consumer LIKE 'wiki@%'"
        )
        .await,
        1,
        "the expired generation is gone"
    );
    // /metrics: wiki_outbox_pending.
    let m = oxledger::metrics::render(&pool, &e.w.app.index)
        .await
        .unwrap();
    assert!(m.contains("\nwiki_outbox_pending 0\n"), "{m}");
    create(&e, "WEB", "Late", "after the gc").await;
    let m = oxledger::metrics::render(&pool, &e.w.app.index)
        .await
        .unwrap();
    assert!(m.contains("\nwiki_outbox_pending 1\n"));
}

/// The wiki index on disk: a restart opens the generation in `CURRENT`
/// and resumes from its payload.
#[tokio::test]
async fn the_wiki_index_survives_a_restart() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    create(&e, "WEB", "Durable", "platypus").await;
    let dir = std::env::temp_dir().join(format!("oxledger-wiki-{}", Uuid::now_v7()));
    let first = wiki_search::WikiIndex::open(Some(&dir)).unwrap();
    first.sync_all(&pool).await.unwrap();
    let generation = first.generation().unwrap();
    drop(first);
    let again = wiki_search::WikiIndex::open(Some(&dir)).unwrap();
    again.sync_all(&pool).await.unwrap();
    assert_eq!(again.generation(), Some(generation));
    let v = again.view().unwrap();
    assert_eq!(
        v.search(&query_terms("platypus"), &[e.w.web], 10)
            .unwrap()
            .len(),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- ranking

/// Chinese, English and mixed queries: a title match ranks above pages
/// that only mention the words in their text; snippets mark the hits.
#[tokio::test]
async fn titles_rank_before_text_matches() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    for (t, b) in [
        ("會議紀錄", "今天討論了部署流程的細節，下週再確認"),
        ("部署流程", "說明如何上線新版本"),
        ("雜記", "部署之前先備份資料庫"),
        ("Weekly notes", "we deploy on fridays after the review"),
        ("Deploy guide", "steps to release a build"),
        ("Kubernetes 部署", "叢集設定"),
        ("Cluster notes", "用 kubernetes 部署服務時要注意資源"),
    ] {
        create(&e, "WEB", t, b).await;
    }
    sync(&e).await;
    let zh = titles(&e, e.carol, "部署流程").await;
    assert_eq!(zh[0], "部署流程", "{zh:?}");
    assert!(zh.contains(&"會議紀錄".to_string()));
    let en = titles(&e, e.carol, "deploy").await;
    assert_eq!(en, ["Deploy guide", "Weekly notes"]);
    let mixed = titles(&e, e.carol, "kubernetes 部署").await;
    assert_eq!(mixed[0], "Kubernetes 部署", "{mixed:?}");
    // Both words in the text only: found, below the full title match.
    assert!(
        mixed[1..].contains(&"Cluster notes".to_string()),
        "{mixed:?}"
    );

    let r = search(&e, e.carol, q("deploy")).await;
    let notes = r.items.iter().find(|i| i.title == "Weekly notes").unwrap();
    let hits: Vec<&str> = notes
        .snippet
        .iter()
        .filter(|s| s.hit)
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(hits, ["deploy"]);
    let whole: String = notes.snippet.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(whole, "we deploy on fridays after the review");
    assert_eq!(notes.url_title, "Weekly_notes");
    assert_eq!(notes.project.key, "WEB");
    // Markdown is matched and shown as its text.
    create(
        &e,
        "WEB",
        "Markup",
        "**bold** [link text](https://example.com/zebrafish)",
    )
    .await;
    sync(&e).await;
    let r = search(&e, e.carol, q("link")).await;
    let m = r.items.iter().find(|i| i.title == "Markup").unwrap();
    let whole: String = m.snippet.iter().map(|s| s.text.as_str()).collect();
    assert!(!whole.contains("**") && !whole.contains("]("), "{whole}");
}

// ---------------------------------------------------------------- review follow-ups

/// A deleted page leaves the results at once, before the index syncs: the
/// SQL re-check drops pages whose row is deleted, whatever the index holds.
#[tokio::test]
async fn a_deleted_page_is_gone_before_the_index_syncs() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let web = [e.w.web];
    let p = create(&e, "WEB", "Gone page", "okapi body").await;
    sync(&e).await;
    assert_eq!(titles(&e, e.carol, "okapi").await, ["Gone page"]);
    on_page(&e, "wiki_delete", p.page_id, json!({})).await;
    // No sync: the index still holds the page.
    assert_eq!(indexed(&e, "okapi", &web), [p.page_id]);
    assert!(titles(&e, e.carol, "okapi").await.is_empty());
    assert!(titles(&e, e.alice, "okapi").await.is_empty());
}

/// The `wiki@<generation>` consumer's cursor.
async fn wiki_cursor(pool: &PgPool, generation: Uuid) -> (u64, i64) {
    let (x, id): (String, i64) =
        sqlx::query_as("SELECT last_xid::text, last_id FROM outbox_cursors WHERE consumer = $1")
            .bind(format!("wiki@{generation}"))
            .fetch_one(pool)
            .await
            .unwrap();
    (x.parse().unwrap(), id)
}

/// Once the wiki index is active its consumer reads every outbox row: a
/// run of issue writes with no wiki write moves the `wiki@` cursor past
/// them (else the outbox GC would be held back by the wiki consumer).
#[tokio::test]
async fn the_wiki_cursor_moves_over_issue_rows() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    create(&e, "WEB", "Seed", "seed").await;
    sync(&e).await;
    let g =
        e.w.app
            .wiki
            .generation()
            .expect("an active wiki generation");
    let before = wiki_cursor(&pool, g).await;
    for _ in 0..3 {
        e.w.issue(e.w.web, "issue only").await;
    }
    let (x, id): (String, i64) =
        sqlx::query_as("SELECT xid::text, id FROM outbox ORDER BY xid DESC, id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let last = (x.parse::<u64>().unwrap(), id);
    assert!(last > before, "the issue writes add outbox rows");
    let wiki_after: i64 = count(
        &pool,
        &format!(
            "SELECT count(*) FROM outbox WHERE topic = 'wiki_page' AND (xid::text::numeric, id) > ({}, {})",
            before.0, before.1
        ),
    )
    .await;
    assert_eq!(wiki_after, 0, "no wiki row after the cursor");
    common::wait_horizon_past(&pool, last.0).await;
    sync(&e).await;
    assert_eq!(e.w.app.wiki.generation(), Some(g), "same generation");
    assert!(
        wiki_cursor(&pool, g).await >= last,
        "the wiki cursor stays behind the issue rows"
    );
}

/// Scans of the role tables in the caller's transaction so far.
async fn role_scans(conn: &mut sqlx::PgConnection) -> i64 {
    sqlx::query_scalar(
        "SELECT coalesce(sum(seq_scan + coalesce(idx_scan, 0)), 0)::bigint
         FROM pg_stat_xact_user_tables WHERE relname IN ('roles', 'role_permissions')",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

/// What `readable_wikis` costs `who`: the wikis read, and the scans of the
/// role tables it made (one transaction, so the counts are its own).
async fn readable_cost(pool: &PgPool, who: AuthContext) -> (Vec<Uuid>, i64) {
    let mut tx = pool.begin().await.unwrap();
    let before = role_scans(&mut tx).await;
    let ids = oxledger::wiki::readable_wiki_ids(&mut tx, who)
        .await
        .unwrap();
    let scans = role_scans(&mut tx).await - before;
    tx.rollback().await.unwrap();
    (ids, scans)
}

/// The set of readable wikis is loaded in a fixed number of queries,
/// however many projects there are: a system administrator and a
/// non-member facing 300 more public wikis scan the role tables exactly
/// as often as with a handful (the per-project loop made several queries
/// per wiki, about a second per search with ~300 wikis).
#[tokio::test]
async fn readable_wikis_cost_does_not_grow_with_projects() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let non_member = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility, builtin) VALUES ('Non member','default','non_member') RETURNING id",
    )
    .await;
    grant(&pool, non_member, &["view_wiki_pages"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE principals SET is_admin = true WHERE id = '{}';
             UPDATE projects SET is_public = true WHERE id = '{}'",
            e.w.alice, e.w.web
        ),
    )
    .await;
    let (admin_few, admin_scans_few) = readable_cost(&pool, e.alice).await;
    let (erin_few, erin_scans_few) = readable_cost(&pool, e.erin).await;
    // erin reads WEB (public) through the Non member role: the baseline
    // loads that role's permissions too.
    assert_eq!(erin_few, [e.w.web]);

    exec(
        &pool,
        "INSERT INTO projects (key, name, is_public)
         SELECT 'P' || g, 'P' || g, true FROM generate_series(1, 300) g;
         INSERT INTO enabled_modules (project_id, module)
         SELECT id, 'wiki' FROM projects WHERE key LIKE 'P%'",
    )
    .await;
    let t = std::time::Instant::now();
    let (admin_many, admin_scans_many) = readable_cost(&pool, e.alice).await;
    let admin_time = t.elapsed();
    let t = std::time::Instant::now();
    let (erin_many, erin_scans_many) = readable_cost(&pool, e.erin).await;
    let erin_time = t.elapsed();
    eprintln!("readable_wikis with 300 more wikis: admin {admin_time:?}, non-member {erin_time:?}");

    // The same answers as before the batch: every unarchived wiki for the
    // administrator, the public ones (WEB and the new ones) for erin.
    let all: Vec<Uuid> = sqlx::query_scalar(
        "SELECT p.id FROM projects p
         WHERE p.status <> 'archived'
           AND EXISTS (SELECT 1 FROM enabled_modules m WHERE m.project_id = p.id AND m.module = 'wiki')
         ORDER BY p.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(admin_many, all);
    assert_eq!(admin_many.len(), admin_few.len() + 300);
    let public: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM projects WHERE key LIKE 'P%' OR key = 'WEB' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(erin_many, public);

    // A fixed number of queries.
    assert!(admin_scans_few > 0 && erin_scans_few > 0);
    assert_eq!(admin_scans_many, admin_scans_few, "administrator");
    assert_eq!(erin_scans_many, erin_scans_few, "non-member");

    // And the search itself answers with every one of them.
    let r = search(&e, e.erin, WikiSearchParams::default()).await;
    assert!(r.available);
}
