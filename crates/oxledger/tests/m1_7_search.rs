// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.7 (§2.9, §3.9, §6.13): BM25F over keys, public notes and
//! visible text custom fields, alias expansion through concepts, concept
//! administration, and the blue-green upgrade of a pre-M1.7 index.
//! T-M1-62 (private notes, restricted fields), T-M1-63 (public notes with a
//! note citation), T-M1-64 (expansion and `match.expanded`), T-M1-65
//! (`concept_label_taken`, R18 normalization), plus leak checks for people
//! and agents, the HTTP routes, and search during the upgrade switch.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::citations::{self, CitationStatus};
use oxledger::concepts::{self, ConceptPatch, ConceptQuery, LabelPatch, NewConcept, NewLabel};
use oxledger::error::{ErrorCode, classify};
use oxledger::search::{Layout, SearchIndex};
use oxledger::service::{self, App, SearchParams, SearchResult};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

async fn search_as(app: &App, who: AuthContext, q: &str) -> SearchResult {
    service::search(
        app,
        who,
        SearchParams {
            q: Some(q.into()),
            explain: Some("full".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn keys_as(app: &App, who: AuthContext, q: &str) -> Vec<String> {
    let mut k: Vec<String> = search_as(app, who, q)
        .await
        .items
        .into_iter()
        .map(|i| i.key)
        .collect();
    k.sort();
    k
}

async fn key_of(pool: &sqlx::PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn note(w: &World, i: Uuid, body: &str, private: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO issue_notes (issue_id, author_id, body, is_private) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(i)
    .bind(w.bob)
    .bind(body)
    .bind(private)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

/// A text custom field for the Bug tracker in every project.
async fn text_field(w: &World, name: &str, visible: bool) -> i32 {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ($1, 'text', true, $2) RETURNING id",
    )
    .bind(name)
    .bind(visible)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO field_trackers (field_id, tracker_id) VALUES ($1, $2)")
        .bind(id)
        .bind(w.bug)
        .execute(&w.pool)
        .await
        .unwrap();
    id
}

async fn set_cf(w: &World, i: Uuid, f: i32, v: &str) {
    sqlx::query("INSERT INTO issue_cf (issue_id, field_id, v_text) VALUES ($1, $2, $3)")
        .bind(i)
        .bind(f)
        .bind(v)
        .execute(&w.pool)
        .await
        .unwrap();
}

async fn admin_ctx(w: &World) -> AuthContext {
    auth::create_user(&w.pool, "root", "root", PW, true)
        .await
        .unwrap();
    sign_in(&w.pool, &w.policy, "root").await
}

fn label(l: &str) -> NewLabel {
    NewLabel {
        label: l.into(),
        kind: None,
        status: None,
    }
}

fn err_code(e: &anyhow::Error) -> ErrorCode {
    classify(e).unwrap_or_else(|| panic!("{e:#}")).code
}

fn details(e: &anyhow::Error) -> Value {
    classify(e)
        .unwrap_or_else(|| panic!("{e:#}"))
        .details
        .clone()
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------- T-M1-62, 63

/// T-M1-62: private notes and restricted (visible = false) fields never
/// match, also for people who may read them; a field hidden after the
/// index saw it stops matching before the index syncs (SQL re-check).
#[tokio::test]
async fn t_m1_62_private_notes_and_restricted_fields_do_not_match() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.web, "plain subject one").await;
    let b = w.issue(w.web, "plain subject two").await;
    let c = w.issue(w.web, "plain subject three").await;
    note(&w, a, "wombat secret plan", true).await;
    let hidden = text_field(&w, "Internal", false).await;
    set_cf(&w, b, hidden, "quokka internal").await;
    let shown = text_field(&w, "Customer", true).await;
    set_cf(&w, c, shown, "echidna customer ref").await;
    w.app.index.sync_all(&pool).await.unwrap();
    // alice holds view_private_notes; the note still is not searchable.
    assert!(keys_as(&w.app, w.alice_ctx, "wombat").await.is_empty());
    assert!(keys_as(&w.app, w.alice_ctx, "quokka").await.is_empty());
    let r = search_as(&w.app, w.alice_ctx, "echidna").await;
    assert_eq!(r.items.len(), 1);
    let m = r.items[0].matched.as_ref().unwrap();
    assert_eq!(m.fields, vec!["cf_text"]);
    assert!(
        r.items[0].evidence.is_empty(),
        "no snippet from a custom field"
    );
    // Hidden now; the index has not seen it yet.
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = false WHERE id = {shown}"),
    )
    .await;
    assert!(
        keys_as(&w.app, w.alice_ctx, "echidna").await.is_empty(),
        "dropped by the SQL re-check before sync"
    );
    w.app.index.sync_all(&pool).await.unwrap();
    assert!(keys_as(&w.app, w.alice_ctx, "echidna").await.is_empty());
    // The field no longer applies to the tracker: same.
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = true WHERE id = {shown}"),
    )
    .await;
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(keys_as(&w.app, w.alice_ctx, "echidna").await.len(), 1);
    exec(
        &pool,
        &format!("DELETE FROM field_trackers WHERE field_id = {shown}"),
    )
    .await;
    assert!(keys_as(&w.app, w.alice_ctx, "echidna").await.is_empty());
}

/// T-M1-63: a public note matches with a note citation that verifies;
/// made private or redacted, it stops matching before the index syncs.
#[tokio::test]
async fn t_m1_63_public_notes_match_with_a_note_citation() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.web, "unrelated subject").await;
    let events = format!("SELECT count(*) FROM outbox WHERE topic = 'issue' AND entity_id = '{a}'");
    let before = count(&pool, &events).await;
    let n = note(&w, a, "the platypus import fails on Mondays", false).await;
    assert_eq!(
        count(&pool, &events).await,
        before + 1,
        "a note enqueues its issue"
    );
    w.app.index.sync_all(&pool).await.unwrap();
    let r = search_as(&w.app, w.alice_ctx, "platypus").await;
    assert_eq!(r.items.len(), 1);
    let it = &r.items[0];
    assert_eq!(it.matched.as_ref().unwrap().fields, vec!["notes"]);
    let ev = &it.evidence[0];
    assert_eq!(ev.field, "note");
    assert_eq!(ev.note_id, Some(n));
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        citations::verify_one(&mut conn, w.alice_ctx, &ev.citation_id, &ev.text)
            .await
            .unwrap(),
        CitationStatus::Valid
    );
    drop(conn);
    // carol is in WEB too but the note is public: she finds it as well.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    assert_eq!(keys_as(&w.app, carol, "platypus").await.len(), 1);

    exec(
        &pool,
        &format!("UPDATE issue_notes SET is_private = true WHERE id = '{n}'"),
    )
    .await;
    assert!(keys_as(&w.app, w.alice_ctx, "platypus").await.is_empty());
    exec(
        &pool,
        &format!("UPDATE issue_notes SET is_private = false, redacted_at = now() WHERE id = '{n}'"),
    )
    .await;
    assert!(keys_as(&w.app, w.alice_ctx, "platypus").await.is_empty());
    w.app.index.sync_all(&pool).await.unwrap();
    assert!(keys_as(&w.app, w.alice_ctx, "platypus").await.is_empty());
}

