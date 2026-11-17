// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.2: the wiki core (§3.1, §3.5 R-W1…R-W16, §3.11, §3.13):
//! T-M2-25 … T-M2-35 with 26a, 26b and 34a–34c (T-M2-36/37 are M2.3).
//!
//! Every `not_found` case also sends the same request for a target that
//! does not exist and compares the two errors (§7 rule). The concurrent
//! cases stop the first request at a fault gate while it holds its locks,
//! wait (bounded) until the second waits for a lock, release, and check
//! that no deadlock (40P01) happened. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ErrorCode, classify};
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::undo::{self, UndoInput};
use oxledger::wiki::{self, WikiEffect};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::future::Future;
use std::sync::Arc;
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
const EDITOR: [&str; 5] = [
    "view_wiki_pages",
    "view_wiki_edits",
    "edit_wiki_pages",
    "rename_wiki_pages",
    "delete_wiki_pages",
];
const READER: [&str; 1] = ["view_wiki_pages"];
/// Everything but reading (the user decision of 2026-11-15: editing
/// presupposes reading).
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
    /// Wiki administrator (and manage_project) in WEB, AUTH and OPS.
    alice: AuthContext,
    /// Editor in WEB, AUTH and OPS: no protect_wiki_pages, no manage_wiki.
    bob: AuthContext,
    /// Reader of WEB's and OPS's wiki only; not a member of AUTH.
    carol: AuthContext,
    /// Every wiki permission but view_wiki_pages in WEB.
    dave: AuthContext,
    /// No membership anywhere.
    erin: AuthContext,
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

/// Gives `user` `role` in `project` (a new membership or one more role).
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

/// WEB and AUTH have the wiki module, OPS does not.
async fn setup(pool: &PgPool) -> Env {
    let w = World::build(pool).await;
    faults::clear();
    let ops = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('OPS','Ops') RETURNING id",
    )
    .await;
    exec(
        pool,
        "INSERT INTO enabled_modules (project_id, module)
         SELECT id, 'wiki' FROM projects WHERE key IN ('WEB','AUTH')
         UNION ALL SELECT id, 'issue_tracking' FROM projects WHERE key = 'OPS'",
    )
    .await;
    let admin = role(pool, "WikiAdmin", &ADMIN).await;
    let editor = role(pool, "WikiEditor", &EDITOR).await;
    let reader = role(pool, "WikiReader", &READER).await;
    let blind = role(pool, "WikiBlind", &BLIND).await;
    for p in [w.web, w.auth_p, ops] {
        add_role(pool, p, w.alice, admin).await;
        add_role(pool, p, w.bob, editor).await;
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
        bob: sign_in(pool, &w.policy, "bob").await,
        carol: sign_in(pool, &w.policy, "carol").await,
        dave: sign_in(pool, &w.policy, "dave").await,
        erin: sign_in(pool, &w.policy, "erin").await,
        w,
    }
}

// ---------------------------------------------------------------- documents

fn doc(op: Value, pre: Option<(Uuid, i64)>) -> Value {
    let mut d = json!({"command_schema_version": 1, "ops": [op]});
    if let Some((p, lv)) = pre {
        d["preconditions"] = json!([{"wiki_page": p, "lock_version": lv}]);
    }
    d
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

fn at(project: &str, page: &str) -> Value {
    json!({"project": project, "page": page})
}

fn by_id(project: &str, page: Uuid) -> Value {
    json!({"project": project, "page_id": page})
}

async fn run(e: &Env, who: AuthContext, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(&e.w.app, who, Uuid::now_v7(), d).await
}

async fn create(e: &Env, who: AuthContext, project: &str, title: &str, body: &str) -> WikiEffect {
    let r = run(
        e,
        who,
        &doc(
            op("wiki_put", at(project, title), json!({"body": body})),
            None,
        ),
    )
    .await
    .unwrap_or_else(|err| panic!("creating {title}: {err:#}"));
    r.wiki[0].clone()
}

async fn create_under(e: &Env, who: AuthContext, title: &str, parent: Uuid) -> WikiEffect {
    let r = run(
        e,
        who,
        &doc(
            op(
                "wiki_put",
                at("WEB", title),
                json!({"body": title, "parent": {"page_id": parent}}),
            ),
            None,
        ),
    )
    .await
    .unwrap_or_else(|err| panic!("creating {title}: {err:#}"));
    r.wiki[0].clone()
}

async fn lv(e: &Env, page: Uuid) -> i64 {
    count(
        &e.w.pool,
        &format!("SELECT lock_version FROM wiki_pages WHERE id = '{page}'"),
    )
    .await
}

async fn edit(e: &Env, who: AuthContext, page: Uuid, body: &str) -> anyhow::Result<CommandResult> {
    let l = lv(e, page).await;
    run(
        e,
        who,
        &doc(
            op("wiki_put", by_id("WEB", page), json!({"body": body})),
            Some((page, l)),
        ),
    )
    .await
}

async fn mv(e: &Env, who: AuthContext, page: Uuid, extra: Value) -> anyhow::Result<CommandResult> {
    run(
        e,
        who,
        &doc(op("wiki_move", by_id("WEB", page), extra), None),
    )
    .await
}

async fn delete(
    e: &Env,
    who: AuthContext,
    page: Uuid,
    reparent: bool,
) -> anyhow::Result<CommandResult> {
    let extra = if reparent {
        json!({"children": "reparent"})
    } else {
        json!({})
    };
    run(
        e,
        who,
        &doc(op("wiki_delete", by_id("WEB", page), extra), None),
    )
    .await
}

async fn restore(e: &Env, who: AuthContext, page: Uuid) -> anyhow::Result<CommandResult> {
    run(
        e,
        who,
        &doc(op("wiki_restore", by_id("WEB", page), json!({})), None),
    )
    .await
}

async fn parent(e: &Env, page: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT parent_id FROM wiki_pages WHERE id = $1")
        .bind(page)
        .fetch_one(&e.w.pool)
        .await
        .unwrap()
}

/// `(code, message, details)` of an API error.
fn shape(e: &anyhow::Error) -> Value {
    let a = classify(e).unwrap_or_else(|| panic!("not an API error (a 500?): {e:#}"));
    json!({"code": a.code.as_str(), "message": a.message, "details": a.details})
}

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

fn err<T: std::fmt::Debug>(r: anyhow::Result<T>) -> anyhow::Error {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e,
    }
}

