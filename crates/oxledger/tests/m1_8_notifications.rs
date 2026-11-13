// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.8 (§2.10, §3.11, A2): watchers and the in-app notification
//! centre. T-M1-70…83, T-M1-125, T-M1-126, T-M1-132, the CHECK lists
//! against the code constants, the HTTP routes, and (ignored) the dispatch
//! latency at 10 000 issues / 50 people.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, SubmitResult};
use oxledger::commands;
use oxledger::error::{ErrorCode, classify};
use oxledger::mcp::OxServer;
use oxledger::notifications::{
    self, ListQuery, NotificationItem, NotificationList, PreferencesPut, ReadAllInput,
};
use oxledger::service;
use oxledger::triage::{self, TriageSettingsPut};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

struct F {
    w: World,
    /// Role Viewer in WEB (view_issues only, issues_visibility default): no
    /// private issues, no private notes, no restricted fields.
    dave: Uuid,
    bob_ctx: AuthContext,
    carol_ctx: AuthContext,
    dave_ctx: AuthContext,
}

async fn setup(pool: &sqlx::PgPool) -> F {
    let w = World::build(pool).await;
    let viewer = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(pool, viewer, &["view_issues"]).await;
    let dave = auth::create_user(pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(pool, w.web, dave, viewer).await;
    let bob_ctx = sign_in(pool, &w.policy, "bob").await;
    let carol_ctx = sign_in(pool, &w.policy, "carol").await;
    let dave_ctx = sign_in(pool, &w.policy, "dave").await;
    F {
        w,
        dave,
        bob_ctx,
        carol_ctx,
        dave_ctx,
    }
}

fn doc(ops: Vec<Value>, pre: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "m1.8", "ops": ops,
           "preconditions": pre.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// Creates an issue through the command path (so it has a `created` event).
async fn create(w: &World, who: AuthContext, project: Uuid, fields: Value) -> (Uuid, String) {
    let r = commands::submit(
        &w.app,
        who,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                        "tracker_id": w.bug, "fields": fields}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    // Dispatch the `created` event now, so a watch added next starts after it.
    dispatch(w).await;
    (r.effects[0].issue_id, r.effects[0].key.clone())
}

/// Runs `ops` on issue `i` as `who`.
async fn run(w: &World, who: AuthContext, i: Uuid, ops: Vec<Value>) {
    commands::submit(
        &w.app,
        who,
        Uuid::now_v7(),
        &doc(ops, vec![(i, lv(w, i).await)]),
    )
    .await
    .unwrap();
}

fn set(i: Uuid, v: Value) -> Value {
    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": v})
}

fn note(i: Uuid, body: &str, private: bool) -> Value {
    json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body, "is_private": private})
}

async fn subject(w: &World, who: AuthContext, i: Uuid, s: &str) {
    run(w, who, i, vec![set(i, json!({"subject": s}))]).await;
}

async fn dispatch(w: &World) -> notifications::Round {
    notifications::dispatch_all(&w.pool).await.unwrap()
}

async fn inbox(w: &World, who: AuthContext) -> NotificationList {
    notifications::list(&w.app, who, &ListQuery::default())
        .await
        .unwrap()
}

async fn inbox_all(w: &World, who: AuthContext) -> NotificationList {
    notifications::list(
        &w.app,
        who,
        &ListQuery {
            filter: Some("all".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

/// The item of issue `key` in the unread list, if any.
async fn item(w: &World, who: AuthContext, key: &str) -> Option<NotificationItem> {
    inbox(w, who)
        .await
        .items
        .into_iter()
        .find(|i| i.issue.as_ref().is_some_and(|x| x.key == key))
}

async fn unread(w: &World, who: AuthContext) -> Option<u32> {
    notifications::unread_count(&w.app, who)
        .await
        .unwrap()
        .count
}

/// Rows stored for `user` (whatever they may read).
async fn rows(w: &World, user: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notifications WHERE recipient_id = $1")
        .bind(user)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn rows_for(w: &World, user: Uuid, issue: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE recipient_id = $1 AND issue_id = $2",
    )
    .bind(user)
    .bind(issue)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn watchers(w: &World, issue: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query_as(
        "SELECT user_id, source FROM issue_watchers WHERE issue_id = $1 ORDER BY user_id",
    )
    .bind(issue)
    .fetch_all(&w.pool)
    .await
    .unwrap()
}

async fn source(w: &World, issue: Uuid, user: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT source FROM issue_watchers WHERE issue_id = $1 AND user_id = $2")
        .bind(issue)
        .bind(user)
        .fetch_optional(&w.pool)
        .await
        .unwrap()
}

async fn unmember(w: &World, project: Uuid, who: Uuid) {
    exec(
        &w.pool,
        &format!(
            "DELETE FROM member_roles WHERE member_id IN
               (SELECT id FROM members WHERE project_id = '{project}' AND principal_id = '{who}');
             DELETE FROM members WHERE project_id = '{project}' AND principal_id = '{who}';"
        ),
    )
    .await;
}

fn err_code(e: &anyhow::Error) -> ErrorCode {
    classify(e).unwrap_or_else(|| panic!("{e:#}")).code
}

/// What a client sees of an error apart from the message text.
fn shape(e: &anyhow::Error) -> (ErrorCode, Value) {
    let a = classify(e).unwrap_or_else(|| panic!("{e:#}"));
    (a.code, a.details.clone().unwrap_or(Value::Null))
}

async fn prefs_put(w: &World, who: AuthContext, patch: Value) -> anyhow::Result<()> {
    let cur = notifications::preferences(&w.app, who).await?;
    let mut body = patch;
    body["lock_version"] = json!(cur.lock_version);
    let input: PreferencesPut = serde_json::from_value(body).unwrap();
    notifications::put_preferences(&w.app, who, Uuid::now_v7(), &input).await?;
    Ok(())
}

/// A string custom field (Bug, every project); `visible = false` limits it
/// to `roles`.
async fn string_field(w: &World, name: &str, visible: bool, roles: &[Uuid]) -> i32 {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO field_defs (name, ftype, is_for_all, visible) VALUES ($1, 'string', true, $2) RETURNING id",
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
    for r in roles {
        sqlx::query("INSERT INTO field_roles (field_id, role_id) VALUES ($1, $2)")
            .bind(id)
            .bind(r)
            .execute(&w.pool)
            .await
            .unwrap();
    }
    id
}

/// The quoted values of a CHECK constraint, sorted.
async fn check_values(pool: &sqlx::PgPool, name: &str) -> Vec<String> {
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut v: Vec<String> = def
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(String::from)
        .collect();
    v.sort();
    v.dedup();
    v
}

fn sorted(a: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = a.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- CHECK lists

/// The 0028 CHECK lists are exactly the code constants.
#[tokio::test]
async fn check_lists_match_constants() {
    let pool = test_pool().await;
    assert_eq!(
        check_values(&pool, "issue_watchers_source_check").await,
        sorted(&notifications::WATCH_SOURCES)
    );
    assert_eq!(
        check_values(&pool, "notifications_kind_check").await,
        sorted(&notifications::KINDS)
    );
    assert_eq!(
        check_values(&pool, "notifications_reasons_check").await,
        sorted(&notifications::REASONS)
    );
}

// ---------------------------------------------------------------- basics

/// Watching, assigned and author rows; one unread row per issue that
/// collects events; on_self off by default; the issue detail's `watch`.
#[tokio::test]
async fn delivery_basics() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "first"})).await;
    dispatch(w).await;
    // bob created it: author + auto watch, but his own event is not sent.
    assert_eq!(watchers(w, i).await, vec![(w.bob, "auto_author".into())]);
    assert_eq!(rows(w, w.bob).await, 0, "on_self is off by default");
    // alice watches; carol is assigned.
    let st = notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    assert!(st.watching);
    assert_eq!(st.watcher_count, 2);
    run(
        w,
        f.bob_ctx,
        i,
        vec![set(i, json!({"assigned_to_id": w.carol}))],
    )
    .await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.reasons, vec!["watching"]);
    assert_eq!(a.events.len(), 1);
    assert_eq!(a.events[0].kind, "fields_changed");
    assert_eq!(a.events[0].actor.name, "bob");
    assert_eq!(a.issue.as_ref().unwrap().project.key, "WEB");
    let c = item(w, f.carol_ctx, &key).await.unwrap();
    assert_eq!(c.reasons, vec!["assigned"]);
    assert_eq!(
        source(w, i, w.carol).await.as_deref(),
        Some("auto_assignee"),
        "the new assignee watches"
    );
    // carol writes a note: bob (author, watcher) and alice get it, joined
    // to their unread rows.
    run(w, f.carol_ctx, i, vec![note(i, "on it", false)]).await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.events.len(), 2, "joined to the unread row");
    assert_eq!(a.events[0].kind, "note_added", "newest first");
    assert_eq!(
        a.events[0].summary.as_ref().unwrap()["note_excerpt"],
        "on it"
    );
    assert_eq!(rows_for(w, w.alice, i).await, 1);
    let b = item(w, f.bob_ctx, &key).await.unwrap();
    let r: BTreeSet<&str> = b.reasons.iter().map(String::as_str).collect();
    assert_eq!(r, BTreeSet::from(["author", "watching"]));
    assert!(item(w, f.carol_ctx, &key).await.unwrap().events.len() == 1);
    assert_eq!(unread(w, w.alice_ctx).await, Some(1));
    // The issue detail tells the caller about watching.
    let d = service::get(&w.app, f.carol_ctx, &key).await.unwrap();
    let wi = d.watch.unwrap();
    assert!(wi.watching);
    assert_eq!(wi.auto_reason.as_deref(), Some("assignee"));
    assert_eq!(wi.watcher_count, 3);
    // Read, then the next event opens a new row.
    let m = notifications::mark_read(&w.app, w.alice_ctx, &a.id.to_string())
        .await
        .unwrap();
    assert!(m.read);
    assert_eq!(unread(w, w.alice_ctx).await, Some(0));
    subject(w, f.bob_ctx, i, "second").await;
    dispatch(w).await;
    let a2 = item(w, w.alice_ctx, &key).await.unwrap();
    assert_ne!(a2.id, a.id);
    assert_eq!(a2.events.len(), 1);
    assert_eq!(rows_for(w, w.alice, i).await, 2);
    assert_eq!(inbox_all(w, w.alice_ctx).await.items.len(), 2);
}