/// Keys match as a whole (×5): the current key and a previous one.
#[tokio::test]
async fn keys_match_whole() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.web, "anything").await;
    let _ = w.issue(w.web, "other").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let k = key_of(&pool, a).await;
    let r = search_as(&w.app, w.alice_ctx, &k.to_lowercase()).await;
    assert_eq!(r.items[0].key, k);
    assert_eq!(r.items[0].rank, Some(1));
    assert!(
        r.items[0]
            .matched
            .as_ref()
            .unwrap()
            .fields
            .contains(&"key".to_string())
    );
}

// ---------------------------------------------------------------- T-M1-64

/// T-M1-64: 登入 expands to SSO through a concept and the hit says so;
/// a staging label or a retired concept does not expand.
#[tokio::test]
async fn t_m1_64_alias_expansion() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let a = w.issue(w.web, "SSO redirect loops").await;
    let b = w.issue(w.web, "登入頁面錯字").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let c = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "登入".into(),
            labels: vec![label("SSO"), label("login")],
        },
    )
    .await
    .unwrap();
    let r = search_as(&w.app, w.alice_ctx, "登入").await;
    let (ka, kb) = (key_of(&pool, a).await, key_of(&pool, b).await);
    let got: Vec<&str> = r.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(got[0], kb, "the literal match ranks first");
    assert!(got.contains(&ka.as_str()), "SSO found through 登入");
    let ia = r.items.iter().find(|i| i.key == ka).unwrap();
    let m = ia.matched.as_ref().unwrap();
    assert_eq!(m.expanded.len(), 1);
    assert_eq!(m.expanded[0].from, "登入");
    assert_eq!(m.expanded[0].via, "登入");
    assert_eq!(m.expanded[0].label, "SSO");
    let ib = r.items.iter().find(|i| i.key == kb).unwrap();
    assert!(ib.matched.as_ref().unwrap().expanded.is_empty());
    // Reverse direction, case-insensitive.
    let r = search_as(&w.app, w.alice_ctx, "sso").await;
    assert_eq!(r.items.len(), 2);

    // Staging: no longer expanded.
    concepts::patch_label(
        &w.app,
        root,
        &c.id.to_string(),
        "sso",
        &LabelPatch {
            status: Some("staging".into()),
            kind: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(keys_as(&w.app, w.alice_ctx, "登入").await, vec![kb.clone()]);
    // A staging label does not trigger an expansion either (D5): "sso"
    // finds only its literal match, with no expanded.
    let r = search_as(&w.app, w.alice_ctx, "sso").await;
    let got: Vec<&str> = r.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(got, vec![ka.as_str()]);
    assert!(r.items[0].matched.as_ref().unwrap().expanded.is_empty());
    concepts::patch_label(
        &w.app,
        root,
        &c.id.to_string(),
        "SSO",
        &LabelPatch {
            status: Some("active".into()),
            kind: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(keys_as(&w.app, w.alice_ctx, "登入").await.len(), 2);
    // Retired concept: no expansion.
    let lv = concepts::get(&w.app, root, &c.id.to_string())
        .await
        .unwrap()
        .lock_version;
    concepts::patch(
        &w.app,
        root,
        &c.id.to_string(),
        &ConceptPatch {
            lock_version: lv,
            pref_label: None,
            status: Some("retired".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(keys_as(&w.app, w.alice_ctx, "登入").await, vec![kb]);
}

/// Expansion never reveals what the caller cannot see: carol (WEB only)
/// gets nothing from AUTH through a label, and non-administrators see
/// neither staging labels nor retired concepts; they cannot manage them.
#[tokio::test]
async fn expansion_and_concepts_do_not_leak() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let secret = w.issue(w.auth_p, "kerberos ticket expired").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let c = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "auth".into(),
            labels: vec![
                label("kerberos"),
                NewLabel {
                    label: "internal-codename".into(),
                    kind: Some("hidden".into()),
                    status: Some("staging".into()),
                },
            ],
        },
    )
    .await
    .unwrap();
    // alice sees AUTH: found through the concept.
    assert!(
        keys_as(&w.app, w.alice_ctx, "auth")
            .await
            .contains(&key_of(&pool, secret).await)
    );
    // carol (WEB only): not the AUTH issue, by any route.
    for q in ["auth", "kerberos", "kerberos auth"] {
        let r = search_as(&w.app, carol, q).await;
        assert!(r.items.is_empty(), "{q}: {:?}", r.items);
    }
    // The agent with WEB delegated: same.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    for q in ["auth", "kerberos"] {
        let keys = keys_as(&w.app, agent, q).await;
        assert!(!keys.contains(&key_of(&pool, secret).await), "{q}");
    }
    // Concepts: non-administrators see active labels of active concepts.
    let v = concepts::get(&w.app, carol, &c.id.to_string())
        .await
        .unwrap();
    let ls: Vec<&str> = v.labels.iter().map(|l| l.label.as_str()).collect();
    assert_eq!(ls, vec!["auth", "kerberos"]);
    let v = concepts::get(&w.app, root, &c.id.to_string())
        .await
        .unwrap();
    assert_eq!(v.labels.len(), 3);
    let e = concepts::create(
        &w.app,
        carol,
        &NewConcept {
            pref_label: "x".into(),
            labels: vec![],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::ForbiddenOp);
    let e = concepts::add_label(&w.app, agent, &c.id.to_string(), &label("y"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err_code(&e),
            ErrorCode::ForbiddenOp | ErrorCode::HumanSessionRequired
        ),
        "{e:#}"
    );
    concepts::patch(
        &w.app,
        root,
        &c.id.to_string(),
        &ConceptPatch {
            lock_version: v.lock_version,
            pref_label: None,
            status: Some("retired".into()),
        },
    )
    .await
    .unwrap();
    let e = concepts::get(&w.app, carol, &c.id.to_string())
        .await
        .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::NotFound);
    let l = concepts::list(&w.app, carol, &ConceptQuery::default())
        .await
        .unwrap();
    assert!(l.items.is_empty());
    let l = concepts::list(&w.app, root, &ConceptQuery::default())
        .await
        .unwrap();
    assert_eq!(l.items.len(), 1);
}

// ---------------------------------------------------------------- T-M1-65

/// T-M1-65: an active label belongs to one concept, compared after NFC +
/// casefold (R18): `concept_label_taken` names the holder.
#[tokio::test]
async fn t_m1_65_label_taken() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let c1 = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "Café".into(),
            labels: vec![label("Ünïcode")],
        },
    )
    .await
    .unwrap();
    let c2 = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "other".into(),
            labels: vec![],
        },
    )
    .await
    .unwrap();
    // Case, NFD and surrounding spaces fold to the same label (the
    // casefold of the database's collation, as everywhere else).
    for l in ["CAFÉ", "cafe\u{301}", "U\u{308}NI\u{308}CODE", "  ünïcode "] {
        let e = concepts::add_label(&w.app, root, &c2.id.to_string(), &label(l))
            .await
            .unwrap_err();
        assert_eq!(err_code(&e), ErrorCode::ConceptLabelTaken, "{l}");
        let d = details(&e);
        assert_eq!(d["concept_id"], json!(c1.id), "{l}");
        assert_eq!(d["pref_label"], json!("Café"), "{l}");
    }
    let e = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "café".into(),
            labels: vec![],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::ConceptLabelTaken);
    // A staging label may duplicate; activating it is refused.
    let v = concepts::add_label(
        &w.app,
        root,
        &c2.id.to_string(),
        &NewLabel {
            label: "ÜNÏCODE".into(),
            kind: None,
            status: Some("staging".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(v.labels.len(), 2);
    let e = concepts::patch_label(
        &w.app,
        root,
        &c2.id.to_string(),
        "ünïcode",
        &LabelPatch {
            status: Some("active".into()),
            kind: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::ConceptLabelTaken);
    // A retired concept still holds its labels.
    let lv = concepts::get(&w.app, root, &c1.id.to_string())
        .await
        .unwrap()
        .lock_version;
    concepts::patch(
        &w.app,
        root,
        &c1.id.to_string(),
        &ConceptPatch {
            lock_version: lv,
            pref_label: None,
            status: Some("retired".into()),
        },
    )
    .await
    .unwrap();
    let e = concepts::add_label(&w.app, root, &c2.id.to_string(), &label("café"))
        .await
        .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::ConceptLabelTaken);
    // The unique index is the last word, whatever the application checks.
    let e = sqlx::query(
        "INSERT INTO aliases (entity_kind, entity_id, label, label_kind) VALUES ('concept', $1, 'CAFE\u{301}', 'alt')",
    )
    .bind(c2.id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(format!("{e}").contains("aliases_concept_label_uq"), "{e}");
}

/// D1: a concept whose pref row is missing (data from before 0027's
/// repair) still lists its name, and renaming it creates the pref row, so
/// the new name is a label that expands and is unique.
#[tokio::test]
async fn concept_without_pref_row_lists_and_renames() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO concepts (pref_label, created_by) VALUES ('LOGIN', $1) RETURNING id",
    )
    .bind(w.alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    for (l, k, st) in [
        ("LOGIN", "hidden", "staging"),
        ("sso", "alt", "staging"),
        ("signon", "alt", "active"),
    ] {
        sqlx::query(
            "INSERT INTO aliases (entity_kind, entity_id, label, label_kind, status) VALUES ('concept', $1, $2, $3, $4)",
        )
        .bind(id)
        .bind(l)
        .bind(k)
        .bind(st)
        .execute(&pool)
        .await
        .unwrap();
    }
    let v = concepts::get(&w.app, carol, &id.to_string()).await.unwrap();
    assert!(
        v.labels
            .iter()
            .any(|l| l.label == v.pref_label && l.kind == "pref"),
        "{v:?}"
    );
    assert!(
        v.labels
            .iter()
            .all(|l| l.status == "active" && l.kind != "hidden"),
        "{v:?}"
    );
    let v = concepts::patch(
        &w.app,
        root,
        &id.to_string(),
        &ConceptPatch {
            lock_version: v.lock_version,
            pref_label: Some("Sign-in".into()),
            status: None,
        },
    )
    .await
    .unwrap();
    assert!(
        v.labels
            .iter()
            .any(|l| l.kind == "pref" && l.label == "Sign-in"),
        "{v:?}"
    );
    let rows = count(
        &pool,
        &format!(
            "SELECT count(*) FROM aliases WHERE entity_id = '{id}' AND label_kind = 'pref' AND status = 'active' AND label = 'Sign-in'"
        ),
    )
    .await;
    assert_eq!(rows, 1);
    // The new name expands and holds its place.
    let a = w.issue(w.web, "signon broken").await;
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(
        keys_as(&w.app, w.alice_ctx, "sign-in").await,
        vec![key_of(&pool, a).await]
    );
    let e = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "SIGN-IN".into(),
            labels: vec![],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::ConceptLabelTaken);
}

/// D6: hidden labels (misspellings, internal terms) are listed to
/// administrators only, and `match.expanded` shows the concept's name in
/// their place.
#[tokio::test]
async fn hidden_labels_stay_hidden() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let a = w.issue(w.web, "lgoin typo on the page").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let c = concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "登入".into(),
            labels: vec![
                label("login"),
                NewLabel {
                    label: "lgoin".into(),
                    kind: Some("hidden".into()),
                    status: None,
                },
            ],
        },
    )
    .await
    .unwrap();
    let r = search_as(&w.app, carol, "登入").await;
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].key, key_of(&pool, a).await);
    let ex = &r.items[0].matched.as_ref().unwrap().expanded;
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].label, "登入", "the name, not the hidden label");
    assert_eq!(ex[0].via, "登入");
    assert!(!serde_json::to_string(&r).unwrap().contains("lgoin\""));
    let v = concepts::get(&w.app, carol, &c.id.to_string())
        .await
        .unwrap();
    assert!(v.labels.iter().all(|l| l.label != "lgoin"), "{v:?}");
    let l = concepts::list(
        &w.app,
        carol,
        &ConceptQuery {
            q: Some("lgoin".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(l.items.is_empty(), "no search by a hidden label");
    let v = concepts::get(&w.app, root, &c.id.to_string())
        .await
        .unwrap();
    assert!(
        v.labels
            .iter()
            .any(|l| l.label == "lgoin" && l.kind == "hidden")
    );
}

/// D3: on a legacy generation the status pre-filter matches all, and so
/// does a negation over it: every form keeps the matching issue.
#[tokio::test]
async fn legacy_index_ignores_status_prefilter() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.web, "dingo subject").await;
    let index = SearchIndex::open(None).unwrap();
    index.set_first_build_legacy(true);
    let app = App {
        pool: pool.clone(),
        index,
    };
    app.index.sync_all(&pool).await.unwrap();
    assert_eq!(app.index.view().unwrap().layout(), Layout::Legacy);
    let ka = key_of(&pool, a).await;
    for cond in [
        json!({"field": "status", "op": "ne", "value": w.done}),
        json!({"field": "status", "op": "not_in", "value": [w.done, w.doing]}),
        json!({"field": "status", "op": "eq", "value": w.new}),
        json!({"field": "status", "op": "in", "value": [w.new, w.done]}),
    ] {
        let r = service::search(
            &app,
            w.alice_ctx,
            SearchParams {
                q: Some("dingo".into()),
                where_: Some(cond.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let keys: Vec<&str> = r.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec![ka.as_str()], "{cond}");
    }
}

/// D4: a schema-2 generation on disk is opened as legacy (not rebuilt),
/// serves text search, and asks for the upgrade.
#[tokio::test]
async fn legacy_generation_on_disk_is_opened() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.issue(w.web, "numbat on disk").await;
    let dir = std::env::temp_dir().join(format!("oxledger-m17-{}", Uuid::now_v7()));
    let g = {
        let idx = SearchIndex::open(Some(&dir)).unwrap();
        idx.set_first_build_legacy(true);
        idx.sync_all(&pool).await.unwrap();
        assert_eq!(idx.view().unwrap().layout(), Layout::Legacy);
        idx.generation().unwrap()
    };
    let idx = SearchIndex::open(Some(&dir)).unwrap();
    idx.sync_round(&pool).await.unwrap();
    assert_eq!(
        idx.generation(),
        Some(g),
        "the same generation, not rebuilt"
    );
    assert!(idx.needs_upgrade());
    assert_eq!(idx.view().unwrap().layout(), Layout::Legacy);
    let app = App {
        pool: pool.clone(),
        index: idx,
    };
    assert_eq!(
        keys_as(&app, w.alice_ctx, "numbat").await,
        vec![key_of(&pool, a).await]
    );
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- upgrade

/// A pre-M1.7 index (schema 2) keeps serving with its old semantics (no
/// notes, no expansion) while the rebuild runs; search never stops, and
/// after the switch the new semantics apply.
#[tokio::test]
async fn legacy_index_serves_until_the_upgrade_switches() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let root = admin_ctx(&w).await;
    let a = w.issue(w.web, "dingo subject").await;
    let b = w.issue(w.web, "unrelated").await;
    note(&w, b, "dingo in a note", false).await;
    concepts::create(
        &w.app,
        root,
        &NewConcept {
            pref_label: "dingo".into(),
            labels: vec![label("wild dog")],
        },
    )
    .await
    .unwrap();
    let c = w.issue(w.web, "wild dog sighting").await;
    let index = SearchIndex::open(None).unwrap();
    index.set_first_build_legacy(true);
    let app = Arc::new(App {
        pool: pool.clone(),
        index,
    });
    app.index.sync_all(&pool).await.unwrap();
    assert!(app.index.needs_upgrade());
    assert_eq!(app.index.view().unwrap().layout(), Layout::Legacy);
    let (ka, kb, kc) = (
        key_of(&pool, a).await,
        key_of(&pool, b).await,
        key_of(&pool, c).await,
    );
    assert_eq!(keys_as(&app, w.alice_ctx, "dingo").await, vec![ka.clone()]);

    let gate = Arc::new(Gate::default());
    faults::arm("search.before_switch", Action::Block(gate.clone()));
    assert!(app.index.claim_upgrade());
    assert!(!app.index.claim_upgrade(), "claimed once");
    let a2 = app.clone();
    let p2 = pool.clone();
    let job = tokio::spawn(async move { a2.index.rebuild(&p2).await });
    gate.reached.notified().await;
    // Mid-rebuild: still the legacy semantics, and writes keep syncing.
    let d = w.issue(w.web, "dingo during the rebuild").await;
    app.index.sync_all(&pool).await.unwrap();
    let kd = key_of(&pool, d).await;
    let mut want = vec![ka.clone(), kd.clone()];
    want.sort();
    assert_eq!(keys_as(&app, w.alice_ctx, "dingo").await, want);
    let r = search_as(&app, w.alice_ctx, "dingo").await;
    assert!(
        r.items
            .iter()
            .all(|i| i.matched.as_ref().unwrap().expanded.is_empty())
    );
    gate.release.notify_one();
    common::join("the released rebuild", job).await.unwrap();
    faults::clear();
    assert!(!app.index.needs_upgrade());
    assert_eq!(app.index.view().unwrap().layout(), Layout::Current);
    let mut want = vec![ka, kb, kc, kd];
    want.sort();
    assert_eq!(keys_as(&app, w.alice_ctx, "dingo").await, want);
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "http://localhost:5173";

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    )
}