async fn deadlocks(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Runs `first` until it stops at the fault point `point` (holding its
/// locks), then `second` until it waits for a lock, then lets both finish.
/// Every wait is bounded; no deadlock may happen.
async fn race<A, B>(
    pool: &PgPool,
    point: &str,
    first: impl Future<Output = A> + Send + 'static,
    second: impl Future<Output = B> + Send + 'static,
) -> (A, B)
where
    A: Send + 'static,
    B: Send + 'static,
{
    exec(pool, "SELECT pg_stat_clear_snapshot()").await;
    let before = deadlocks(pool).await;
    let gate = Arc::new(Gate::default());
    faults::arm(point, Action::Block(gate.clone()));
    let a = tokio::spawn(first);
    gate.reached.notified().await;
    let b = tokio::spawn(second);
    common::poll_until(
        "the second request to wait for a lock the first holds",
        common::WAIT_TIMEOUT,
        async || {
            let n = count(
                pool,
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .await;
            if n > 0 {
                common::Break(())
            } else {
                common::Continue(String::new())
            }
        },
    )
    .await;
    gate.release.notify_one();
    let ra = common::join("the first request", a).await;
    let rb = common::join("the second request", b).await;
    faults::clear();
    exec(pool, "SELECT pg_stat_clear_snapshot()").await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    exec(pool, "SELECT pg_stat_clear_snapshot()").await;
    assert_eq!(deadlocks(pool).await, before, "a deadlock (40P01) happened");
    (ra, rb)
}

fn spawn_run(
    e: &Env,
    who: AuthContext,
    d: Value,
) -> impl Future<Output = anyhow::Result<CommandResult>> + Send + 'static {
    let app = e.w.app.clone();
    async move { commands::submit(&app, who, Uuid::now_v7(), &d).await }
}

// ---------------------------------------------------------------- T-M2-25

/// R-W3: create (version 1), update with the right lock_version (version
/// 2), a stale lock_version (`conflict` with the current state), the same
/// content (no version, no lock_version move, item `no_effect`).
#[tokio::test]
async fn t_m2_25_create_update_conflict_noop() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let r = run(
        &e,
        e.alice,
        &doc(
            op(
                "wiki_put",
                at("WEB", "Home"),
                json!({"body": "v1", "comment": "first"}),
            ),
            None,
        ),
    )
    .await
    .unwrap();
    let home = r.wiki[0].clone();
    assert!(home.created && home.changed);
    assert_eq!((home.version, home.lock_version), (1, 1));
    assert_eq!(r.items[0].status, "applied");
    assert_eq!(r.items[0].wiki_page_id, Some(home.page_id));
    assert!(!r.search.consistent_after.is_empty());

    let r = run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_put",
                by_id("WEB", home.page_id),
                json!({"body": "v2"}),
            ),
            Some((home.page_id, 1)),
        ),
    )
    .await
    .unwrap();
    assert_eq!((r.wiki[0].version, r.wiki[0].lock_version), (2, 2));
    assert!(r.wiki[0].changed && !r.wiki[0].created);

    // A stale lock_version: conflict with what the page is now.
    let stale = err(run(
        &e,
        e.alice,
        &doc(
            op("wiki_put", at("WEB", "Home"), json!({"body": "mine"})),
            Some((home.page_id, 1)),
        ),
    )
    .await);
    assert_eq!(code(&stale), ErrorCode::Conflict);
    let d = details(&stale);
    assert_eq!(d["page_id"], json!(home.page_id));
    assert_eq!(d["expected_lock_version"], 1);
    assert_eq!(d["lock_version"], 2);
    assert_eq!(d["current_version"], 2);
    assert_eq!(d["updated_by"], json!(e.w.bob));

    // The same content: nothing written.
    let r = run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_put",
                by_id("WEB", home.page_id),
                json!({"body": "v2"}),
            ),
            Some((home.page_id, 2)),
        ),
    )
    .await
    .unwrap();
    assert!(!r.wiki[0].changed);
    assert_eq!(r.items[0].status, "no_effect");
    assert_eq!((r.wiki[0].version, r.wiki[0].lock_version), (2, 2));
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM wiki_page_versions WHERE page_id = '{}'",
                home.page_id
            )
        )
        .await,
        2
    );

    // A create whose title is taken; an update by id without lock_version.
    let taken = err(run(
        &e,
        e.bob,
        &doc(
            op("wiki_put", at("WEB", "home"), json!({"body": "x"})),
            None,
        ),
    )
    .await);
    assert_eq!(code(&taken), ErrorCode::WikiPageExists);
    assert_eq!(details(&taken)["page_id"], json!(home.page_id));
    let bare = err(run(
        &e,
        e.bob,
        &doc(
            op("wiki_put", by_id("WEB", home.page_id), json!({"body": "x"})),
            None,
        ),
    )
    .await);
    assert_eq!(code(&bare), ErrorCode::InvalidArgument);

    // Resending a document returns its first result.
    let key = Uuid::now_v7();
    let d = doc(
        op(
            "wiki_put",
            by_id("WEB", home.page_id),
            json!({"body": "v3"}),
        ),
        Some((home.page_id, 2)),
    );
    let first = commands::submit(&e.w.app, e.bob, key, &d).await.unwrap();
    let again = commands::submit(&e.w.app, e.bob, key, &d).await.unwrap();
    assert_eq!(first.execution_id, again.execution_id);
    assert_eq!(again.wiki[0].version, 3);

    let page = wiki::page(&e.w.app, e.carol, "WEB", "Home").await.unwrap();
    assert_eq!(
        (page.body.as_str(), page.version, page.lock_version),
        ("v3", 3, 3)
    );
    assert_eq!(page.updated_by.as_ref().map(|p| p.id), Some(e.w.bob));
    assert!(!page.can.edit && !page.can.history);
    let page = wiki::page(&e.w.app, e.bob, "WEB", "Home").await.unwrap();
    assert!(page.can.edit && page.can.rename && page.can.delete && !page.can.protect);
    let vs = wiki::versions(&e.w.app, e.bob, "WEB", "Home", None)
        .await
        .unwrap();
    assert_eq!(
        vs.items.iter().map(|v| v.version).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert_eq!(vs.items[2].comment, "first");
}

// ---------------------------------------------------------------- T-M2-26

/// Two people create the same title (other case, an ideographic space) at
/// once: the wiki_settings lock serializes them, one gets the page, the
/// other `wiki_page_exists`.
#[tokio::test]
async fn t_m2_26_concurrent_create_same_title() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let (a, b) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.alice,
            doc(
                op("wiki_put", at("WEB", "Team Notes"), json!({"body": "a"})),
                None,
            ),
        ),
        spawn_run(
            &e,
            e.bob,
            doc(
                op(
                    "wiki_put",
                    at("WEB", "team\u{3000}NOTES"),
                    json!({"body": "b"}),
                ),
                None,
            ),
        ),
    )
    .await;
    let a = a.unwrap();
    let b = err(b);
    assert_eq!(code(&b), ErrorCode::WikiPageExists);
    assert_eq!(details(&b)["page_id"], json!(a.wiki[0].page_id));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE deleted_at IS NULL"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT page_count::bigint FROM wiki_settings WHERE project_id = '{}'",
                e.w.web
            )
        )
        .await,
        1
    );
}

// ---------------------------------------------------------------- T-M2-26a

/// R-W2 / C13: titles compare by NFKC_Casefold (width, case, ß); Cf
/// characters are refused with the character. Attachments are M2.4.
#[tokio::test]
async fn t_m2_26a_title_keys_and_format_chars() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let abc = create(&e, e.bob, "WEB", "ＡＢＣ", "wide").await;
    let same = err(run(
        &e,
        e.bob,
        &doc(op("wiki_put", at("WEB", "abc"), json!({"body": "x"})), None),
    )
    .await);
    assert_eq!(code(&same), ErrorCode::WikiPageExists);
    assert_eq!(details(&same)["page_id"], json!(abc.page_id));
    create(&e, e.bob, "WEB", "Straße", "de").await;
    let same = err(run(
        &e,
        e.bob,
        &doc(
            op("wiki_put", at("WEB", "STRASSE"), json!({"body": "x"})),
            None,
        ),
    )
    .await);
    assert_eq!(code(&same), ErrorCode::WikiPageExists);
    // Reading by another spelling of the key finds the page.
    let p = wiki::page(&e.w.app, e.carol, "WEB", "abc").await.unwrap();
    assert_eq!((p.id, p.title.as_str()), (abc.page_id, "ＡＢＣ"));

    for c in ['\u{202E}', '\u{200B}'] {
        let t = format!("a{c}b");
        let bad = err(run(
            &e,
            e.bob,
            &doc(op("wiki_put", at("WEB", &t), json!({"body": "x"})), None),
        )
        .await);
        assert_eq!(code(&bad), ErrorCode::WikiTitleInvalid);
        let d = details(&bad);
        assert_eq!(d["reason"], "char");
        assert_eq!(d["char"], c.to_string());
        assert_eq!(d["codepoint"], format!("U+{:04X}", c as u32));
        // A rename to it too.
        let bad = err(mv(&e, e.bob, abc.page_id, json!({"title": t})).await);
        assert_eq!(code(&bad), ErrorCode::WikiTitleInvalid);
    }
    for (t, reason) in [(".hidden", "leading_dot"), ("", "length")] {
        let bad = err(run(
            &e,
            e.bob,
            &doc(op("wiki_put", at("WEB", t), json!({"body": "x"})), None),
        )
        .await);
        assert_eq!(code(&bad), ErrorCode::WikiTitleInvalid, "{t:?}");
        assert_eq!(details(&bad)["reason"], reason);
    }
    // Attachment file names refuse format characters too (R-W2).
    let att = err(run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_put",
                at("WEB", "Files"),
                json!({"body": "x", "attachments": [{"upload_id": Uuid::now_v7(), "filename": "a\u{202E}gpj.exe"}]}),
            ),
            None,
        ),
    )
    .await);
    assert_eq!(code(&att), ErrorCode::InvalidValue);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM wiki_pages").await,
        2,
        "no refused title was written"
    );
}

// ---------------------------------------------------------------- T-M2-26b

/// A restore and a create of the same title at once, both orders: one
/// wins, the other is `wiki_page_exists` (never an internal error).
#[tokio::test]
async fn t_m2_26b_restore_and_create_race() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let plan = create(&e, e.alice, "WEB", "Plan", "old plan").await;
    delete(&e, e.alice, plan.page_id, false).await.unwrap();

    // The restore first.
    let (r, c) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.alice,
            doc(
                op("wiki_restore", by_id("WEB", plan.page_id), json!({})),
                None,
            ),
        ),
        spawn_run(
            &e,
            e.bob,
            doc(
                op("wiki_put", at("WEB", "plan"), json!({"body": "new"})),
                None,
            ),
        ),
    )
    .await;
    r.unwrap();
    let c = err(c);
    assert_eq!(code(&c), ErrorCode::WikiPageExists);
    assert_eq!(details(&c)["page_id"], json!(plan.page_id));

    // The create first.
    delete(&e, e.alice, plan.page_id, false).await.unwrap();
    let (c, r) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.bob,
            doc(
                op("wiki_put", at("WEB", "PLAN"), json!({"body": "new"})),
                None,
            ),
        ),
        spawn_run(
            &e,
            e.alice,
            doc(
                op("wiki_restore", by_id("WEB", plan.page_id), json!({})),
                None,
            ),
        ),
    )
    .await;
    let c = c.unwrap();
    let r = err(r);
    assert_eq!(code(&r), ErrorCode::WikiPageExists);
    assert_eq!(details(&r)["page_id"], json!(c.wiki[0].page_id));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_pages WHERE deleted_at IS NULL AND title_key = 'plan'"
        )
        .await,
        1
    );
}

// ---------------------------------------------------------------- T-M2-27