// ---------------------------------------------------------------- T-M1-70

/// T-M1-70: watch on a missing key, an invisible issue and a malformed key
/// is the same not_found; marking someone else's or a missing row is the
/// same not_found; a project filter on an invisible project is an empty
/// list, like one without rows.
#[tokio::test]
async fn t_m1_70_not_found_is_uniform() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    // AUTH: carol is not a member.
    let (i, key) = create(w, f.bob_ctx, w.auth_p, json!({"subject": "secret"})).await;
    let n: i32 = key.rsplit('-').next().unwrap().parse().unwrap();
    let missing = format!("AUTH-{}", n + 1000);
    let e1 = notifications::watch(&w.app, f.carol_ctx, &key, true)
        .await
        .unwrap_err();
    let e2 = notifications::watch(&w.app, f.carol_ctx, &missing, true)
        .await
        .unwrap_err();
    let e3 = notifications::watch(&w.app, f.carol_ctx, &key, false)
        .await
        .unwrap_err();
    let e4 = notifications::watch(&w.app, f.carol_ctx, "not a key", true)
        .await
        .unwrap_err();
    assert_eq!(err_code(&e1), ErrorCode::NotFound);
    assert_eq!(shape(&e1), shape(&e2));
    assert_eq!(shape(&e1), shape(&e3));
    assert_eq!(shape(&e1), shape(&e4));
    assert_eq!(
        e1.to_string().replace(&key, "K"),
        e2.to_string().replace(&missing, "K")
    );
    assert!(watchers(w, i).await.iter().all(|x| x.0 != w.carol));
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_watch_optouts WHERE user_id = '{}'",
                w.carol
            )
        )
        .await,
        0,
        "nothing stored for an invisible issue"
    );
    // bob's row cannot be marked by carol: same as a missing id.
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    subject(w, f.bob_ctx, i, "secret 2").await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    for id in [a.id.to_string(), Uuid::now_v7().to_string(), "junk".into()] {
        let e = notifications::mark_read(&w.app, f.carol_ctx, &id)
            .await
            .unwrap_err();
        assert_eq!(shape(&e), (ErrorCode::NotFound, Value::Null), "{id}");
        let e = notifications::mark_unread(&w.app, f.carol_ctx, &id)
            .await
            .unwrap_err();
        assert_eq!(shape(&e), (ErrorCode::NotFound, Value::Null), "{id}");
    }
    assert!(!item(w, w.alice_ctx, &key).await.unwrap().read);
    // Project filter: AUTH (invisible to carol) and a missing project both
    // give an empty list.
    for p in ["AUTH", "NOPE"] {
        let l = notifications::list(
            &w.app,
            f.carol_ctx,
            &ListQuery {
                project: Some(p.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(l.items.is_empty() && l.next_cursor.is_none(), "{p}");
    }
    let l = notifications::list(
        &w.app,
        w.alice_ctx,
        &ListQuery {
            project: Some("AUTH".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(l.items.len(), 1);
}

// ---------------------------------------------------------------- T-M1-71

/// T-M1-71: a private note reaches alice (view_private_notes) but not dave;
/// dave gets no row and his unread count does not move.
#[tokio::test]
async fn t_m1_71_private_note() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "notes"})).await;
    for c in [w.alice_ctx, f.dave_ctx] {
        notifications::watch(&w.app, c, &key, true).await.unwrap();
    }
    dispatch(w).await;
    let before = unread(w, f.dave_ctx).await;
    run(w, f.bob_ctx, i, vec![note(i, "internal only", true)]).await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    let s = a.events[0].summary.as_ref().unwrap();
    assert_eq!(s["private"], true);
    assert_eq!(s["note_excerpt"], "internal only");
    assert_eq!(rows(w, f.dave).await, 0);
    assert_eq!(unread(w, f.dave_ctx).await, before);
    // A public note reaches both.
    run(w, f.bob_ctx, i, vec![note(i, "public", false)]).await;
    dispatch(w).await;
    let d = item(w, f.dave_ctx, &key).await.unwrap();
    assert_eq!(d.events.len(), 1);
    assert_eq!(
        d.events[0].summary.as_ref().unwrap()["note_excerpt"],
        "public"
    );
    // The note made private afterwards: dave's row has nothing readable left.
    exec(
        &pool,
        &format!("UPDATE issue_notes SET is_private = true WHERE issue_id = '{i}'"),
    )
    .await;
    assert!(item(w, f.dave_ctx, &key).await.is_none());
    assert_eq!(unread(w, f.dave_ctx).await, Some(0));
}

// ---------------------------------------------------------------- T-M1-72

/// T-M1-72: a change of a restricted field only is sent to those who may
/// read the field; together with a public field, the others get the event
/// without the restricted value.
#[tokio::test]
async fn t_m1_72_restricted_field() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let hidden = string_field(w, "Internal", false, &[w.dev]).await;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "cf"})).await;
    for c in [w.alice_ctx, f.dave_ctx] {
        notifications::watch(&w.app, c, &key, true).await.unwrap();
    }
    dispatch(w).await;
    let cf = format!("cf:{hidden}");
    run(
        w,
        f.bob_ctx,
        i,
        vec![set(i, json!({cf.clone(): {"string": "s3cret"}}))],
    )
    .await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert!(
        a.events[0]
            .summary
            .as_ref()
            .unwrap()
            .to_string()
            .contains("s3cret")
    );
    assert_eq!(rows(w, f.dave).await, 0, "only the restricted field moved");
    run(
        w,
        f.bob_ctx,
        i,
        vec![set(
            i,
            json!({cf.clone(): {"string": "other"}, "subject": "cf 2"}),
        )],
    )
    .await;
    dispatch(w).await;
    let d = item(w, f.dave_ctx, &key).await.unwrap();
    let s = d.events[0].summary.as_ref().unwrap().to_string();
    assert!(s.contains("cf 2"), "{s}");
    assert!(!s.contains("other") && !s.contains(&cf), "{s}");
}

// ---------------------------------------------------------------- T-M1-73

/// T-M1-73: a private issue reaches nobody who cannot see it, and watch on
/// it answers like a missing issue.
#[tokio::test]
async fn t_m1_73_private_issue() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(
        w,
        f.bob_ctx,
        w.web,
        json!({"subject": "private", "is_private": true}),
    )
    .await;
    let missing = format!("WEB-{}", 9999);
    let e1 = notifications::watch(&w.app, f.dave_ctx, &key, true)
        .await
        .unwrap_err();
    let e2 = notifications::watch(&w.app, f.dave_ctx, &missing, true)
        .await
        .unwrap_err();
    assert_eq!(shape(&e1), shape(&e2));
    assert_eq!(
        e1.to_string().replace(&key, "K"),
        e2.to_string().replace(&missing, "K")
    );
    // A watcher row from before (as if the issue became private later).
    exec(
        &pool,
        &format!(
            "INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ('{i}', '{}', 'manual')",
            f.dave
        ),
    )
    .await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    subject(w, f.bob_ctx, i, "private 2").await;
    dispatch(w).await;
    assert!(item(w, w.alice_ctx, &key).await.is_some());
    assert_eq!(rows(w, f.dave).await, 0);
    // watcher_count leaves dave out (he cannot see it).
    let st = notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    assert_eq!(st.watcher_count, 2, "alice and bob");
}

// ---------------------------------------------------------------- T-M1-74, 75