struct Browser {
    session: String,
    csrf: String,
}

fn cookie_value(resp: &axum::response::Response, name: &str) -> String {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{name}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap()
}

async fn login(r: &Router, login: &str) -> Browser {
    let lc = r
        .clone()
        .oneshot(
            Request::get("/api/v1/auth/login-context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = cookie_value(&lc, LOGIN_COOKIE);
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::ORIGIN, ORIGIN_SPA)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={cookie}"))
                .header("x-csrf-token", &token)
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = cookie_value(&resp, SESSION_COOKIE);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    Browser { session, csrf }
}

async fn call(
    r: &Router,
    b: Option<&Browser>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header(header::ORIGIN, ORIGIN_SPA);
    if let Some(b) = b {
        req = req
            .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
            .header("x-csrf-token", &b.csrf);
    }
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The concept routes and an expanded search over HTTP.
#[tokio::test]
async fn m1_7_http() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let a = w.issue(w.web, "SSO redirect loops").await;
    let secret = w.issue(w.auth_p, "SSO token leak").await;
    w.app.index.sync_all(&pool).await.unwrap();
    let r = router(&w);
    let (s, _) = call(&r, None, "GET", "/admin/concepts", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let root = login(&r, "root").await;
    let carol = login(&r, "carol").await;
    let (s, v) = call(
        &r,
        Some(&carol),
        "POST",
        "/admin/concepts",
        Some(json!({"pref_label": "登入"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "forbidden_op");
    let (s, v) = call(
        &r,
        Some(&root),
        "POST",
        "/admin/concepts",
        Some(json!({"pref_label": "登入", "labels": [{"label": "SSO"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = call(
        &r,
        Some(&root),
        "POST",
        &format!("/admin/concepts/{id}/labels"),
        Some(json!({"label": "sso"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["details"]["kind"], "label_exists");
    let (s, v) = call(
        &r,
        Some(&root),
        "POST",
        "/admin/concepts",
        Some(json!({"pref_label": "sso"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["code"], "concept_label_taken");
    assert_eq!(v["error"]["details"]["concept_id"], json!(id));
    let (s, v) = call(
        &r,
        Some(&root),
        "POST",
        &format!("/admin/concepts/{id}/labels"),
        Some(json!({"label": "login", "kind": "hidden"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = call(
        &r,
        Some(&root),
        "PATCH",
        &format!("/admin/concepts/{id}/labels/login"),
        Some(json!({"status": "staging"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = call(
        &r,
        Some(&carol),
        "GET",
        &format!("/admin/concepts/{id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["labels"].as_array().unwrap().len(), 2);
    let (s, v) = call(&r, Some(&carol), "GET", "/admin/concepts?q=sso", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 1);
    // carol searches 登入: the WEB issue through SSO, never the AUTH one.
    let (s, v) = call(
        &r,
        Some(&carol),
        "POST",
        "/issues/search",
        Some(json!({"q": "登入"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["key"], json!(key_of(&pool, a).await));
    assert_eq!(
        items[0]["match"]["expanded"],
        json!([{"from": "登入", "via": "登入", "label": "SSO"}])
    );
    assert!(!v.to_string().contains(&key_of(&pool, secret).await));
}