/// R-W4: a rename leaves a redirect; the old title opens the page with
/// `redirected_from`; a new page taking the old title removes it.
#[tokio::test]
async fn t_m2_27_redirects() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "Old Name", "body").await;
    let r = mv(&e, e.bob, p.page_id, json!({"title": "New Name"}))
        .await
        .unwrap();
    assert_eq!(r.wiki[0].title, "New Name");
    for t in ["Old_Name", "old name"] {
        let got = wiki::page(&e.w.app, e.carol, "WEB", t).await.unwrap();
        assert_eq!(got.id, p.page_id);
        assert_eq!(got.title, "New Name");
        assert!(got.redirected_from.is_some(), "{t}");
    }
    // A new page takes the old title: the redirect goes.
    let n = create(&e, e.bob, "WEB", "Old Name", "another").await;
    assert_eq!(count(&pool, "SELECT count(*) FROM wiki_redirects").await, 0);
    let got = wiki::page(&e.w.app, e.carol, "WEB", "Old_Name")
        .await
        .unwrap();
    assert_eq!(got.id, n.page_id);
    assert_eq!(got.redirected_from, None);
    // keep_redirect false leaves none.
    mv(
        &e,
        e.bob,
        n.page_id,
        json!({"title": "Third", "keep_redirect": false}),
    )
    .await
    .unwrap();
    let gone = err(wiki::page(&e.w.app, e.carol, "WEB", "Old_Name").await);
    let never = err(wiki::page(&e.w.app, e.carol, "WEB", "Never_Was").await);
    assert_eq!(code(&gone), ErrorCode::NotFound);
    assert_eq!(shape(&gone), shape(&never));
}

// ---------------------------------------------------------------- T-M2-28

/// R-W4: no cycles, at most 10 levels.
#[tokio::test]
async fn t_m2_28_cycle_and_depth() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let a = create(&e, e.bob, "WEB", "A", "a").await;
    let b = create_under(&e, e.bob, "B", a.page_id).await;
    let c = create_under(&e, e.bob, "C", b.page_id).await;
    for to in [c.page_id, a.page_id] {
        let bad = err(mv(&e, e.bob, a.page_id, json!({"parent": {"page_id": to}})).await);
        assert_eq!(code(&bad), ErrorCode::InvalidValue);
        assert_eq!(kind(&bad), "wiki_cycle");
    }
    // wiki_put moving the page is checked the same way.
    let l = lv(&e, a.page_id).await;
    let bad = err(run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_put",
                by_id("WEB", a.page_id),
                json!({"body": "a", "parent": {"page": "C"}}),
            ),
            Some((a.page_id, l)),
        ),
    )
    .await);
    assert_eq!(kind(&bad), "wiki_cycle");
    assert_eq!(parent(&e, a.page_id).await, None);

    // D1 … D10: ten levels.
    let mut chain = vec![create(&e, e.bob, "WEB", "D1", "d").await];
    for i in 2..=10 {
        let up = chain.last().unwrap().page_id;
        chain.push(create_under(&e, e.bob, &format!("D{i}"), up).await);
    }
    let bad = err(run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_put",
                at("WEB", "D11"),
                json!({"body": "d", "parent": {"page_id": chain[9].page_id}}),
            ),
            None,
        ),
    )
    .await);
    assert_eq!(code(&bad), ErrorCode::InvalidValue);
    assert_eq!(kind(&bad), "wiki_depth");
    // A subtree of height 3 (A/B/C) under D8 would reach 11; under D7, 10.
    let bad = err(mv(
        &e,
        e.bob,
        a.page_id,
        json!({"parent": {"page_id": chain[7].page_id}}),
    )
    .await);
    assert_eq!(kind(&bad), "wiki_depth");
    mv(
        &e,
        e.bob,
        a.page_id,
        json!({"parent": {"page_id": chain[6].page_id}}),
    )
    .await
    .unwrap();
    assert_eq!(parent(&e, a.page_id).await, Some(chain[6].page_id));
}

// ---------------------------------------------------------------- T-M2-29

/// R-W5: a page with children needs `reparent`; the children move up;
/// restore within 30 days; a restore whose title is taken.
#[tokio::test]
async fn t_m2_29_delete_reparent_restore() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let root = create(&e, e.alice, "WEB", "Root", "r").await;
    let p = create_under(&e, e.alice, "Parent", root.page_id).await;
    let c1 = create_under(&e, e.alice, "Child 1", p.page_id).await;
    let c2 = create_under(&e, e.alice, "Child 2", p.page_id).await;

    let bad = err(delete(&e, e.bob, p.page_id, false).await);
    assert_eq!(code(&bad), ErrorCode::WikiHasChildren);
    assert_eq!(details(&bad)["children"], 2);
    assert_eq!(details(&bad)["page_id"], json!(p.page_id));

    let r = delete(&e, e.bob, p.page_id, true).await.unwrap();
    let mut moved = r.wiki[0].reparented.clone();
    moved.sort();
    let mut want = vec![c1.page_id, c2.page_id];
    want.sort();
    assert_eq!(moved, want);
    for c in [c1.page_id, c2.page_id] {
        assert_eq!(parent(&e, c).await, Some(root.page_id));
    }
    let gone = err(wiki::page(&e.w.app, e.bob, "WEB", "Parent").await);
    assert_eq!(code(&gone), ErrorCode::NotFound);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT page_count::bigint FROM wiki_settings WHERE project_id = '{}'",
                e.w.web
            )
        )
        .await,
        3
    );
    let list = wiki::deleted(&e.w.app, e.alice, "WEB").await.unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].id, p.page_id);
    assert!(!list.items[0].title_taken);

    // bob has no manage_wiki: the deleted page does not exist for him.
    let his = err(restore(&e, e.bob, p.page_id).await);
    let none = err(restore(&e, e.bob, Uuid::now_v7()).await);
    assert_eq!(code(&his), ErrorCode::NotFound);
    assert_eq!(shape(&his), shape(&none));

    restore(&e, e.alice, p.page_id).await.unwrap();
    let back = wiki::page(&e.w.app, e.bob, "WEB", "Parent").await.unwrap();
    assert_eq!(back.parent.map(|l| l.id), Some(root.page_id));

    // Deleted again; a new page takes the title; the restore is refused.
    delete(&e, e.alice, p.page_id, false).await.unwrap();
    let other = create(&e, e.bob, "WEB", "parent", "mine now").await;
    let list = wiki::deleted(&e.w.app, e.alice, "WEB").await.unwrap();
    assert!(list.items[0].title_taken);
    let taken = err(restore(&e, e.alice, p.page_id).await);
    assert_eq!(code(&taken), ErrorCode::WikiPageExists);
    assert_eq!(details(&taken)["page_id"], json!(other.page_id));

    // After 30 days the page is gone for good.
    exec(
        &pool,
        &format!(
            "UPDATE wiki_pages SET deleted_at = now() - interval '31 days' WHERE id = '{}'",
            p.page_id
        ),
    )
    .await;
    let old = err(restore(&e, e.alice, p.page_id).await);
    let none = err(restore(&e, e.alice, Uuid::now_v7()).await);
    assert_eq!(shape(&old), shape(&none));
    let old = err(wiki::deleted_page(&e.w.app, e.alice, "WEB", p.page_id).await);
    assert_eq!(code(&old), ErrorCode::NotFound);
    assert!(
        wiki::deleted(&e.w.app, e.alice, "WEB")
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

// ---------------------------------------------------------------- T-M2-30

/// R-W6: a protected page is changed only with protect_wiki_pages.
#[tokio::test]
async fn t_m2_30_protected_page() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "Rules", "be nice").await;
    run(
        &e,
        e.alice,
        &doc(
            op(
                "wiki_protect",
                by_id("WEB", p.page_id),
                json!({"protected": true}),
            ),
            None,
        ),
    )
    .await
    .unwrap();
    let page = wiki::page(&e.w.app, e.bob, "WEB", "Rules").await.unwrap();
    assert!(page.protected);
    assert!(!page.can.edit && !page.can.rename && !page.can.delete && !page.can.protect);
    let before = lv(&e, p.page_id).await;
    for r in [
        edit(&e, e.bob, p.page_id, "be mean").await,
        mv(&e, e.bob, p.page_id, json!({"title": "Suggestions"})).await,
        delete(&e, e.bob, p.page_id, false).await,
    ] {
        let x = err(r);
        assert_eq!(code(&x), ErrorCode::ForbiddenOp);
        assert_eq!(details(&x)["reason"], "wiki_protected");
    }
    let x = err(run(
        &e,
        e.bob,
        &doc(
            op(
                "wiki_protect",
                by_id("WEB", p.page_id),
                json!({"protected": false}),
            ),
            None,
        ),
    )
    .await);
    assert_eq!(code(&x), ErrorCode::ForbiddenOp);
    assert_eq!(details(&x)["reason"], "permission");
    assert_eq!(lv(&e, p.page_id).await, before);
    // The protector edits.
    let r = edit(&e, e.alice, p.page_id, "be kind").await.unwrap();
    assert_eq!(r.wiki[0].version, 2);
    let page = wiki::page(&e.w.app, e.alice, "WEB", "Rules").await.unwrap();
    assert!(page.can.edit && page.can.protect);
}