/// T-M1-74: access revoked after delivery: the row disappears on the next
/// read, the unread count drops, marking it is not_found; a redacted note
/// shows its marker. T-M1-75: no new rows after the revocation.
#[tokio::test]
async fn t_m1_74_75_revocation() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.auth_p, json!({"subject": "auth"})).await;
    let (j, key_j) = create(w, f.bob_ctx, w.web, json!({"subject": "web"})).await;
    for k in [&key, &key_j] {
        notifications::watch(&w.app, w.alice_ctx, k, true)
            .await
            .unwrap();
    }
    subject(w, f.bob_ctx, i, "auth 2").await;
    run(w, f.bob_ctx, j, vec![note(j, "to be redacted", false)]).await;
    dispatch(w).await;
    assert_eq!(unread(w, w.alice_ctx).await, Some(2));
    let gone = item(w, w.alice_ctx, &key).await.unwrap();

    // D12: the note is redacted; the row stays with the marker, no text.
    exec(
        &pool,
        &format!("UPDATE issue_notes SET redacted_at = now() WHERE issue_id = '{j}'"),
    )
    .await;
    let r = item(w, w.alice_ctx, &key_j).await.unwrap();
    let s = r.events[0].summary.as_ref().unwrap();
    assert_eq!(s["redacted"], true);
    assert!(s.get("note_excerpt").is_none());
    assert!(!s.to_string().contains("to be redacted"));

    unmember(w, w.auth_p, w.alice).await;
    assert!(item(w, w.alice_ctx, &key).await.is_none());
    assert_eq!(inbox_all(w, w.alice_ctx).await.items.len(), 1);
    assert_eq!(unread(w, w.alice_ctx).await, Some(1));
    let e1 = notifications::mark_read(&w.app, w.alice_ctx, &gone.id.to_string())
        .await
        .unwrap_err();
    let e2 = notifications::mark_read(&w.app, w.alice_ctx, &Uuid::now_v7().to_string())
        .await
        .unwrap_err();
    assert_eq!(shape(&e1), shape(&e2));
    assert_eq!(e1.to_string(), e2.to_string());
    // read-all does not touch it either (no oracle in its answer).
    let done = notifications::read_all(&w.app, w.alice_ctx, &ReadAllInput { before: Utc::now() })
        .await
        .unwrap();
    assert!(serde_json::to_value(&done).unwrap().get("count").is_none());

    // T-M1-75: new events are not sent to her any more.
    let before = rows_for(w, w.alice, i).await;
    subject(w, f.bob_ctx, i, "auth 3").await;
    run(w, f.bob_ctx, i, vec![note(i, "later", false)]).await;
    dispatch(w).await;
    assert_eq!(rows_for(w, w.alice, i).await, before);
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT unnest(event_ids) FROM notifications WHERE recipient_id = $1 AND issue_id = $2",
    )
    .bind(w.alice)
    .bind(i)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ids.len(), 1, "nothing joined the old row either");
    // Back in AUTH: the old row is readable again (it never held text).
    member(&pool, w.auth_p, w.alice, w.dev).await;
    assert!(
        item(w, w.alice_ctx, &key).await.is_none(),
        "it was read by read-all"
    );
    assert_eq!(inbox_all(w, w.alice_ctx).await.items.len(), 2);
}

// ---------------------------------------------------------------- T-M1-76

/// T-M1-76: an agent is never a recipient (or a watcher); its delegator
/// gets agent_on_behalf even with on_self off; a note by the agent makes the
/// delegator watch; the actor shows the delegator.
#[tokio::test]
async fn t_m1_76_agents() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "for the agent"})).await;
    dispatch(w).await;
    exec(&pool, "DELETE FROM agent_write_log").await;
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(i, lv(w, i).await, "agent did this"),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    exec(&pool, "DELETE FROM agent_write_log").await;
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(i, lv(w, i).await, "agent note"),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.reasons, vec!["agent_on_behalf"]);
    assert_eq!(a.events.len(), 2);
    let actor = &a.events[0].actor;
    assert_eq!(actor.kind, "agent");
    assert_eq!(actor.on_behalf_of.as_ref().unwrap().id, w.alice);
    let b = item(w, f.bob_ctx, &key).await.unwrap();
    assert!(b.reasons.contains(&"author".to_string()));
    assert_eq!(rows(w, d.agent_principal_id).await, 0);
    assert_eq!(
        source(w, i, w.alice).await.as_deref(),
        Some("auto_note"),
        "the delegator watches for the agent's note"
    );
    assert!(source(w, i, d.agent_principal_id).await.is_none());
    // An agent cannot read notifications or watch.
    let e = notifications::list(&w.app, agent, &ListQuery::default())
        .await
        .unwrap_err();
    assert_ne!(err_code(&e), ErrorCode::NotFound);
    assert!(notifications::unread_count(&w.app, agent).await.is_err());
    assert!(
        notifications::watch(&w.app, agent, &key, true)
            .await
            .is_err()
    );
    // on_my_agents off: nothing for alice from the agent any more.
    let n = rows(w, w.alice).await;
    prefs_put(
        w,
        w.alice_ctx,
        json!({"on_my_agents": false, "on_watching": false}),
    )
    .await
    .unwrap();
    exec(&pool, "DELETE FROM agent_write_log").await;
    changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(i, lv(w, i).await, "agent again"),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    dispatch(w).await;
    let after: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(cardinality(event_ids)), 0) FROM notifications WHERE recipient_id = $1",
    )
    .bind(w.alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows(w, w.alice).await, n);
    assert_eq!(after, 2);
}

// ---------------------------------------------------------------- T-M1-77

/// T-M1-77: a repeated outbox message and a crash after the per-issue
/// commits but before the cursor send nothing twice.
#[tokio::test]
async fn t_m1_77_at_least_once_is_exactly_once() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "once"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    dispatch(w).await;
    subject(w, f.bob_ctx, i, "once 2").await;
    // Crash before the cursor moves.
    faults::arm("notify.before_cursor", Action::Fail);
    let mut failed = false;
    for _ in 0..50 {
        match notifications::dispatch_round(&pool).await {
            Err(_) => {
                failed = true;
                break;
            }
            Ok(r) if r.messages == 0 => tokio::time::sleep(Duration::from_millis(20)).await,
            Ok(_) => {}
        }
    }
    faults::clear();
    assert!(failed, "the fault fired");
    assert_eq!(item(w, w.alice_ctx, &key).await.unwrap().events.len(), 1);
    // The same messages again (after the crash, then from the very start).
    dispatch(w).await;
    exec(
        &pool,
        "UPDATE outbox_cursors SET last_xid = '0', last_id = 0 WHERE consumer = 'notify'",
    )
    .await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.events.len(), 1);
    assert_eq!(rows_for(w, w.alice, i).await, 1);
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT unnest(event_ids) FROM notifications WHERE recipient_id = $1")
            .bind(w.alice)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(ids.len(), 1);
    // A message without new events (search-only change) sends nothing.
    exec(
        &pool,
        &format!("INSERT INTO outbox (topic, entity_id) VALUES ('issue', '{i}')"),
    )
    .await;
    let r = dispatch(w).await;
    assert_eq!(r.events, 0);
}

// ---------------------------------------------------------------- T-M1-78

/// T-M1-78: automatic watching (author, assignee, note) and an unwatch that
/// later automatic watching does not undo; the preferences switch it off.
#[tokio::test]
async fn t_m1_78_auto_watch() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(
        w,
        f.bob_ctx,
        w.web,
        json!({"subject": "auto", "assigned_to_id": w.carol}),
    )
    .await;
    dispatch(w).await;
    assert_eq!(source(w, i, w.bob).await.as_deref(), Some("auto_author"));
    assert_eq!(
        source(w, i, w.carol).await.as_deref(),
        Some("auto_assignee")
    );
    run(w, w.alice_ctx, i, vec![note(i, "hi", false)]).await;
    dispatch(w).await;
    assert_eq!(source(w, i, w.alice).await.as_deref(), Some("auto_note"));
    // carol unwatches; reassigned to her later, she stays out.
    let st = notifications::watch(&w.app, f.carol_ctx, &key, false)
        .await
        .unwrap();
    assert!(!st.watching);
    let d = service::get(&w.app, f.carol_ctx, &key).await.unwrap();
    assert!(d.watch.as_ref().unwrap().opted_out);
    run(
        w,
        f.bob_ctx,
        i,
        vec![set(i, json!({"assigned_to_id": w.bob}))],
    )
    .await;
    run(
        w,
        f.bob_ctx,
        i,
        vec![set(i, json!({"assigned_to_id": w.carol}))],
    )
    .await;
    run(w, f.carol_ctx, i, vec![note(i, "mine", false)]).await;
    dispatch(w).await;
    assert!(source(w, i, w.carol).await.is_none());
    // Manual watch overrides the optout and makes the source manual.
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    assert_eq!(source(w, i, w.alice).await.as_deref(), Some("manual"));
    // auto_watch_author off: what alice creates is not watched.
    prefs_put(w, w.alice_ctx, json!({"auto_watch_author": false}))
        .await
        .unwrap();
    let (k, _) = create(w, w.alice_ctx, w.web, json!({"subject": "no auto"})).await;
    dispatch(w).await;
    assert!(source(w, k, w.alice).await.is_none());
}

// ---------------------------------------------------------------- T-M1-79