// ---------------------------------------------------------------- T-M2-31

/// R-W1 and the user decision of 2026-11-15: whoever cannot read a wiki
/// (no view_wiki_pages even with every other wiki permission, no
/// membership, the module off, a project they cannot see) gets the one
/// `not_found` of a page that does not exist, for every read and every op.
#[tokio::test]
async fn t_m2_31_existence_is_not_leaked() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let web = create(&e, e.alice, "WEB", "Real", "secret plan").await;
    edit(&e, e.alice, web.page_id, "secret plan 2")
        .await
        .unwrap();
    let auth_page = create(&e, e.alice, "AUTH", "Real", "auth secret").await;
    // OPS had its wiki on once: a page exists, then the module went off.
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects WHERE key = 'OPS'",
    )
    .await;
    let ops_page = create(&e, e.alice, "OPS", "Real", "ops secret").await;
    exec(
        &pool,
        "DELETE FROM enabled_modules WHERE module = 'wiki' AND project_id = (SELECT id FROM projects WHERE key = 'OPS')",
    )
    .await;
    let deleted = create(&e, e.alice, "WEB", "Deleted", "x").await;
    delete(&e, e.alice, deleted.page_id, false).await.unwrap();

    // The reference: a page that does not exist in a wiki carol reads.
    let reference = shape(&err(
        wiki::page(&e.w.app, e.carol, "WEB", "No_Such_Page").await
    ));
    assert_eq!(reference["code"], "not_found");

    // (who, project, an existing page there, its title)
    let cases: Vec<(&str, AuthContext, &str, Uuid)> = vec![
        (
            "dave: every permission but view",
            e.dave,
            "WEB",
            web.page_id,
        ),
        ("erin: no membership", e.erin, "WEB", web.page_id),
        (
            "carol: a project she cannot see",
            e.carol,
            "AUTH",
            auth_page.page_id,
        ),
        ("alice: the module is off", e.alice, "OPS", ops_page.page_id),
        ("bob: the module is off", e.bob, "OPS", ops_page.page_id),
        ("carol: a deleted page", e.carol, "WEB", deleted.page_id),
    ];
    let app = &e.w.app;
    for (label, who, project, page) in cases {
        let title = if page == deleted.page_id {
            "Deleted"
        } else {
            "Real"
        };
        let ghost = Uuid::now_v7();
        // Reads: the real page and one that does not exist.
        for t in [title, "Not_There"] {
            let mut errs = vec![
                shape(&err(wiki::page(app, who, project, t).await)),
                shape(&err(wiki::versions(app, who, project, t, None).await)),
                shape(&err(wiki::version(app, who, project, t, 1).await)),
                shape(&err(wiki::page_diff(app, who, project, t, None, None).await)),
                shape(&err(wiki::events(app, who, project, t).await)),
            ];
            if page != deleted.page_id {
                errs.push(shape(&err(wiki::index(app, who, project).await)));
                errs.push(shape(&err(wiki::settings(app, who, project).await)));
                errs.push(shape(&err(wiki::deleted(app, who, project).await)));
                errs.push(shape(&err(wiki::deleted_page(
                    app,
                    who,
                    project,
                    deleted.page_id,
                )
                .await)));
            }
            for (i, s) in errs.iter().enumerate() {
                assert_eq!(s, &reference, "{label}: read {i} of {t}");
            }
        }
        // Every op, on the real page and on one that does not exist.
        for target in [page, ghost] {
            let l = count(
                &pool,
                &format!(
                    "SELECT coalesce(max(lock_version), 1) FROM wiki_pages WHERE id = '{target}'"
                ),
            )
            .await;
            let docs = [
                doc(
                    op("wiki_put", by_id(project, target), json!({"body": "pwned"})),
                    Some((target, l)),
                ),
                doc(
                    op(
                        "wiki_move",
                        by_id(project, target),
                        json!({"title": "Moved"}),
                    ),
                    None,
                ),
                doc(op("wiki_delete", by_id(project, target), json!({})), None),
                doc(op("wiki_restore", by_id(project, target), json!({})), None),
                doc(
                    op(
                        "wiki_protect",
                        by_id(project, target),
                        json!({"protected": true}),
                    ),
                    None,
                ),
            ];
            for d in &docs {
                let s = shape(&err(run(&e, who, d).await));
                assert_eq!(s, reference, "{label}: {}", d["ops"][0]["kind"]);
            }
        }
        // A put by title: the real title and a new one.
        if page != deleted.page_id {
            for t in [title, "Brand New"] {
                let s = shape(&err(run(
                    &e,
                    who,
                    &doc(op("wiki_put", at(project, t), json!({"body": "x"})), None),
                )
                .await));
                assert_eq!(s, reference, "{label}: put {t}");
            }
        }
        // D12 on a wiki one cannot read.
        let s = shape(&err(redaction::redact(
            app,
            who,
            &e.w.policy,
            Uuid::now_v7(),
            &Target::WikiVersion {
                project: project.into(),
                page: title.into(),
                version: 1,
            },
            &RedactInput {
                reason_code: "confidential".into(),
                reason: "test".into(),
                attachments: Vec::new(),
            },
        )
        .await));
        assert_eq!(s, reference, "{label}: redact");
    }
    // A project that does not exist at all.
    let s = shape(&err(wiki::page(app, e.alice, "NOPE", "Real").await));
    assert_eq!(s, reference);
    let s = shape(&err(run(
        &e,
        e.alice,
        &doc(
            op("wiki_put", at("NOPE", "Real"), json!({"body": "x"})),
            None,
        ),
    )
    .await));
    assert_eq!(s, reference);
    // Nothing was written.
    assert_eq!(lv(&e, web.page_id).await, 2);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM wiki_pages").await,
        4,
        "no page was created"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM wiki_pages WHERE protected").await,
        0
    );
}

// ---------------------------------------------------------------- T-M2-32

/// R-W7: view_wiki_pages without view_wiki_edits reads pages, not their
/// history (`forbidden_op`: the wiki's existence is no secret to them).
#[tokio::test]
async fn t_m2_32_history_needs_view_wiki_edits() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.bob, "WEB", "Notes", "one").await;
    edit(&e, e.bob, p.page_id, "two").await.unwrap();
    let page = wiki::page(&e.w.app, e.carol, "WEB", "Notes").await.unwrap();
    assert_eq!(page.body, "two");
    assert!(!page.can.history);
    let app = &e.w.app;
    for x in [
        err(wiki::versions(app, e.carol, "WEB", "Notes", None).await),
        err(wiki::version(app, e.carol, "WEB", "Notes", 1).await),
        err(wiki::page_diff(app, e.carol, "WEB", "Notes", None, None).await),
        err(wiki::events(app, e.carol, "WEB", "Notes").await),
    ] {
        assert_eq!(code(&x), ErrorCode::ForbiddenOp);
        assert_eq!(details(&x)["reason"], "permission");
    }
    // bob has it.
    assert_eq!(
        wiki::version(app, e.bob, "WEB", "Notes", 1)
            .await
            .unwrap()
            .body,
        "one"
    );
    let d = wiki::page_diff(app, e.bob, "WEB", "Notes", None, None)
        .await
        .unwrap();
    assert_eq!((d.from, d.to), (1, 2));
}

// ---------------------------------------------------------------- T-M2-33

fn changed_lines(d: &wiki::WikiDiff) -> usize {
    d.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.op != "equal")
        .count()
}

/// R-W7: a big page, Chinese words, more than 2,000 changed lines.
#[tokio::test]
async fn t_m2_33_diff() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    // 5,000 lines, one changed: one hunk with 3 lines of context.
    let old: Vec<String> = (1..=5000).map(|i| format!("line {i}")).collect();
    let mut new = old.clone();
    new[2499] = "line 2500 changed".into();
    let p = create(&e, e.bob, "WEB", "Big", &old.join("\n")).await;
    edit(&e, e.bob, p.page_id, &new.join("\n")).await.unwrap();
    let d = wiki::page_diff(app, e.bob, "WEB", "Big", None, None)
        .await
        .unwrap();
    assert!(!d.truncated);
    assert_eq!(d.hunks.len(), 1);
    let h = &d.hunks[0];
    assert_eq!((h.old_start, h.new_start), (2497, 2497));
    let ops: Vec<&str> = h.lines.iter().map(|l| l.op.as_str()).collect();
    assert_eq!(
        ops,
        [
            "equal", "equal", "equal", "delete", "insert", "equal", "equal", "equal"
        ]
    );
    let ins = h.lines[4].words.as_ref().unwrap();
    assert_eq!(ins.last().unwrap().op, "insert");
    assert_eq!(ins.last().unwrap().text, " changed");

    // Chinese compares by character, ASCII by word.
    let c = create(&e, e.bob, "WEB", "會議", "我們今天開會討論 wiki 設計").await;
    edit(&e, e.bob, c.page_id, "我們明天開會討論 Wiki 設計")
        .await
        .unwrap();
    let d = wiki::page_diff(app, e.bob, "WEB", "會議", None, None)
        .await
        .unwrap();
    let lines = &d.hunks[0].lines;
    let del: Vec<(String, String)> = lines[0]
        .words
        .as_ref()
        .unwrap()
        .iter()
        .map(|w| (w.op.clone(), w.text.clone()))
        .collect();
    assert_eq!(
        del,
        [
            ("equal", "我們"),
            ("delete", "今"),
            ("equal", "天開會討論 "),
            ("delete", "wiki"),
            ("equal", " 設計"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let ins: Vec<(String, String)> = lines[1]
        .words
        .as_ref()
        .unwrap()
        .iter()
        .map(|w| (w.op.clone(), w.text.clone()))
        .collect();
    assert!(ins.contains(&("insert".into(), "明".into())));
    assert!(ins.contains(&("insert".into(), "Wiki".into())));
    // from = 0: everything is new.
    let d = wiki::page_diff(app, e.bob, "WEB", "會議", Some(0), Some(1))
        .await
        .unwrap();
    assert_eq!(changed_lines(&d), 1);
    assert_eq!(d.hunks[0].lines[0].op, "insert");

    // 1,500 lines replaced by 1,500 others: 3,000 changed, 2,000 shown.
    let a: Vec<String> = (1..=1500).map(|i| format!("a{i}")).collect();
    let b: Vec<String> = (1..=1500).map(|i| format!("b{i}")).collect();
    let t = create(&e, e.bob, "WEB", "Rewrite", &a.join("\n")).await;
    edit(&e, e.bob, t.page_id, &b.join("\n")).await.unwrap();
    let d = wiki::page_diff(app, e.bob, "WEB", "Rewrite", None, None)
        .await
        .unwrap();
    assert!(d.truncated);
    assert_eq!(changed_lines(&d), wiki::DIFF_MAX_LINES);
    // A version that does not exist.
    let x = err(wiki::page_diff(app, e.bob, "WEB", "Rewrite", Some(1), Some(9)).await);
    assert_eq!(code(&x), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- T-M2-34

/// R-W9: undo of one's own wiki write; `changed_since` after someone
/// else's; `redacted_source` when the version to go back to is redacted.
#[tokio::test]
async fn t_m2_34_undo_content() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Doc", "first").await;
    let x = edit(&e, e.alice, p.page_id, "second").await.unwrap();
    let pv = undo::preview(app, e.alice, x.execution_id).await.unwrap();
    assert_eq!(pv.wiki.len(), 1);
    let w = &pv.wiki[0];
    assert_eq!((w.change.as_str(), w.restorable), ("updated", true));
    assert_eq!(w.before["version"], 1);
    assert_eq!(w.after["version"], 2);
    assert_eq!(w.current["version"], 2);
    let u = undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        x.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.wiki_pages_restored, 1);
    assert_eq!(u.reverts_execution_id, x.execution_id);
    let page = wiki::page(app, e.alice, "WEB", "Doc").await.unwrap();
    assert_eq!((page.body.as_str(), page.version), ("first", 3));
    let v = wiki::version(app, e.alice, "WEB", "Doc", 3).await.unwrap();
    assert_eq!(v.comment, "復原：回到第 1 版");
    // Undone once: the preview says so, a second undo has nothing to do.
    let pv = undo::preview(app, e.alice, x.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("changed_since"));

    // Someone else wrote after: changed_since.
    let y = edit(&e, e.alice, p.page_id, "third").await.unwrap();
    edit(&e, e.bob, p.page_id, "bob's").await.unwrap();
    let pv = undo::preview(app, e.alice, y.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("changed_since"));
    assert_eq!(pv.wiki[0].current["version"], 5);
    let x2 = err(undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        y.execution_id,
        &UndoInput::default(),
    )
    .await);
    assert_eq!(code(&x2), ErrorCode::UndoNotApplicable);
    assert_eq!(
        wiki::page(app, e.alice, "WEB", "Doc").await.unwrap().body,
        "bob's"
    );

    // The version to go back to is redacted: redacted_source.
    let q = create(&e, e.alice, "WEB", "Doc2", "leaked token").await;
    let z = edit(&e, e.alice, q.page_id, "clean").await.unwrap();
    redaction::redact(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        &Target::WikiVersion {
            project: "WEB".into(),
            page: "Doc2".into(),
            version: 1,
        },
        &RedactInput {
            reason_code: "confidential".into(),
            reason: "a token".into(),
            attachments: Vec::new(),
        },
    )
    .await
    .unwrap();
    let pv = undo::preview(app, e.alice, z.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("redacted_source"));
    let x3 = err(undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        z.execution_id,
        &UndoInput::default(),
    )
    .await);
    assert_eq!(code(&x3), ErrorCode::UndoNotApplicable);
    let v1 = wiki::version(app, e.alice, "WEB", "Doc2", 1).await.unwrap();
    assert!(v1.redacted);
    assert!(!v1.body.contains("leaked"));
    // Another person cannot undo alice's execution.
    let x4 = err(undo::preview(app, e.bob, z.execution_id).await);
    assert_eq!(code(&x4), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- T-M2-34a

/// R-W9 / B-56: undoing a create soft-deletes the page (restorable);
/// undoing a delete with reparent restores the page and moves back the
/// children nobody moved since (the moved one is listed, untouched).
#[tokio::test]
async fn t_m2_34a_undo_create_and_reparenting_delete() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let r = run(
        &e,
        e.alice,
        &doc(
            op("wiki_put", at("WEB", "Fresh"), json!({"body": "new"})),
            None,
        ),
    )
    .await
    .unwrap();
    let fresh = r.wiki[0].page_id;
    let pv = undo::preview(app, e.alice, r.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].change, "created");
    assert!(pv.wiki[0].restorable);
    undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        r.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        code(&err(wiki::page(app, e.alice, "WEB", "Fresh").await)),
        ErrorCode::NotFound
    );
    let list = wiki::deleted(app, e.alice, "WEB").await.unwrap();
    assert_eq!(list.items[0].id, fresh);
    restore(&e, e.alice, fresh).await.unwrap();
    assert_eq!(
        wiki::page(app, e.alice, "WEB", "Fresh").await.unwrap().body,
        "new"
    );

    // A delete with reparent, then bob moves one child away.
    let p = create(&e, e.alice, "WEB", "Parent", "p").await;
    let c1 = create_under(&e, e.alice, "C1", p.page_id).await;
    let c2 = create_under(&e, e.alice, "C2", p.page_id).await;
    let c3 = create_under(&e, e.alice, "C3", p.page_id).await;
    let d = delete(&e, e.alice, p.page_id, true).await.unwrap();
    mv(&e, e.bob, c2.page_id, json!({"parent": {"page": "Fresh"}}))
        .await
        .unwrap();
    let pv = undo::preview(app, e.alice, d.execution_id).await.unwrap();
    let by: std::collections::BTreeMap<Uuid, &wiki::UndoWikiPage> =
        pv.wiki.iter().map(|w| (w.page_id, w)).collect();
    assert_eq!(by[&p.page_id].change, "deleted");
    assert!(by[&p.page_id].restorable);
    for c in [c1.page_id, c3.page_id] {
        assert_eq!(by[&c].change, "reparented");
        assert!(by[&c].restorable);
    }
    assert_eq!(by[&c2.page_id].change, "reparented");
    assert_eq!(by[&c2.page_id].reason.as_deref(), Some("changed_since"));
    let u = undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        d.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.wiki_pages_restored, 3);
    let page = wiki::page(app, e.alice, "WEB", "Parent").await.unwrap();
    assert_eq!(page.id, p.page_id);
    assert_eq!(parent(&e, c1.page_id).await, Some(p.page_id));
    assert_eq!(parent(&e, c3.page_id).await, Some(p.page_id));
    assert_eq!(parent(&e, c2.page_id).await, Some(fresh));
}

// ---------------------------------------------------------------- T-M2-34b

/// L2: A under B and B under A at once: the wiki_settings lock serializes
/// them, the second is `wiki_cycle`, no cycle is written.
#[tokio::test]
async fn t_m2_34b_concurrent_moves_make_no_cycle() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let a = create(&e, e.alice, "WEB", "A", "a").await;
    let b = create(&e, e.alice, "WEB", "B", "b").await;
    let (first, second) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.alice,
            doc(
                op(
                    "wiki_move",
                    by_id("WEB", a.page_id),
                    json!({"parent": {"page_id": b.page_id}}),
                ),
                None,
            ),
        ),
        spawn_run(
            &e,
            e.bob,
            doc(
                op(
                    "wiki_move",
                    by_id("WEB", b.page_id),
                    json!({"parent": {"page_id": a.page_id}}),
                ),
                None,
            ),
        ),
    )
    .await;
    first.unwrap();
    let second = err(second);
    assert_eq!(code(&second), ErrorCode::InvalidValue);
    assert_eq!(kind(&second), "wiki_cycle");
    assert_eq!(parent(&e, a.page_id).await, Some(b.page_id));
    assert_eq!(parent(&e, b.page_id).await, None);
}

// ---------------------------------------------------------------- T-M2-34c