/// T-M1-79: on_self, muted projects and locked accounts.
#[tokio::test]
async fn t_m1_79_self_mute_locked() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "self"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    notifications::watch(&w.app, f.carol_ctx, &key, true)
        .await
        .unwrap();
    dispatch(w).await;
    // on_self: alice's own change reaches her only when switched on.
    subject(w, w.alice_ctx, i, "by alice").await;
    dispatch(w).await;
    assert_eq!(rows(w, w.alice).await, 0);
    prefs_put(w, w.alice_ctx, json!({"on_self": true}))
        .await
        .unwrap();
    subject(w, w.alice_ctx, i, "by alice 2").await;
    dispatch(w).await;
    assert_eq!(rows(w, w.alice).await, 1);
    // Muting WEB: nothing more.
    let web = w.web.to_string();
    prefs_put(w, w.alice_ctx, json!({"muted_projects": [web]}))
        .await
        .unwrap();
    let p = notifications::preferences(&w.app, w.alice_ctx)
        .await
        .unwrap();
    assert_eq!(p.muted_projects, vec![w.web]);
    let before = item(w, w.alice_ctx, &key).await.unwrap().events.len();
    subject(w, f.bob_ctx, i, "muted").await;
    dispatch(w).await;
    assert_eq!(
        item(w, w.alice_ctx, &key).await.unwrap().events.len(),
        before
    );
    // A project the caller cannot see cannot be muted (same as missing).
    let e1 = prefs_put(
        w,
        f.carol_ctx,
        json!({"muted_projects": [w.auth_p.to_string()]}),
    )
    .await
    .unwrap_err();
    let e2 = prefs_put(
        w,
        f.carol_ctx,
        json!({"muted_projects": [Uuid::now_v7().to_string()]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e1), ErrorCode::InvalidValue);
    assert_eq!(kind(&e1), "unknown_project");
    assert_eq!(shape(&e1), shape(&e2));
    // A stale lock_version is a conflict.
    let stale = PreferencesPut {
        lock_version: 0,
        on_watching: Some(false),
        on_assigned: None,
        on_author: None,
        on_triage_decided: None,
        on_my_agents: None,
        on_self: None,
        on_mentioned: None,
        auto_watch_author: None,
        auto_watch_assignee: None,
        auto_watch_note: None,
        muted_projects: None,
    };
    let e = notifications::put_preferences(&w.app, w.alice_ctx, Uuid::now_v7(), &stale)
        .await
        .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::Conflict);
    // Locked: carol gets nothing.
    let c = rows(w, w.carol).await;
    exec(
        &pool,
        &format!(
            "UPDATE principals SET status = 'locked' WHERE id = '{}'",
            w.carol
        ),
    )
    .await;
    subject(w, f.bob_ctx, i, "locked").await;
    dispatch(w).await;
    assert_eq!(rows(w, w.carol).await, c);
    let cnt: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(cardinality(event_ids)), 0) FROM notifications WHERE recipient_id = $1",
    )
    .bind(w.carol)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cnt, 3, "by alice, by alice 2, muted (not 'locked')");
}

// ---------------------------------------------------------------- T-M1-80, 132

/// T-M1-80: the unread count caps at 99, counts only readable rows, and
/// answers within 2 s with the last value (stale). T-M1-132: when the
/// caller's reach changed since, it answers null.
#[tokio::test]
async fn t_m1_80_132_unread_count() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    for n in 0..99 {
        let _ = n;
        create(
            w,
            f.bob_ctx,
            w.web,
            json!({"subject": "assigned", "assigned_to_id": w.alice}),
        )
        .await;
    }
    create(
        w,
        f.bob_ctx,
        w.auth_p,
        json!({"subject": "assigned", "assigned_to_id": w.alice}),
    )
    .await;
    dispatch(w).await;
    assert_eq!(rows(w, w.alice).await, 100);
    let c = notifications::unread_count(&w.app, w.alice_ctx)
        .await
        .unwrap();
    assert_eq!((c.count, c.capped, c.stale), (Some(99), true, false));
    unmember(w, w.auth_p, w.alice).await;
    let c = notifications::unread_count(&w.app, w.alice_ctx)
        .await
        .unwrap();
    assert_eq!((c.count, c.capped, c.stale), (Some(99), false, false));
    // Slow count, same reach: the last value, stale.
    let gate = Arc::new(Gate::default());
    faults::arm("notify.count", Action::Block(gate.clone()));
    let t = Instant::now();
    let c = notifications::unread_count(&w.app, w.alice_ctx)
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_secs(2));
    assert_eq!((c.count, c.capped, c.stale), (Some(99), false, true));
    // T-M1-132: reach changed (back in AUTH): no number.
    member(&pool, w.auth_p, w.alice, w.dev).await;
    let gate = Arc::new(Gate::default());
    faults::arm("notify.count", Action::Block(gate.clone()));
    let c = notifications::unread_count(&w.app, w.alice_ctx)
        .await
        .unwrap();
    faults::clear();
    assert_eq!((c.count, c.capped, c.stale), (None, false, true));
    let c = notifications::unread_count(&w.app, w.alice_ctx)
        .await
        .unwrap();
    assert_eq!((c.count, c.capped, c.stale), (Some(99), true, false));
}

/// T-M1-132 (watchers): watcher_count leaves out people who left the
/// project and locked accounts.
#[tokio::test]
async fn t_m1_132_watcher_count() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (_, key) = create(w, f.bob_ctx, w.web, json!({"subject": "count"})).await;
    dispatch(w).await;
    for c in [w.alice_ctx, f.carol_ctx, f.dave_ctx] {
        notifications::watch(&w.app, c, &key, true).await.unwrap();
    }
    let st = notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    assert_eq!((st.watcher_count, st.watcher_count_capped), (4, false));
    unmember(w, w.web, w.carol).await;
    exec(
        &pool,
        &format!(
            "UPDATE principals SET status = 'locked' WHERE id = '{}'",
            f.dave
        ),
    )
    .await;
    let d = service::get(&w.app, f.bob_ctx, &key).await.unwrap();
    assert_eq!(d.watch.unwrap().watcher_count, 2);
}

// ---------------------------------------------------------------- T-M1-81

/// T-M1-81: read-all marks rows that arrived at or before `before`; a row
/// that arrived later stays unread.
#[tokio::test]
async fn t_m1_81_read_all_before() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, ki) = create(w, f.bob_ctx, w.web, json!({"subject": "a"})).await;
    let (j, kj) = create(w, f.bob_ctx, w.web, json!({"subject": "b"})).await;
    for k in [&ki, &kj] {
        notifications::watch(&w.app, w.alice_ctx, k, true)
            .await
            .unwrap();
    }
    subject(w, f.bob_ctx, i, "a2").await;
    dispatch(w).await;
    let loaded = inbox(w, w.alice_ctx).await;
    let before = loaded.items[0].last_at;
    subject(w, f.bob_ctx, j, "b2").await;
    dispatch(w).await;
    notifications::read_all(&w.app, w.alice_ctx, &ReadAllInput { before })
        .await
        .unwrap();
    let left = inbox(w, w.alice_ctx).await;
    assert_eq!(left.items.len(), 1);
    assert_eq!(left.items[0].issue.as_ref().unwrap().key, kj);
    assert_eq!(unread(w, w.alice_ctx).await, Some(1));
}

// ---------------------------------------------------------------- T-M1-82

/// T-M1-82: while notify lags, the outbox GC keeps its messages; the lag
/// shows on /metrics.
#[tokio::test]
async fn t_m1_82_gc_waits_for_notify() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    exec(
        &pool,
        "INSERT INTO outbox_cursors (consumer, last_xid, last_id, state)
         VALUES ('notify', '0', 0, 'active') ON CONFLICT (consumer) DO NOTHING",
    )
    .await;
    let (i, _) = create(w, f.bob_ctx, w.web, json!({"subject": "gc"})).await;
    subject(w, f.bob_ctx, i, "gc 2").await;
    w.app.index.sync_all(&pool).await.unwrap();
    exec(
        &pool,
        "UPDATE outbox SET created_at = now() - interval '2 hours'",
    )
    .await;
    let total = count(&pool, "SELECT count(*) FROM outbox").await;
    assert!(total > 0);
    let (deleted, _) = w.app.index.gc(&pool).await.unwrap();
    assert_eq!(deleted, 0, "notify has not read them");
    let m = oxledger::metrics::render(&pool, &w.app.index)
        .await
        .unwrap();
    let pending = m
        .lines()
        .find(|l| l.starts_with("notify_outbox_pending "))
        .unwrap_or_else(|| panic!("{m}"));
    assert_ne!(pending.trim_end(), "notify_outbox_pending 0", "{pending}");
    assert!(m.contains("notify_outbox_oldest_pending_age_seconds"));
    dispatch(w).await;
    w.app.index.sync_all(&pool).await.unwrap();
    let (deleted, _) = w.app.index.gc(&pool).await.unwrap();
    assert!(deleted > 0, "after notify caught up");
}

// ---------------------------------------------------------------- T-M1-83

/// T-M1-83: MCP has no notification or watch tool.
#[tokio::test]
async fn t_m1_83_no_mcp_tools() {
    for t in OxServer::tool_names() {
        assert!(
            !t.contains("notif") && !t.contains("watch"),
            "unexpected MCP tool {t}"
        );
    }
}

// ---------------------------------------------------------------- triage

/// triage_decided: the reporter hears about the Triage outcome.
#[tokio::test]
async fn triage_decided_reaches_the_reporter() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    grant(&pool, w.dev, &["triage_issues", "manage_project"]).await;
    let cur = triage::settings(&w.app, w.alice_ctx, "WEB").await.unwrap();
    triage::put_settings(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        "WEB",
        &TriageSettingsPut {
            lock_version: cur.lock_version,
            enabled: true,
            entry_rule: "all_new".into(),
            responsible_user_id: Some(w.alice),
            decline_status_id: Some(w.done),
        },
    )
    .await
    .unwrap();
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "triage me"})).await;
    prefs_put(
        w,
        f.bob_ctx,
        json!({"on_author": false, "on_watching": false}),
    )
    .await
    .unwrap();
    dispatch(w).await;
    // A field change: bob wants neither author nor watching rows.
    subject(w, w.alice_ctx, i, "triage me 2").await;
    dispatch(w).await;
    assert!(item(w, f.bob_ctx, &key).await.is_none());
    run(
        w,
        w.alice_ctx,
        i,
        vec![
            json!({"op_id": op(), "kind": "triage_decide", "target": tgt(i), "decision": "accept"}),
        ],
    )
    .await;
    dispatch(w).await;
    let b = item(w, f.bob_ctx, &key).await.unwrap();
    assert!(
        b.reasons.contains(&"triage_decided".to_string()),
        "{:?}",
        b.reasons
    );
    assert_eq!(b.events.len(), 1);
    assert_eq!(b.events[0].kind, "triage_changed");
}