/// R-W16: a deleted page is `not_found` to readers (like one that never
/// existed); `manage_wiki` reads it under /wiki/deleted.
#[tokio::test]
async fn t_m2_34c_deleted_page_reads() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Gone", "it was here").await;
    edit(&e, e.alice, p.page_id, "it was here 2").await.unwrap();
    delete(&e, e.alice, p.page_id, false).await.unwrap();
    for who in [e.carol, e.bob, e.alice] {
        let gone = err(wiki::page(app, who, "WEB", "Gone").await);
        let never = err(wiki::page(app, who, "WEB", "Never").await);
        assert_eq!(code(&gone), ErrorCode::NotFound);
        assert_eq!(shape(&gone), shape(&never));
    }
    let got = wiki::deleted_page(app, e.alice, "WEB", p.page_id)
        .await
        .unwrap();
    assert_eq!((got.body.as_str(), got.version), ("it was here 2", 2));
    assert_eq!(got.versions.as_ref().map(Vec::len), Some(2));
    assert_eq!(got.deleted_by.as_ref().map(|d| d.id), Some(e.w.alice));
    // Without manage_wiki: forbidden_op (they read the wiki; the list is
    // what needs the permission, checked before any page is looked up).
    for who in [e.bob, e.carol] {
        let x = err(wiki::deleted_page(app, who, "WEB", p.page_id).await);
        let y = err(wiki::deleted_page(app, who, "WEB", Uuid::now_v7()).await);
        assert_eq!(code(&x), ErrorCode::ForbiddenOp);
        assert_eq!(shape(&x), shape(&y));
        assert_eq!(
            code(&err(wiki::deleted(app, who, "WEB").await)),
            ErrorCode::ForbiddenOp
        );
    }
    // A live page is not a deleted one.
    let live = create(&e, e.alice, "WEB", "Alive", "x").await;
    let x = err(wiki::deleted_page(app, e.alice, "WEB", live.page_id).await);
    let y = err(wiki::deleted_page(app, e.alice, "WEB", Uuid::now_v7()).await);
    assert_eq!(code(&x), ErrorCode::NotFound);
    assert_eq!(shape(&x), shape(&y));
    // The tree and the index leave it out.
    let idx = wiki::index(app, e.carol, "WEB").await.unwrap();
    assert!(idx.tree.iter().all(|t| t.id != p.page_id));
}

// ---------------------------------------------------------------- T-M2-35

/// R-W10: a page create and a D12 redaction of the same project at once,
/// both orders, serialize without deadlock.
#[tokio::test]
async fn t_m2_35_create_and_redaction_serialize() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let t = create(&e, e.alice, "WEB", "Target", "one").await;
    edit(&e, e.alice, t.page_id, "two").await.unwrap();
    edit(&e, e.alice, t.page_id, "three").await.unwrap();
    let redact = |version: i32| {
        let app = e.w.app.clone();
        let policy = e.w.policy.clone();
        let who = e.alice;
        async move {
            redaction::redact(
                &app,
                who,
                &policy,
                Uuid::now_v7(),
                &Target::WikiVersion {
                    project: "WEB".into(),
                    page: "Target".into(),
                    version,
                },
                &RedactInput {
                    reason_code: "confidential".into(),
                    reason: "test".into(),
                    attachments: Vec::new(),
                },
            )
            .await
        }
    };
    let epoch0 = epoch(&pool, e.w.web).await;
    // The create first (it holds the project FOR SHARE and wiki_settings).
    let (c, r) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.bob,
            doc(
                op("wiki_put", at("WEB", "New One"), json!({"body": "n"})),
                None,
            ),
        ),
        redact(1),
    )
    .await;
    c.unwrap();
    assert!(r.unwrap().changed);
    // The redaction first (it holds the project FOR UPDATE).
    let (r, c) = race(
        &pool,
        "redact.before_derived",
        redact(2),
        spawn_run(
            &e,
            e.bob,
            doc(
                op("wiki_put", at("WEB", "Another"), json!({"body": "n"})),
                None,
            ),
        ),
    )
    .await;
    assert!(r.unwrap().changed);
    c.unwrap();
    assert_eq!(epoch(&pool, e.w.web).await, epoch0 + 2);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM wiki_page_versions WHERE page_id = '{}' AND redaction_id IS NOT NULL",
                t.page_id
            )
        )
        .await,
        2
    );
}

/// R-W10: two creates at once when one page is left: one gets it, the
/// other `wiki_page_limit`; the count never passes 5,000.
#[tokio::test]
async fn t_m2_35_page_limit_under_concurrency() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    exec(
        &pool,
        &format!(
            "UPDATE wiki_settings SET page_count = {} WHERE project_id = '{}'",
            wiki::MAX_PAGES - 1,
            e.w.web
        ),
    )
    .await;
    let (a, b) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.alice,
            doc(op("wiki_put", at("WEB", "L1"), json!({"body": "x"})), None),
        ),
        spawn_run(
            &e,
            e.bob,
            doc(op("wiki_put", at("WEB", "L2"), json!({"body": "x"})), None),
        ),
    )
    .await;
    a.unwrap();
    let b = err(b);
    assert_eq!(code(&b), ErrorCode::InvalidValue);
    assert_eq!(kind(&b), "wiki_page_limit");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT page_count::bigint FROM wiki_settings WHERE project_id = '{}'",
                e.w.web
            )
        )
        .await,
        i64::from(wiki::MAX_PAGES)
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM wiki_pages").await, 1);
}

// ---------------------------------------------------------------- verification fixes

async fn redact_title(e: &Env, page: &str, new_title: &str) {
    redaction::redact(
        &e.w.app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        &Target::WikiTitle {
            project: "WEB".into(),
            page: page.into(),
            new_title: new_title.into(),
        },
        &RedactInput {
            reason_code: "confidential".into(),
            reason: "test".into(),
            attachments: Vec::new(),
        },
    )
    .await
    .unwrap();
}

/// D12: no stored result of a wiki request holds a title; a resent
/// PUT /wiki/settings after a title redaction shows the title as it is now.
#[tokio::test]
async fn v_settings_replay_after_title_redaction() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "Secret Plan", "x").await;
    let key = Uuid::now_v7();
    let input = wiki::WikiSettingsPut {
        start_page_id: Some(p.page_id),
        lock_version: 0,
    };
    let s = wiki::put_settings(&e.w.app, e.alice, key, "WEB", &input)
        .await
        .unwrap();
    assert_eq!(s.start_page.unwrap().title, "Secret Plan");
    redact_title(&e, "Secret_Plan", "Plan").await;
    let again = wiki::put_settings(&e.w.app, e.alice, key, "WEB", &input)
        .await
        .unwrap();
    assert_eq!(again.start_page_id, Some(p.page_id));
    assert_eq!(again.start_page.unwrap().title, "Plan");
    assert_eq!(again.lock_version, s.lock_version);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM operation_requests WHERE result_ref::text LIKE '%Secret%'"
        )
        .await,
        0,
        "a stored result keeps a redacted title"
    );
}

/// A resent PUT /wiki/settings links the page it stored (by its title now),
/// not the start page set since; a deleted stored page links nothing (no
/// fallback to the "Wiki" page).
#[tokio::test]
async fn v_settings_replay_links_the_stored_page() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let a = create(&e, e.alice, "WEB", "Alpha", "a").await;
    let b = create(&e, e.alice, "WEB", "Beta", "b").await;
    create(&e, e.alice, "WEB", "Wiki", "w").await;
    let key = Uuid::now_v7();
    let first = wiki::WikiSettingsPut {
        start_page_id: Some(a.page_id),
        lock_version: 0,
    };
    let s = wiki::put_settings(app, e.alice, key, "WEB", &first)
        .await
        .unwrap();
    let second = wiki::WikiSettingsPut {
        start_page_id: Some(b.page_id),
        lock_version: s.lock_version,
    };
    wiki::put_settings(app, e.alice, Uuid::now_v7(), "WEB", &second)
        .await
        .unwrap();
    let again = wiki::put_settings(app, e.alice, key, "WEB", &first)
        .await
        .unwrap();
    assert_eq!(again.start_page_id, Some(a.page_id));
    let link = again.start_page.unwrap();
    assert_eq!((link.id, link.title.as_str()), (a.page_id, "Alpha"));
    delete(&e, e.alice, a.page_id, false).await.unwrap();
    let gone = wiki::put_settings(app, e.alice, key, "WEB", &first)
        .await
        .unwrap();
    assert_eq!(gone.start_page_id, Some(a.page_id));
    assert!(gone.start_page.is_none(), "{:?}", gone.start_page);
}

/// R-W2: runs of spaces and `_` are one `_` in the key, so every title
/// opens through its own URL and `a b`, `a_b`, `a__b` are one title; links
/// and D12 name pages the same way.
#[tokio::test]
async fn v_underscores_in_titles() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let init = create(&e, e.bob, "WEB", "__init__", "py").await;
    let got = wiki::page(app, e.carol, "WEB", &wiki::url_title("__init__"))
        .await
        .unwrap();
    assert_eq!(got.id, init.page_id);
    let ab = create(&e, e.bob, "WEB", "a b", "space").await;
    for t in ["a__b", "a_b", "A _ B"] {
        let x = err(run(
            &e,
            e.bob,
            &doc(op("wiki_put", at("WEB", t), json!({"body": "x"})), None),
        )
        .await);
        assert_eq!(code(&x), ErrorCode::WikiPageExists, "{t}");
        assert_eq!(details(&x)["page_id"], json!(ab.page_id));
    }
    for u in ["a__b", "a_b"] {
        assert_eq!(
            wiki::page(app, e.carol, "WEB", u).await.unwrap().id,
            ab.page_id
        );
    }
    // [[…]] links resolve by the same key.
    let input: oxledger::markdown::MarkdownPreviewInput = serde_json::from_value(json!({
        "context": {"wiki_project": "WEB"},
        "text": "[[a__b]] and [[__init__]]",
    }))
    .unwrap();
    let html = oxledger::markdown::preview(app, e.bob, &input)
        .await
        .unwrap()
        .html;
    assert!(
        html.contains(r#"class="ox-wiki" href="/projects/WEB/wiki/a__b""#),
        "{html}"
    );
    assert!(
        html.contains(r#"class="ox-wiki" href="/projects/WEB/wiki/__init__""#),
        "{html}"
    );
    // D12 names the page by any spelling of its key.
    redact_title(&e, "a__b", "c d").await;
    assert_eq!(
        wiki::page(app, e.carol, "WEB", "c_d").await.unwrap().id,
        ab.page_id
    );
}

/// R-W9 with R-W5's limit: undoing a delete in a full wiki is refused
/// (preview reason, `invalid_value`), not a 500.
#[tokio::test]
async fn v_undo_delete_at_page_limit() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Full", "x").await;
    let d = delete(&e, e.alice, p.page_id, false).await.unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE wiki_settings SET page_count = {} WHERE project_id = '{}'",
            wiki::MAX_PAGES,
            e.w.web
        ),
    )
    .await;
    let pv = undo::preview(app, e.alice, d.execution_id).await.unwrap();
    assert!(!pv.wiki[0].restorable);
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("wiki_page_limit"));
    let x = err(undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        d.execution_id,
        &UndoInput::default(),
    )
    .await);
    assert_eq!(code(&x), ErrorCode::InvalidValue);
    assert_eq!(kind(&x), "wiki_page_limit");
}

/// R-W9 with R-W3's limit: undoing an edit of a page at 10,000 versions is
/// refused (preview reason, `invalid_value`), not a 500.
#[tokio::test]
async fn v_undo_at_version_limit() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Long", "v").await;
    exec(
        &pool,
        &format!(
            "UPDATE wiki_page_versions SET version = {n} WHERE page_id = '{id}';
             UPDATE wiki_pages SET current_version = {n} WHERE id = '{id}'",
            n = wiki::MAX_VERSIONS - 1,
            id = p.page_id
        ),
    )
    .await;
    let x = edit(&e, e.alice, p.page_id, "last").await.unwrap();
    assert_eq!(x.wiki[0].version, wiki::MAX_VERSIONS);
    let pv = undo::preview(app, e.alice, x.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("wiki_version_limit"));
    let y = err(undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        x.execution_id,
        &UndoInput::default(),
    )
    .await);
    assert_eq!(code(&y), ErrorCode::InvalidValue);
    assert_eq!(kind(&y), "wiki_version_limit");
}

/// R-W9: a child goes back under a deleted page only when that page comes
/// back too; skipping the page keeps the children where they are.
#[tokio::test]
async fn v_undo_reparent_skipping_the_parent() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Parent", "p").await;
    let c1 = create_under(&e, e.alice, "C1", p.page_id).await;
    let c2 = create_under(&e, e.alice, "C2", p.page_id).await;
    let d = delete(&e, e.alice, p.page_id, true).await.unwrap();
    let input = UndoInput {
        skip_wiki_pages: vec![p.page_id],
        ..UndoInput::default()
    };
    let x = err(undo::apply(app, e.alice, Uuid::now_v7(), d.execution_id, &input).await);
    assert_eq!(code(&x), ErrorCode::UndoNotApplicable);
    let reasons = details(&x)["reasons"].clone();
    for c in [c1.page_id, c2.page_id] {
        assert_eq!(parent(&e, c).await, None);
        assert!(
            reasons
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["page_id"] == json!(c) && r["reason"] == "dependency"),
            "{reasons}"
        );
    }
    let idx = wiki::index(app, e.carol, "WEB").await.unwrap();
    assert!(idx.tree.iter().all(|t| t.id != p.page_id));
    let child = wiki::page(app, e.carol, "WEB", "C1").await.unwrap();
    assert!(child.breadcrumbs.is_empty() && child.parent.is_none());
}

/// R-W4 for undo: moving a page (or a child of a deleted page) back may not
/// nest deeper than 10 levels, its subtree included.
#[tokio::test]
async fn v_undo_respects_depth() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let mut chain = vec![create(&e, e.bob, "WEB", "D1", "d").await];
    for i in 2..=8 {
        let up = chain.last().unwrap().page_id;
        chain.push(create_under(&e, e.bob, &format!("D{i}"), up).await);
    }
    let d8 = chain[7].page_id;
    // X under D8 (depth 9), Y under X (depth 10); X moves to the top.
    let x = create_under(&e, e.alice, "X", d8).await;
    let y = create_under(&e, e.alice, "Y", x.page_id).await;
    let m = mv(&e, e.alice, x.page_id, json!({"parent": null}))
        .await
        .unwrap();
    create_under(&e, e.bob, "Z", y.page_id).await;
    let pv = undo::preview(app, e.alice, m.execution_id).await.unwrap();
    assert_eq!(pv.wiki[0].reason.as_deref(), Some("wiki_depth"));
    let r = err(undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        m.execution_id,
        &UndoInput::default(),
    )
    .await);
    assert_eq!(code(&r), ErrorCode::UndoNotApplicable);
    assert_eq!(parent(&e, x.page_id).await, None);

    // P under D8 (depth 9) with child C; deleted with reparent; C grows.
    let p = create_under(&e, e.alice, "P", d8).await;
    let c = create_under(&e, e.alice, "C", p.page_id).await;
    let d = delete(&e, e.alice, p.page_id, true).await.unwrap();
    create_under(&e, e.bob, "K", c.page_id).await;
    let pv = undo::preview(app, e.alice, d.execution_id).await.unwrap();
    let by: std::collections::BTreeMap<Uuid, &wiki::UndoWikiPage> =
        pv.wiki.iter().map(|w| (w.page_id, w)).collect();
    assert!(by[&p.page_id].restorable);
    assert_eq!(by[&c.page_id].reason.as_deref(), Some("wiki_depth"));
    let u = undo::apply(
        app,
        e.alice,
        Uuid::now_v7(),
        d.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.wiki_pages_restored, 1);
    assert_eq!(parent(&e, p.page_id).await, Some(d8));
    assert_eq!(parent(&e, c.page_id).await, Some(d8));
    let deepest = count(
        &pool,
        "WITH RECURSIVE t(id, d) AS (
             SELECT id, 1 FROM wiki_pages WHERE parent_id IS NULL AND deleted_at IS NULL
             UNION ALL
             SELECT p.id, t.d + 1 FROM wiki_pages p JOIN t ON p.parent_id = t.id
             WHERE p.deleted_at IS NULL)
         SELECT max(d)::bigint FROM t",
    )
    .await;
    assert!(deepest <= i64::from(wiki::MAX_DEPTH), "depth {deepest}");
}

/// R-W6: deleting with reparent moves the children, so a protected child
/// needs protect_wiki_pages as a direct move would.
#[tokio::test]
async fn v_reparent_delete_moves_no_protected_child_without_protect() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "Parent", "p").await;
    let c = create_under(&e, e.alice, "Guarded", p.page_id).await;
    run(
        &e,
        e.alice,
        &doc(
            op(
                "wiki_protect",
                by_id("WEB", c.page_id),
                json!({"protected": true}),
            ),
            None,
        ),
    )
    .await
    .unwrap();
    let x = err(delete(&e, e.bob, p.page_id, true).await);
    assert_eq!(code(&x), ErrorCode::ForbiddenOp);
    assert_eq!(details(&x)["reason"], "wiki_protected");
    assert_eq!(parent(&e, c.page_id).await, Some(p.page_id));
    delete(&e, e.alice, p.page_id, true).await.unwrap();
    assert_eq!(parent(&e, c.page_id).await, None);
}

/// D12 resolves a wiki page under the project lock: a page deleted while
/// the redaction waited is not redacted by its old title.
#[tokio::test]
async fn v_redaction_resolves_the_page_under_the_lock() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "Doc", "one").await;
    edit(&e, e.alice, p.page_id, "two").await.unwrap();
    let redact = {
        let app = e.w.app.clone();
        let policy = e.w.policy.clone();
        let who = e.alice;
        async move {
            redaction::redact(
                &app,
                who,
                &policy,
                Uuid::now_v7(),
                &Target::WikiVersion {
                    project: "WEB".into(),
                    page: "Doc".into(),
                    version: 1,
                },
                &RedactInput {
                    reason_code: "confidential".into(),
                    reason: "test".into(),
                    attachments: Vec::new(),
                },
            )
            .await
        }
    };
    let (d, r) = race(
        &pool,
        "wiki.after_settings",
        spawn_run(
            &e,
            e.bob,
            doc(op("wiki_delete", by_id("WEB", p.page_id), json!({})), None),
        ),
        redact,
    )
    .await;
    d.unwrap();
    let r = err(r);
    assert_eq!(code(&r), ErrorCode::NotFound);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_page_versions WHERE redaction_id IS NOT NULL"
        )
        .await,
        0
    );
}