// ---------------------------------------------------------------- T-M1-125

/// T-M1-125: only users can be watchers, recipients, preference or mute
/// owners (user_column_guard).
#[tokio::test]
async fn t_m1_125_user_guard() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let d = w.delegate(w.web).await.unwrap();
    let agent = d.agent_principal_id;
    let (i, _) = create(w, f.bob_ctx, w.web, json!({"subject": "guard"})).await;
    let ev: Uuid = sqlx::query_scalar("SELECT id FROM issue_events WHERE issue_id = $1 LIMIT 1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let group = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'Ops') RETURNING id",
    )
    .await;
    for who in [agent, group] {
        for sql in [
            format!(
                "INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ('{i}', '{who}', 'manual')"
            ),
            format!("INSERT INTO issue_watch_optouts (issue_id, user_id) VALUES ('{i}', '{who}')"),
            format!(
                "INSERT INTO notifications (recipient_id, issue_id, project_id, event_ids, event_reasons, reasons, first_at, last_at)
                 VALUES ('{who}', '{i}', '{}', ARRAY['{ev}']::uuid[], ARRAY['watching'], ARRAY['watching'], now(), now())",
                w.web
            ),
            format!("INSERT INTO notification_preferences (user_id) VALUES ('{who}')"),
            format!(
                "INSERT INTO notification_project_mutes (user_id, project_id) VALUES ('{who}', '{}')",
                w.web
            ),
        ] {
            let e = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&pool)
                .await
                .unwrap_err();
            let code = e
                .as_database_error()
                .and_then(|d| d.code())
                .map(|c| c.to_string());
            assert_eq!(code.as_deref(), Some("23514"), "{sql}: {e}");
            assert!(e.to_string().contains("must be a user"), "{sql}: {e}");
        }
    }
    // Moving a row to an agent is refused too.
    exec(
        &pool,
        &format!(
            "INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ('{i}', '{}', 'manual') ON CONFLICT DO NOTHING",
            w.carol
        ),
    )
    .await;
    let e = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "UPDATE issue_watchers SET user_id = '{agent}' WHERE user_id = '{}'",
        w.carol
    )))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("must be a user"), "{e}");
}

// ---------------------------------------------------------------- T-M1-126

/// T-M1-126: an event dispatched while its row is being marked read lands
/// in a new unread row, never in the read one; `unread` on a read row joins
/// it to the issue's unread row.
#[tokio::test]
async fn t_m1_126_read_race_and_unread_merge() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "race"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    dispatch(w).await;
    subject(w, f.bob_ctx, i, "race 1").await;
    dispatch(w).await;
    let first = item(w, w.alice_ctx, &key).await.unwrap();
    // The dispatcher stops just before locking alice's unread row …
    subject(w, f.bob_ctx, i, "race 2").await;
    let gate = Arc::new(Gate::default());
    faults::arm("notify.deliver", Action::Block(gate.clone()));
    let p = pool.clone();
    let job = tokio::spawn(async move { notifications::dispatch_all(&p).await });
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the dispatcher reached notify.deliver");
    // … alice reads the row meanwhile …
    notifications::mark_read(&w.app, w.alice_ctx, &first.id.to_string())
        .await
        .unwrap();
    gate.release.notify_one();
    job.await.unwrap().unwrap();
    faults::clear();
    // … so the new event is in a new unread row.
    let second = item(w, w.alice_ctx, &key).await.unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(second.events.len(), 1);
    let read: Vec<Uuid> =
        sqlx::query_scalar("SELECT unnest(event_ids) FROM notifications WHERE id = $1")
            .bind(first.id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(read.len(), 1, "the read row did not grow");
    // unread on the read row: one unread row with both events.
    let m = notifications::mark_unread(&w.app, w.alice_ctx, &first.id.to_string())
        .await
        .unwrap();
    assert!(m.merged_into && !m.read);
    assert_eq!(rows_for(w, w.alice, i).await, 1);
    let merged = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(merged.id, m.id);
    assert_eq!(merged.events.len(), 2);
    assert_eq!(merged.events[0].id, second.events[0].id, "event order kept");
    // unread on an unread row: unchanged.
    let again = notifications::mark_unread(&w.app, w.alice_ctx, &m.id.to_string())
        .await
        .unwrap();
    assert!(!again.merged_into);
    assert_eq!(again.id, m.id);
}

/// Older rows past retention go; newer ones stay.
#[tokio::test]
async fn retention_purge() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "old"})).await;
    let (j, kj) = create(w, f.bob_ctx, w.web, json!({"subject": "new"})).await;
    for k in [&key, &kj] {
        notifications::watch(&w.app, w.alice_ctx, k, true)
            .await
            .unwrap();
    }
    subject(w, f.bob_ctx, i, "old 2").await;
    subject(w, f.bob_ctx, j, "new 2").await;
    dispatch(w).await;
    exec(
        &pool,
        &format!(
            "UPDATE notifications SET first_at = now() - interval '91 days',
                    last_at = now() - interval '91 days' WHERE issue_id = '{i}'"
        ),
    )
    .await;
    assert_eq!(notifications::purge(&pool).await.unwrap(), 1);
    assert_eq!(rows(w, w.alice).await, 1);
    let a = item(w, w.alice_ctx, &kj).await.unwrap();
    notifications::mark_read(&w.app, w.alice_ctx, &a.id.to_string())
        .await
        .unwrap();
    exec(
        &pool,
        "UPDATE notifications SET read_at = now() - interval '29 days'",
    )
    .await;
    assert_eq!(notifications::purge(&pool).await.unwrap(), 0);
    exec(
        &pool,
        "UPDATE notifications SET read_at = now() - interval '31 days'",
    )
    .await;
    assert_eq!(notifications::purge(&pool).await.unwrap(), 1);
}

/// Paging with a cursor, and the 50-event cap of a row.
#[tokio::test]
async fn paging_and_event_cap() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mut keys = Vec::new();
    for _ in 0..5 {
        let (_, k) = create(
            w,
            f.bob_ctx,
            w.web,
            json!({"subject": "page", "assigned_to_id": w.alice}),
        )
        .await;
        keys.push(k);
    }
    dispatch(w).await;
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let l = notifications::list(
            &w.app,
            w.alice_ctx,
            &ListQuery {
                limit: Some(2),
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seen.extend(
            l.items
                .iter()
                .map(|i| i.issue.as_ref().unwrap().key.clone()),
        );
        match l.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    keys.reverse();
    assert_eq!(seen, keys, "newest first, each once");
    let e = notifications::list(
        &w.app,
        w.alice_ctx,
        &ListQuery {
            filter: Some("bogus".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::InvalidArgument);

    // 52 events on one issue: the row keeps the newest 50.
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "busy"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    dispatch(w).await;
    for n in 0..52 {
        subject(w, f.bob_ctx, i, &format!("busy {n}")).await;
    }
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.events.len(), 3);
    assert_eq!(a.more_events, 47);
}

// ---------------------------------------------------------------- verification follow-ups

/// Waits until a backend of this database waits on a lock while running a
/// statement LIKE `pattern` (at most 20 s).
async fn wait_lock(pool: &sqlx::PgPool, pattern: &str) {
    let t = Instant::now();
    loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE $1",
        )
        .bind(pattern)
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "nobody waits on a lock in {pattern}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Redacts (D12, event level) the newest event of `kind` on issue `i`.
async fn redact_event(w: &World, i: Uuid, kind: &str) -> Uuid {
    let ev: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND kind = $2 ORDER BY seq DESC LIMIT 1",
    )
    .bind(i)
    .bind(kind)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    let r: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_event_redactions (event_id, redacted_by, reason_code, reason)
         VALUES ($1, $2, 'test', 'test') RETURNING id",
    )
    .bind(ev)
    .bind(w.alice)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE issue_events SET payload = '\"[已遮蔽]\"', redaction_id = $2 WHERE id = $1",
    )
    .bind(ev)
    .bind(r)
    .execute(&w.pool)
    .await
    .unwrap();
    ev
}

/// Verification D1 (T-M1-81 under parallel dispatch): a row whose
/// transaction read the clock earlier but commits later cannot hide behind
/// a row the list already showed: for one person, rows commit in `last_at`
/// order (per-recipient lock), so the list shows nothing until the earlier
/// row is in, and `read-all {before}` marks exactly what the list showed.
#[tokio::test]
async fn v_read_all_before_marks_row_committed_after_list() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (x, kx) = create(w, f.bob_ctx, w.web, json!({"subject": "x"})).await;
    let (y, ky) = create(w, f.bob_ctx, w.web, json!({"subject": "y"})).await;
    for k in [&kx, &ky] {
        notifications::watch(&w.app, w.alice_ctx, k, true)
            .await
            .unwrap();
    }
    dispatch(w).await;
    run(w, f.carol_ctx, x, vec![note(x, "seen?", false)]).await;
    subject(w, f.bob_ctx, y, "y2").await;
    // y's dispatch waits for its progress row …
    let mut hold_y = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM notification_issue_progress WHERE issue_id = $1 FOR UPDATE")
        .bind(y)
        .execute(&mut *hold_y)
        .await
        .unwrap();
    // … x's dispatch stops in automatic watching (carol's note), after it
    // wrote alice's row and before it commits.
    let mut hold_x = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ($1, $2, 'manual')")
        .bind(x)
        .bind(w.carol)
        .execute(&mut *hold_x)
        .await
        .unwrap();
    let p = pool.clone();
    let job = tokio::spawn(async move {
        loop {
            let r = notifications::dispatch_round(&p).await?;
            if r.events > 0 {
                return anyhow::Ok(r);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    wait_lock(&pool, "%INSERT INTO issue_watchers%").await;
    wait_lock(&pool, "%notification_issue_progress%").await;
    hold_y.commit().await.unwrap();
    // y now waits for alice's lock, held by x's transaction: it cannot get
    // a later last_at committed before x's row.
    wait_lock(&pool, "%pg_advisory_xact_lock%").await;
    for _ in 0..10 {
        assert!(inbox(w, w.alice_ctx).await.items.is_empty(), "y overtook x");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    hold_x.rollback().await.unwrap();
    job.await.unwrap().unwrap();
    let shown = inbox(w, w.alice_ctx).await;
    let keys: Vec<&str> = shown
        .items
        .iter()
        .map(|i| i.issue.as_ref().unwrap().key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![ky.as_str(), kx.as_str()],
        "y last (committed last)"
    );
    let before = shown.items[0].last_at;
    // Something new on x after the list was loaded: its row moves past
    // `before` and stays unread.
    subject(w, f.bob_ctx, x, "x2").await;
    dispatch(w).await;
    notifications::read_all(&w.app, w.alice_ctx, &ReadAllInput { before })
        .await
        .unwrap();
    let left = inbox(w, w.alice_ctx).await;
    assert_eq!(left.items.len(), 1);
    assert_eq!(left.items[0].issue.as_ref().unwrap().key, kx);
}

/// Verification D2: watcher_count is one statement; 1000 watchers who
/// cannot see the issue cost little.
#[tokio::test]
async fn v_watcher_count_with_many_invisible_watchers() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "crowd"})).await;
    exec(
        &pool,
        &format!(
            "WITH u AS (INSERT INTO principals (kind, login, name)
                        SELECT 'user', 'crowd' || g, 'Crowd ' || g FROM generate_series(1, 1000) g
                        RETURNING id)
             INSERT INTO issue_watchers (issue_id, user_id, source) SELECT '{i}', id, 'manual' FROM u"
        ),
    )
    .await;
    // Warm up once (plans, caches), then measure.
    service::get(&w.app, f.bob_ctx, &key).await.unwrap();
    let t = Instant::now();
    let d = service::get(&w.app, f.bob_ctx, &key).await.unwrap();
    let took = t.elapsed();
    let wi = d.watch.unwrap();
    assert_eq!(
        (wi.watcher_count, wi.watcher_count_capped),
        (1, false),
        "bob only"
    );
    assert!(took < Duration::from_millis(500), "GET took {took:?}");
    let (sql, rust) = notifications::watcher_count_both(&pool, i).await.unwrap();
    assert_eq!(sql, rust);
}

/// Verification D2: the SQL watcher count agrees with `load_scope` +
/// `can_see` for admins, groups, default/own visibility, tracker limits,
/// the non-member role of a public project and archived projects.
#[tokio::test]
async fn v_watcher_count_matches_scope() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let mk = |login: &'static str| {
        let pool = pool.clone();
        async move {
            auth::create_user(&pool, login, login, PW, false)
                .await
                .unwrap()
        }
    };
    let root = auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let (erin, frank, gina, hank) = (
        mk("erin").await,
        mk("frank").await,
        mk("gina").await,
        mk("hank").await,
    );
    let group = id(
        &pool,
        "INSERT INTO principals (kind, name) VALUES ('group', 'Crew') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO group_members (group_id, user_id) VALUES ('{group}', '{erin}')"),
    )
    .await;
    let own = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Own','own') RETURNING id",
    )
    .await;
    grant(&pool, own, &["view_issues"]).await;
    let viewer: Uuid = sqlx::query_scalar("SELECT id FROM roles WHERE name = 'Viewer'")
        .fetch_one(&pool)
        .await
        .unwrap();
    // erin: Viewer through the group; frank: Own; gina: Viewer limited to
    // another tracker; hank: not a member (non-member role once public).
    member(&pool, w.web, group, viewer).await;
    member(&pool, w.web, frank, own).await;
    let tr = id(
        &pool,
        "INSERT INTO trackers (name, position, default_status_id) SELECT 'Task', 2, id FROM issue_statuses WHERE name = 'New' RETURNING id",
    )
    .await;
    let limited = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Limited','all') RETURNING id",
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ('{limited}', 'view_issues', false);
             INSERT INTO role_permission_trackers VALUES ('{limited}', 'view_issues', '{tr}');"
        ),
    )
    .await;
    member(&pool, w.web, gina, limited).await;
    let nm = id(
        &pool,
        "INSERT INTO roles (name, builtin, issues_visibility) VALUES ('Non member', 'non_member', 'default') RETURNING id",
    )
    .await;
    grant(&pool, nm, &["view_issues"]).await;

    let (i, _) = create(
        w,
        f.bob_ctx,
        w.web,
        json!({"subject": "who sees", "is_private": true}),
    )
    .await;
    for u in [root, erin, frank, gina, hank, f.dave, w.alice] {
        exec(
            &pool,
            &format!("INSERT INTO issue_watchers (issue_id, user_id, source) VALUES ('{i}', '{u}', 'manual') ON CONFLICT DO NOTHING"),
        )
        .await;
    }
    let check = |label: &'static str, want: u32| {
        let pool = pool.clone();
        async move {
            let (sql, rust) = notifications::watcher_count_both(&pool, i).await.unwrap();
            assert_eq!(sql, rust, "{label}");
            assert_eq!(sql, want, "{label}");
        }
    };
    // private: root (admin), alice, bob (Dev, all).
    check("private", 3).await;
    exec(
        &pool,
        &format!("UPDATE issues SET assigned_to_id = '{group}' WHERE id = '{i}'"),
    )
    .await;
    // + erin (assigned to her group, default) + frank? no (own: not his).
    check("assigned to the group", 4).await;
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = false, assigned_to_id = NULL WHERE id = '{i}'"),
    )
    .await;
    // public issue: + erin, dave (default); frank (own) no; gina (other tracker) no.
    check("public", 5).await;
    exec(
        &pool,
        &format!("UPDATE issues SET assigned_to_id = '{frank}' WHERE id = '{i}'"),
    )
    .await;
    check("assigned to frank (own)", 6).await;
    exec(
        &pool,
        &format!("UPDATE issues SET tracker_id = '{tr}' WHERE id = '{i}'"),
    )
    .await;
    // Task tracker: gina sees it; Dev/Viewer roles have all trackers.
    check("gina's tracker", 7).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET is_public = true WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    check("public project: hank as non-member", 8).await;
    exec(
        &pool,
        &format!("UPDATE principals SET status = 'locked' WHERE id = '{hank}'"),
    )
    .await;
    check("hank locked", 7).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'archived' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    check("archived project", 0).await;
}

/// Verification D3: on_my_agents off silences what my own agents do, also
/// through watching; other people's changes still arrive.
#[tokio::test]
async fn v_on_my_agents_off_also_for_watching() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "agents"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    prefs_put(w, w.alice_ctx, json!({"on_my_agents": false}))
        .await
        .unwrap();
    exec(&pool, "DELETE FROM agent_write_log").await;
    changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(i, lv(w, i).await, "by my agent"),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    dispatch(w).await;
    assert_eq!(rows(w, w.alice).await, 0, "watching, but it was my agent");
    subject(w, f.bob_ctx, i, "by bob").await;
    dispatch(w).await;
    let a = item(w, w.alice_ctx, &key).await.unwrap();
    assert_eq!(a.events.len(), 1);
    assert_eq!(a.events[0].actor.name, "bob");
    assert_eq!(a.reasons, vec!["watching"]);
}

/// Verification D4: an unwatch while a dispatch is writing the automatic
/// watch waits for it and wins (the opt-out holds).
#[tokio::test]
async fn v_unwatch_during_auto_watch() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "race"})).await;
    run(w, f.carol_ctx, i, vec![note(i, "I will watch", false)]).await;
    let gate = Arc::new(Gate::default());
    faults::arm("notify.after_auto_watch", Action::Block(gate.clone()));
    let p = pool.clone();
    let job = tokio::spawn(async move { notifications::dispatch_all(&p).await });
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the dispatcher wrote the automatic watch");
    let app = w.app.clone();
    let carol = f.carol_ctx;
    let k = key.clone();
    let unwatch = tokio::spawn(async move { notifications::watch(&app, carol, &k, false).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !unwatch.is_finished(),
        "the unwatch waits for the dispatch's automatic watch"
    );
    gate.release.notify_one();
    job.await.unwrap().unwrap();
    let st = unwatch.await.unwrap().unwrap();
    faults::clear();
    assert!(!st.watching);
    assert!(source(w, i, w.carol).await.is_none());
    let d = service::get(&w.app, f.carol_ctx, &key).await.unwrap();
    assert!(d.watch.unwrap().opted_out);
    // Later notes do not watch her again.
    run(w, f.carol_ctx, i, vec![note(i, "again", false)]).await;
    dispatch(w).await;
    assert!(source(w, i, w.carol).await.is_none());
}