// ---------------------------------------------------------------- R-W8 revert_from

/// R-W8: `wiki_put` with `revert_from` copies version N under the page's
/// lock; a redacted version is refused by the server (`redacted_source`),
/// as are a version the page lacks, a new page, and body with revert_from.
#[tokio::test]
async fn v_revert_from_is_checked_by_the_server() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let app = &e.w.app;
    let p = create(&e, e.alice, "WEB", "Rev", "first").await;
    edit(&e, e.alice, p.page_id, "second").await.unwrap();
    let revert = |n: i64, extra: Value| {
        let mut o = op(
            "wiki_put",
            by_id("WEB", p.page_id),
            json!({"revert_from": n}),
        );
        if let Value::Object(m) = extra {
            for (k, v) in m {
                o[k] = v;
            }
        }
        o
    };

    // Back to version 1: a new version with its body and the default comment.
    let l = lv(&e, p.page_id).await;
    let r = run(
        &e,
        e.alice,
        &doc(revert(1, json!({})), Some((p.page_id, l))),
    )
    .await
    .unwrap();
    assert!(r.wiki[0].changed);
    assert_eq!(r.wiki[0].version, 3);
    let v3 = wiki::version(app, e.alice, "WEB", "Rev", 3).await.unwrap();
    assert_eq!(v3.body, "first");
    assert_eq!(v3.comment, "回復到第 1 版");

    // Shape errors: both, neither, a version the page lacks, a new page.
    let l = lv(&e, p.page_id).await;
    let both = err(run(
        &e,
        e.alice,
        &doc(revert(1, json!({"body": "x"})), Some((p.page_id, l))),
    )
    .await);
    assert_eq!(code(&both), ErrorCode::InvalidRequest);
    let neither = err(run(
        &e,
        e.alice,
        &doc(
            op("wiki_put", by_id("WEB", p.page_id), json!({})),
            Some((p.page_id, l)),
        ),
    )
    .await);
    assert_eq!(code(&neither), ErrorCode::InvalidRequest);
    let missing = err(run(
        &e,
        e.alice,
        &doc(revert(99, json!({})), Some((p.page_id, l))),
    )
    .await);
    assert_eq!(code(&missing), ErrorCode::InvalidArgument);
    assert_eq!(details(&missing)["path"], "ops[0].revert_from");
    let fresh = err(run(
        &e,
        e.alice,
        &doc(
            op("wiki_put", at("WEB", "Not_Yet"), json!({"revert_from": 1})),
            None,
        ),
    )
    .await);
    assert_eq!(code(&fresh), ErrorCode::InvalidArgument);

    // Version 1 redacted: the server refuses it as a source, writes nothing.
    redaction::redact(
        app,
        e.alice,
        &e.w.policy,
        Uuid::now_v7(),
        &Target::WikiVersion {
            project: "WEB".into(),
            page: "Rev".into(),
            version: 1,
        },
        &RedactInput {
            reason_code: "confidential".into(),
            reason: "a token".into(),
            attachments: Vec::new(),
        },
    )
    .await
    .unwrap();
    let l = lv(&e, p.page_id).await;
    let x = err(run(
        &e,
        e.alice,
        &doc(revert(1, json!({})), Some((p.page_id, l))),
    )
    .await);
    assert_eq!(code(&x), ErrorCode::InvalidValue);
    assert_eq!(details(&x)["kind"], "redacted_source");
    assert_eq!(lv(&e, p.page_id).await, l, "nothing was written");
    assert_eq!(
        wiki::page(app, e.alice, "WEB", "Rev")
            .await
            .unwrap()
            .version,
        3
    );
    // An unredacted version is still a source.
    let r = run(
        &e,
        e.alice,
        &doc(revert(2, json!({})), Some((p.page_id, l))),
    )
    .await
    .unwrap();
    assert_eq!(r.wiki[0].version, 4);
}

// ---------------------------------------------------------------- R-W8 revert_from × D12

fn rv(page: Uuid, n: Value) -> Value {
    op("wiki_put", by_id("WEB", page), json!({"revert_from": n}))
}

fn spawn_redact(
    e: &Env,
    title: &'static str,
    v: i32,
) -> impl Future<Output = anyhow::Result<redaction::Redacted>> + Send + 'static {
    let app = e.w.app.clone();
    let policy = e.w.policy.clone();
    let who = e.alice;
    async move {
        redaction::redact(
            &app,
            who,
            &policy,
            Uuid::now_v7(),
            &Target::WikiVersion {
                project: "WEB".into(),
                page: title.into(),
                version: v,
            },
            &RedactInput {
                reason_code: "confidential".into(),
                reason: "probe".into(),
                attachments: Vec::new(),
            },
        )
        .await
    }
}

/// Redaction holds its locks (before_derived) when the revert arrives: the
/// revert must wait and then see the redacted source.
#[tokio::test]
async fn v_revert_waits_for_a_redaction_and_sees_it() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "RaceA", "SECRET_ONE").await;
    edit(&e, e.alice, p.page_id, "two").await.unwrap();
    let l = lv(&e, p.page_id).await;
    let (red, rev) = race(
        &pool,
        "redact.before_derived",
        spawn_redact(&e, "RaceA", 1),
        spawn_run(
            &e,
            e.bob,
            doc(rv(p.page_id, json!(1)), Some((p.page_id, l))),
        ),
    )
    .await;
    red.unwrap();
    let x = err(rev);
    assert_eq!(code(&x), ErrorCode::InvalidValue, "{:?}", shape(&x));
    assert_eq!(details(&x)["kind"], "redacted_source");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM wiki_page_versions WHERE body LIKE '%SECRET_ONE%'"
        )
        .await,
        0,
        "the redacted text was copied"
    );
}

/// The revert holds its locks (before_write, source already read) when the
/// redaction arrives: the redaction waits; the copy happened before it.
#[tokio::test]
async fn v_redaction_after_a_revert_is_serial() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let p = create(&e, e.alice, "WEB", "RaceB", "SECRET_ONE").await;
    edit(&e, e.alice, p.page_id, "two").await.unwrap();
    let l = lv(&e, p.page_id).await;
    let (rev, red) = race(
        &pool,
        "wiki.before_write",
        spawn_run(
            &e,
            e.bob,
            doc(rv(p.page_id, json!(1)), Some((p.page_id, l))),
        ),
        spawn_redact(&e, "RaceB", 1),
    )
    .await;
    let r = rev.unwrap();
    assert_eq!(r.wiki[0].version, 3);
    assert!(red.unwrap().changed);
    let v1: bool = sqlx::query_scalar(
        "SELECT redaction_id IS NOT NULL FROM wiki_page_versions WHERE page_id=$1 AND version=1",
    )
    .bind(p.page_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(v1);
    let b3: String =
        sqlx::query_scalar("SELECT body FROM wiki_page_versions WHERE page_id=$1 AND version=3")
            .bind(p.page_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(b3, "SECRET_ONE", "serial order revert -> redaction");
}

/// R-W8 × R-W7: an editor without view_wiki_edits cannot revert (the server
/// would read an earlier version for them); an edit by body still works
/// (使用者決定 2026-11-16).
#[tokio::test]
async fn v_revert_needs_view_wiki_edits() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let r = role(
        &pool,
        "WikiEditNoHistory",
        &["view_wiki_pages", "edit_wiki_pages"],
    )
    .await;
    let frank = auth::create_user(&pool, "frank", "frank", PW, false)
        .await
        .unwrap();
    member(&pool, e.w.web, frank, r).await;
    let frank = sign_in(&pool, &e.w.policy, "frank").await;
    let p = create(&e, e.alice, "WEB", "Old", "OLD_SECRET").await;
    edit(&e, e.alice, p.page_id, "clean").await.unwrap();
    let l = lv(&e, p.page_id).await;
    for n in [1, 99] {
        let x = err(run(
            &e,
            frank,
            &doc(
                op(
                    "wiki_put",
                    by_id("WEB", p.page_id),
                    json!({"revert_from": n}),
                ),
                Some((p.page_id, l)),
            ),
        )
        .await);
        assert_eq!(code(&x), ErrorCode::ForbiddenOp, "revert_from {n}");
        assert_eq!(details(&x)["reason"], "permission");
    }
    assert_eq!(lv(&e, p.page_id).await, l, "nothing was written");
    assert_eq!(
        wiki::page(&e.w.app, frank, "WEB", "Old")
            .await
            .unwrap()
            .body,
        "clean"
    );
    edit(&e, frank, p.page_id, "frank's").await.unwrap();
}