/// Verification D5: an issue that keeps failing is skipped after
/// MAX_ISSUE_FAILURES rounds; the other issues are dispatched from the first
/// round and the cursor moves on.
#[tokio::test]
async fn v_poison_issue_is_skipped() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, ki) = create(w, f.bob_ctx, w.web, json!({"subject": "poison"})).await;
    let (j, kj) = create(w, f.bob_ctx, w.web, json!({"subject": "fine"})).await;
    for k in [&ki, &kj] {
        notifications::watch(&w.app, w.alice_ctx, k, true)
            .await
            .unwrap();
    }
    subject(w, f.bob_ctx, i, "poison 2").await;
    subject(w, f.bob_ctx, j, "fine 2").await;
    let point = format!("notify.issue:{i}");
    for _ in 0..notifications::MAX_ISSUE_FAILURES {
        faults::arm(&point, Action::Deadlock);
    }
    let mut errors = 0;
    let mut ok = false;
    for _ in 0..50 {
        match notifications::dispatch_round(&pool).await {
            Err(_) => {
                errors += 1;
                assert!(
                    item(w, w.alice_ctx, &kj).await.is_some(),
                    "j goes out anyway"
                );
            }
            Ok(r) if r.messages > 0 => {
                ok = true;
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    faults::clear();
    assert!(ok);
    assert_eq!(errors, notifications::MAX_ISSUE_FAILURES - 1);
    assert_eq!(rows_for(w, w.alice, i).await, 0, "i's event was skipped");
    let (progress, last): (i64, i64) = sqlx::query_as(
        "SELECT np.last_event_seq, (SELECT max(seq) FROM issue_events WHERE issue_id = $1)
         FROM notification_issue_progress np WHERE np.issue_id = $1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(progress, last);
    let m = oxledger::metrics::render(&pool, &w.app.index)
        .await
        .unwrap();
    assert!(
        m.lines()
            .any(|l| l.starts_with("notify_skipped_issues_total ") && !l.ends_with(" 0")),
        "{m}"
    );
    // Later events of i are sent as usual.
    subject(w, f.bob_ctx, i, "poison 3").await;
    dispatch(w).await;
    assert_eq!(item(w, w.alice_ctx, &ki).await.unwrap().events.len(), 1);
}

/// Verification D6: the reasons shown are those of events the reader can
/// still read. (The row is widened by hand: a private note that dave was
/// never sent, with a reason of its own, next to an event he can read.)
#[tokio::test]
async fn v_reasons_follow_visible_events() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "reasons"})).await;
    notifications::watch(&w.app, f.dave_ctx, &key, true)
        .await
        .unwrap();
    run(w, f.bob_ctx, i, vec![note(i, "internal", true)]).await;
    subject(w, f.bob_ctx, i, "by bob").await;
    dispatch(w).await;
    let d = item(w, f.dave_ctx, &key).await.unwrap();
    assert_eq!(d.events.len(), 1, "the private note was not sent");
    let hidden: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND kind = 'note_added'",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE notifications SET event_ids = array_prepend($2, event_ids),
                event_reasons = array_prepend('assigned', event_reasons),
                reasons = ARRAY['assigned', 'watching']
         WHERE id = $1",
    )
    .bind(d.id)
    .bind(hidden)
    .execute(&pool)
    .await
    .unwrap();
    let d = item(w, f.dave_ctx, &key).await.unwrap();
    assert_eq!(d.events.len(), 1);
    assert_eq!(d.reasons, vec!["watching"], "not the hidden event's reason");
    exec(
        &pool,
        &format!("UPDATE issue_notes SET is_private = false WHERE issue_id = '{i}'"),
    )
    .await;
    let d = item(w, f.dave_ctx, &key).await.unwrap();
    assert_eq!(d.events.len(), 2);
    assert_eq!(d.reasons, vec!["assigned", "watching"]);
}

/// Mutation guard: an event redacted before dispatch is never sent.
#[tokio::test]
async fn v_redacted_before_dispatch_not_sent() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "redact"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    run(w, f.bob_ctx, i, vec![note(i, "oops, a secret", false)]).await;
    redact_event(w, i, "note_added").await;
    dispatch(w).await;
    assert_eq!(rows_for(w, w.alice, i).await, 0);
}

/// Mutation guard: the progress lock. Dispatcher A stops before writing;
/// B runs meanwhile and alice marks her row read; each (recipient, event)
/// still lands once.
#[tokio::test]
async fn v_progress_lock_with_read_between() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "twice?"})).await;
    notifications::watch(&w.app, w.alice_ctx, &key, true)
        .await
        .unwrap();
    subject(w, f.bob_ctx, i, "one").await;
    dispatch(w).await;
    let first = item(w, w.alice_ctx, &key).await.unwrap();
    subject(w, f.bob_ctx, i, "two").await;
    let gate = Arc::new(Gate::default());
    faults::arm("notify.deliver", Action::Block(gate.clone()));
    let p = pool.clone();
    let a = tokio::spawn(async move { notifications::dispatch_all(&p).await });
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .unwrap();
    let p = pool.clone();
    let b = tokio::spawn(async move { notifications::dispatch_round(&p).await });
    wait_lock(&pool, "%notification_issue_progress%").await;
    notifications::mark_read(&w.app, w.alice_ctx, &first.id.to_string())
        .await
        .unwrap();
    gate.release.notify_one();
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    faults::clear();
    dispatch(w).await;
    let pairs: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT e, count(*) FROM notifications n, unnest(n.event_ids) e
         WHERE n.recipient_id = $1 GROUP BY e",
    )
    .bind(w.alice)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(pairs.len(), 2, "{pairs:?}");
    assert!(pairs.iter().all(|p| p.1 == 1), "{pairs:?}");
}

/// Mutation guard: delivered rows are checked again on read for each way
/// access can go — a restricted field, a private issue, a move to a project
/// the reader is not in, an event redaction (marker, row kept) and a role
/// that loses view_issues.
#[tokio::test]
async fn v_revocation_variants() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let shown = string_field(w, "Shown", true, &[]).await;
    let mut keys = Vec::new();
    let mut ids = Vec::new();
    for n in 0..4 {
        let (i, k) = create(w, f.bob_ctx, w.web, json!({"subject": format!("v{n}")})).await;
        notifications::watch(&w.app, f.dave_ctx, &k, true)
            .await
            .unwrap();
        ids.push(i);
        keys.push(k);
    }
    let cf = format!("cf:{shown}");
    run(
        w,
        f.bob_ctx,
        ids[0],
        vec![set(ids[0], json!({cf: {"string": "value"}}))],
    )
    .await;
    for i in &ids[1..] {
        subject(w, f.bob_ctx, *i, "changed").await;
    }
    dispatch(w).await;
    assert_eq!(unread(w, f.dave_ctx).await, Some(4));
    let row0 = item(w, f.dave_ctx, &keys[0]).await.unwrap();
    // The only changed field becomes restricted.
    exec(
        &pool,
        &format!("UPDATE field_defs SET visible = false WHERE id = {shown}"),
    )
    .await;
    assert!(item(w, f.dave_ctx, &keys[0]).await.is_none());
    assert_eq!(unread(w, f.dave_ctx).await, Some(3));
    let e = notifications::mark_read(&w.app, f.dave_ctx, &row0.id.to_string())
        .await
        .unwrap_err();
    assert_eq!(err_code(&e), ErrorCode::NotFound);
    // The issue becomes private.
    exec(
        &pool,
        &format!(
            "UPDATE issues SET is_private = true WHERE id = '{}'",
            ids[1]
        ),
    )
    .await;
    assert!(item(w, f.dave_ctx, &keys[1]).await.is_none());
    assert_eq!(unread(w, f.dave_ctx).await, Some(2));
    // Moved to AUTH (dave is not a member).
    exec(
        &pool,
        &format!(
            "UPDATE issues SET project_id = '{}' WHERE id = '{}'",
            w.auth_p, ids[2]
        ),
    )
    .await;
    assert!(item(w, f.dave_ctx, &keys[2]).await.is_none());
    assert_eq!(unread(w, f.dave_ctx).await, Some(1));
    // The event is redacted: the row stays, with the marker only.
    redact_event(w, ids[3], "fields_changed").await;
    let r = item(w, f.dave_ctx, &keys[3]).await.unwrap();
    assert!(r.events[0].redacted);
    assert!(r.events[0].summary.is_none());
    // The role loses view_issues: nothing at all.
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE role_id = (SELECT id FROM roles WHERE name = 'Viewer')",
    )
    .await;
    assert!(inbox_all(w, f.dave_ctx).await.items.is_empty());
    assert_eq!(unread(w, f.dave_ctx).await, Some(0));
    assert_eq!(
        rows(w, f.dave).await,
        4,
        "rows stay; they only stop showing"
    );
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
    idem: Option<Uuid>,
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
    if let Some(k) = idem {
        req = req.header("idempotency-key", k.to_string());
    }
    if method != "GET" {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
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

/// Every M1.8 route over HTTP; watch on an invisible issue is the same 404
/// body as a missing one.
#[tokio::test]
async fn m1_8_http() {
    let pool = test_pool().await;
    let f = setup(&pool).await;
    let w = &f.w;
    let (i, key) = create(w, f.bob_ctx, w.web, json!({"subject": "http"})).await;
    let (_, secret) = create(w, f.bob_ctx, w.auth_p, json!({"subject": "hidden"})).await;
    let r = router(w);
    let (s, _) = call(&r, None, "GET", "/notifications", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let alice = login(&r, "alice").await;
    let carol = login(&r, "carol").await;

    let (s, v) = call(
        &r,
        Some(&alice),
        "PUT",
        &format!("/issues/{key}/watch"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["watching"], true);
    let (s1, v1) = call(
        &r,
        Some(&carol),
        "PUT",
        &format!("/issues/{secret}/watch"),
        None,
        None,
    )
    .await;
    let (s2, v2) = call(
        &r,
        Some(&carol),
        "PUT",
        "/issues/AUTH-999/watch",
        None,
        None,
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    let strip = |mut v: Value, k: &str| {
        if let Some(m) = v["error"]["message"].as_str() {
            v["error"]["message"] = json!(m.replace(k, "K"));
        }
        v["error"]["request_id"] = Value::Null;
        v
    };
    assert_eq!(strip(v1, &secret), strip(v2, "AUTH-999"));

    subject(w, f.bob_ctx, i, "http 2").await;
    dispatch(w).await;
    let (s, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/notifications/unread-count",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["count"], 1);
    let (s, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/notifications?filter=all&limit=10",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let nid = items[0]["id"].as_str().unwrap().to_string();
    let last_at = items[0]["last_at"].clone();
    assert_eq!(items[0]["issue"]["key"], key);
    let (s, v) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("/notifications/{nid}/read"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["read"], true);
    let (s, v) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("/notifications/{nid}/unread"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["read"], false);
    let (s, _) = call(
        &r,
        Some(&carol),
        "POST",
        &format!("/notifications/{nid}/read"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = call(
        &r,
        Some(&alice),
        "POST",
        "/notifications/read-all",
        Some(json!({"before": last_at})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/notifications/unread-count",
        None,
        None,
    )
    .await;
    assert_eq!(v["data"]["count"], 0);
    let (s, v) = call(
        &r,
        Some(&alice),
        "DELETE",
        &format!("/issues/{key}/watch"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["watching"], false);

    let (s, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/me/notification-preferences",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["on_self"], false);
    let lvn = v["data"]["lock_version"].clone();
    let body = json!({"lock_version": lvn, "on_self": true});
    let (s, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/me/notification-preferences",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "Idempotency-Key is required: {v}"
    );
    let k = Uuid::now_v7();
    let (s, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/me/notification-preferences",
        Some(body.clone()),
        Some(k),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["on_self"], true);
    let (s, v2) = call(
        &r,
        Some(&alice),
        "PUT",
        "/me/notification-preferences",
        Some(body),
        Some(k),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "replayed: {v2}");
    assert_eq!(v2["data"], v["data"]);
    let (s, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/me/notification-preferences",
        Some(json!({"lock_version": 0, "bogus": true})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    let (s, v) = call(
        &r,
        Some(&alice),
        "GET",
        &format!("/issues/{key}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["watch"]["watching"], false);
    assert_eq!(v["data"]["watch"]["opted_out"], true);
}

// ---------------------------------------------------------------- performance

/// Dispatch latency at 10 000 issues and 50 people (§8 M1.8: p95 ≤ 10 s).
/// Ignored by default: `cargo test --test m1_8_notifications -- --ignored
/// perf_dispatch_p95 --nocapture`. The dispatcher runs as `serve` does (a
/// round, then 1 s of sleep unless the batch was full); 10 writers change
/// subjects as fast as they can; latency = commit returned → the event is
/// in a notification row (polled every 50 ms).
#[tokio::test]
#[ignore]
async fn perf_dispatch_p95() {
    const ISSUES: i64 = 10_000;
    const PEOPLE: usize = 50;
    const WRITERS: usize = 10;
    const PER_WRITER: usize = 60;
    const WATCHERS_PER_ISSUE: i64 = 5;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    // 47 more people (50 with alice, bob, carol), all in WEB.
    let mut people = vec![w.alice, w.bob, w.carol];
    for n in 0..PEOPLE - 3 {
        let login = format!("u{n:02}");
        let u = auth::create_user(&pool, &login, &login, PW, false)
            .await
            .unwrap();
        member(&pool, w.web, u, w.dev).await;
        people.push(u);
    }
    // 10 000 issues in one statement (no events: as if imported).
    sqlx::query(
        "WITH ins AS (
           INSERT INTO issues (project_id, tracker_id, status_id, subject, description, author_id)
           SELECT $1, $2, $3, 'perf ' || g, '', $4 FROM generate_series(1, $5::int) g
           RETURNING id, subject)
         INSERT INTO issue_keys (key, issue_id, project_id, number)
         SELECT 'WEB-' || substr(subject, 6), id, $1, substr(subject, 6)::int FROM ins",
    )
    .bind(w.web)
    .bind(w.bug)
    .bind(w.new)
    .bind(w.bob)
    .bind(ISSUES as i32)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE projects SET issue_counter = $2 WHERE id = $1")
        .bind(w.web)
        .bind(ISSUES as i32)
        .execute(&pool)
        .await
        .unwrap();
    // 5 watchers per issue, spread over the 50 people.
    sqlx::query(
        "INSERT INTO issue_watchers (issue_id, user_id, source)
         SELECT i.id, ($1::uuid[])[1 + ((k.number * 7 + s * 11) % $2)], 'manual'
         FROM issue_keys k JOIN issues i ON i.id = k.issue_id, generate_series(0, $3 - 1) s
         ON CONFLICT DO NOTHING",
    )
    .bind(&people)
    .bind(PEOPLE as i32)
    .bind(WATCHERS_PER_ISSUE as i32)
    .execute(&pool)
    .await
    .unwrap();
    // Catch up with the import's outbox before measuring.
    let t = Instant::now();
    notifications::dispatch_all(&pool).await.unwrap();
    eprintln!("catch-up over {ISSUES} import messages: {:?}", t.elapsed());

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher = {
        let pool = pool.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let more = notifications::dispatch_round(&pool)
                    .await
                    .map(|r| r.more)
                    .unwrap_or(false);
                if !more {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        })
    };
    // Writers: 10 people (u00..u09), each on its own issues.
    let mut writers = Vec::new();
    for n in 0..WRITERS {
        let ctx = sign_in(&pool, &w.policy, &format!("u{n:02}")).await;
        let app = w.app.clone();
        let pool = pool.clone();
        writers.push(tokio::spawn(async move {
            let mut done: Vec<(Uuid, Instant)> = Vec::new();
            for k in 0..PER_WRITER {
                let number = (n * PER_WRITER + k) as i32 * 13 % ISSUES as i32 + 1;
                let (issue, lock): (Uuid, i32) = sqlx::query_as(
                    "SELECT i.id, i.lock_version FROM issue_keys k JOIN issues i ON i.id = k.issue_id
                     WHERE k.key = $1",
                )
                .bind(format!("WEB-{number}"))
                .fetch_one(&pool)
                .await
                .unwrap();
                let r = commands::submit(
                    &app,
                    ctx,
                    Uuid::now_v7(),
                    &doc(
                        vec![set(issue, json!({"subject": format!("perf edit {n}-{k}")}))],
                        vec![(issue, lock)],
                    ),
                )
                .await
                .unwrap();
                let at = Instant::now();
                let ev: Uuid = sqlx::query_scalar(
                    "SELECT id FROM issue_events WHERE command_execution_id = $1 LIMIT 1",
                )
                .bind(r.execution_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                done.push((ev, at));
            }
            done
        }));
    }
    let started = Instant::now();
    let mut sent: Vec<(Uuid, Instant)> = Vec::new();
    for h in writers {
        sent.extend(h.await.unwrap());
    }
    let wrote = started.elapsed();
    // Poll until every event is in some row.
    let mut seen: std::collections::HashMap<Uuid, Instant> = std::collections::HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while seen.len() < sent.len() && Instant::now() < deadline {
        let pending: Vec<Uuid> = sent
            .iter()
            .map(|s| s.0)
            .filter(|e| !seen.contains_key(e))
            .collect();
        let got: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT e FROM notifications n, unnest(n.event_ids) e WHERE e = ANY($1)",
        )
        .bind(&pending)
        .fetch_all(&pool)
        .await
        .unwrap();
        let now = Instant::now();
        for e in got {
            seen.entry(e).or_insert(now);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    dispatcher.await.unwrap();
    assert_eq!(seen.len(), sent.len(), "every event was delivered");
    let mut lat: Vec<f64> = sent
        .iter()
        .map(|(e, at)| seen[e].saturating_duration_since(*at).as_secs_f64())
        .collect();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| lat[((lat.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    let rows: i64 = count(&pool, "SELECT count(*) FROM notifications").await;
    eprintln!(
        "events {} in {:?} ({:.1}/s); notification rows {rows}; latency p50 {:.2}s p95 {:.2}s max {:.2}s",
        lat.len(),
        wrote,
        lat.len() as f64 / wrote.as_secs_f64(),
        pct(0.50),
        pct(0.95),
        lat[lat.len() - 1]
    );
    assert!(pct(0.95) <= 10.0, "p95 {:.2}s > 10 s", pct(0.95));
}
