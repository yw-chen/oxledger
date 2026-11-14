// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.3: Triage (§2.4, §3.4, §3.10.1, §4.1, §4.3).
//! T-M1-21 … T-M1-30c, settings, counts (R28), the `triage` and
//! `triage_self_decided` filter fields, the `triage` view column, the issue
//! detail, the review queue shape and the CHECK lists of 0022.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents::{self, DelegationInput, PolicyInput};
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::AuthContext;
use oxledger::auth::SessionPolicy;
use oxledger::changesets::{self, ApproveInput, SubmitResult, faults};
use oxledger::commands::{self, CommandResult};
use oxledger::describe;
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::review_queue;
use oxledger::service;
use oxledger::triage::{self, ListQuery, TriageListParams, TriageSettingsPut};
use oxledger::undo::{self, UndoInput};
use oxledger::views::{self, PreviewInput};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

struct T {
    w: World,
    /// Triager: triage_issues, manage_project, workflow new→doing, new→done.
    tri: Uuid,
    /// triage_issues but no workflow transition to done (T-M1-25).
    tri_open: Uuid,
    dave: Uuid,
    erin: Uuid,
    frank: Uuid,
    bob_ctx: AuthContext,
    carol_ctx: AuthContext,
    dave_ctx: AuthContext,
    erin_ctx: AuthContext,
    frank_ctx: AuthContext,
    /// A project only bob is in (invisible to alice).
    hidden: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> T {
    let w = World::build(pool).await;
    let tri = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Triager','all') RETURNING id",
    )
    .await;
    grant(
        pool,
        tri,
        &[
            "view_issues",
            "add_issues",
            "edit_issues",
            "add_issue_notes",
            "manage_issue_relations",
            "view_private_notes",
            "set_notes_private",
            "triage_issues",
            "manage_project",
        ],
    )
    .await;
    let tri_open = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('TriageOpenOnly','all') RETURNING id",
    )
    .await;
    grant(
        pool,
        tri_open,
        &[
            "view_issues",
            "edit_issues",
            "add_issue_notes",
            "triage_issues",
        ],
    )
    .await;
    let viewer = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(pool, viewer, &["view_issues"]).await;
    for (role, to) in [(tri, w.doing), (tri, w.done), (tri_open, w.doing)] {
        sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
            .bind(w.bug)
            .bind(role)
            .bind(w.new)
            .bind(to)
            .execute(pool)
            .await
            .unwrap();
    }
    let dave = oxledger::auth::create_user(pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let erin = oxledger::auth::create_user(pool, "erin", "erin", PW, false)
        .await
        .unwrap();
    let frank = oxledger::auth::create_user(pool, "frank", "frank", PW, false)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_roles SELECT id, $3 FROM members WHERE project_id = $1 AND principal_id = $2",
    )
    .bind(w.web)
    .bind(w.alice)
    .bind(tri)
    .execute(pool)
    .await
    .unwrap();
    member(pool, w.web, dave, tri).await;
    member(pool, w.web, erin, viewer).await;
    member(pool, w.web, frank, tri_open).await;
    let hidden = id(
        pool,
        "INSERT INTO projects (key, name) VALUES ('HID','Hidden') RETURNING id",
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO enabled_modules VALUES ('{hidden}', 'issue_tracking');
             INSERT INTO project_trackers VALUES ('{hidden}', '{}');",
            w.bug
        ),
    )
    .await;
    member(pool, hidden, w.bob, w.dev).await;
    let bob_ctx = sign_in(pool, &w.policy, "bob").await;
    let carol_ctx = sign_in(pool, &w.policy, "carol").await;
    let dave_ctx = sign_in(pool, &w.policy, "dave").await;
    let erin_ctx = sign_in(pool, &w.policy, "erin").await;
    let frank_ctx = sign_in(pool, &w.policy, "frank").await;
    T {
        w,
        tri,
        tri_open,
        dave,
        erin,
        frank,
        bob_ctx,
        carol_ctx,
        dave_ctx,
        erin_ctx,
        frank_ctx,
        hidden,
    }
}

async fn enable(t: &T, project: &str, rule: &str) {
    let cur = triage::settings(&t.w.app, t.w.alice_ctx, project)
        .await
        .unwrap();
    triage::put_settings(
        &t.w.app,
        t.w.alice_ctx,
        Uuid::now_v7(),
        project,
        &TriageSettingsPut {
            lock_version: cur.lock_version,
            enabled: true,
            entry_rule: rule.into(),
            responsible_user_id: Some(t.dave),
            decline_status_id: Some(t.w.done),
        },
    )
    .await
    .unwrap();
}

async fn lv(w: &World, issue: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// (state, self_triage, same_sponsor), or None outside Triage.
async fn tstate(w: &World, issue: Uuid) -> Option<(String, bool, bool)> {
    sqlx::query_as("SELECT state, self_triage, same_sponsor FROM issue_triage WHERE issue_id = $1")
        .bind(issue)
        .fetch_optional(&w.pool)
        .await
        .unwrap()
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "triage", "ops": ops, "preconditions": pre})
}

fn pre(i: Uuid, lv: i32) -> Value {
    json!({"target": tgt(i), "lock_version": lv})
}

fn decide(i: Uuid, decision: &str) -> Value {
    json!({"op_id": op(), "kind": "triage_decide", "target": tgt(i), "decision": decision})
}

fn transition(i: Uuid, to: Uuid) -> Value {
    json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to})
}

fn note(i: Uuid, body: &str, private: bool) -> Value {
    json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body, "is_private": private})
}

async fn cmd(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

async fn create_as(w: &World, who: AuthContext, project: Uuid, subject: &str) -> Uuid {
    let r = cmd(
        w,
        who,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                        "tracker_id": w.bug, "fields": {"subject": subject}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    r.effects[0].issue_id
}

/// The per-agent write rates (quota.rs) are not what this file tests.
async fn clear_rates(w: &World) {
    exec(&w.pool, "DELETE FROM agent_write_log").await;
}

async fn agent_create(w: &World, agent: AuthContext, project: Uuid, subject: &str) -> Uuid {
    clear_rates(w).await;
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": project,
                        "tracker_id": w.bug, "fields": {"subject": subject}}),
            ],
            vec![],
        ),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    match r {
        SubmitResult::Applied { effects, .. } => effects[0].issue_id,
        other => panic!("expected applied, got {other:?}"),
    }
}

async fn submit(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<SubmitResult> {
    clear_rates(w).await;
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN).await
}

fn applied(r: &SubmitResult) -> Uuid {
    match r {
        SubmitResult::Applied { execution_id, .. } => *execution_id,
        other => panic!("expected applied, got {other:?}"),
    }
}

fn details(e: &anyhow::Error) -> Value {
    classify(e)
        .and_then(|a| a.details.clone())
        .unwrap_or(Value::Null)
}

async fn events(w: &World, issue: Uuid, kind: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM issue_events WHERE issue_id = $1 AND kind = $2 ORDER BY seq",
    )
    .bind(issue)
    .bind(kind)
    .fetch_all(&w.pool)
    .await
    .unwrap()
}

fn modes(c: &[(&str, PolicyMode)]) -> PolicyInput {
    PolicyInput {
        commands: c
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
        fields: BTreeMap::new(),
    }
}

/// A new delegation of `project` to `client` that includes triage_decide
/// (§4.3: direct by default).
async fn delegate_triage(w: &World, client: Uuid, project: Uuid) -> agents::DelegationView {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: client,
            project_id: project,
            proposal_ops: [
                "create_issue",
                "update_fields",
                "transition",
                "add_note",
                "duplicate_of",
                "triage_decide",
            ]
            .map(String::from)
            .to_vec(),
            writable_fields: ["subject", "description"].map(String::from).to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap()
}

async fn inbox(w: &World, who: AuthContext, project: &str, q: ListQuery) -> Vec<String> {
    triage::project_inbox(&w.app, who, project, &q)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.key)
        .collect()
}

async fn key(w: &World, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn m1_3_triage_people() {
    let pool = test_pool().await;
    faults::clear();
    let t = setup(&pool).await;
    let w = &t.w;
    let a = w.alice_ctx;

    // ------------------------------------------------ settings (§3.4)
    let s = triage::settings(&w.app, t.bob_ctx, "WEB").await.unwrap();
    assert!(!s.enabled && !s.can_manage);
    assert_eq!(s.entry_rule, "creator_without_triage");
    let put = |lock_version, enabled, rule: &str, resp: Option<Uuid>, decline: Option<Uuid>| {
        TriageSettingsPut {
            lock_version,
            enabled,
            entry_rule: rule.into(),
            responsible_user_id: resp,
            decline_status_id: decline,
        }
    };
    // Not a manager: forbidden_op; a project bob cannot see: not_found.
    let e = triage::put_settings(
        &w.app,
        t.bob_ctx,
        Uuid::now_v7(),
        "WEB",
        &put(0, true, "all_new", None, Some(w.done)),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "HID",
        &put(0, true, "all_new", None, Some(w.done)),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    for (p, k) in [
        (
            put(0, true, "all_new", None, None),
            "triage_decline_status_required",
        ),
        (
            put(0, true, "all_new", None, Some(w.doing)),
            "triage_decline_status_invalid",
        ),
        (
            put(0, true, "everyone", None, Some(w.done)),
            "triage_entry_rule",
        ),
        // erin's role is not assignable? It is; an agent or unknown id is not.
        (
            put(0, true, "all_new", Some(Uuid::now_v7()), Some(w.done)),
            "triage_responsible_invalid",
        ),
    ] {
        let e = triage::put_settings(&w.app, a, Uuid::now_v7(), "WEB", &p)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue, "{k}");
        assert_eq!(details(&e)["kind"], k);
    }
    // Enabling warns about roles with triage_issues and no transition to
    // the decline status (TriageOpenOnly), and does not block.
    let key_op = Uuid::now_v7();
    let p = put(
        0,
        true,
        "creator_without_triage",
        Some(t.dave),
        Some(w.done),
    );
    let s = triage::put_settings(&w.app, a, key_op, "WEB", &p)
        .await
        .unwrap();
    assert!(s.enabled && s.can_manage);
    assert_eq!(s.lock_version, 1);
    assert_eq!(s.responsible.as_ref().unwrap().id, t.dave);
    assert_eq!(s.warnings.len(), 1);
    assert_eq!(s.warnings[0].kind, "decline_transition_missing");
    assert_eq!(
        s.warnings[0].roles.iter().map(|r| r.id).collect::<Vec<_>>(),
        [t.tri_open]
    );
    // Same key, same body: the current settings, nothing written again.
    let s2 = triage::put_settings(&w.app, a, key_op, "WEB", &p)
        .await
        .unwrap();
    assert_eq!(s2.lock_version, 1);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM project_setting_events WHERE kind = 'triage_settings_updated'"
        )
        .await,
        1
    );
    // Stale lock_version: conflict with the current settings.
    let e = triage::put_settings(&w.app, a, Uuid::now_v7(), "WEB", &p)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    assert_eq!(details(&e)["current_lock_version"], 1);

    // ------------------------------------------------ T-M1-21: entry rule
    let by_alice = create_as(w, a, w.web, "alice files").await;
    let by_bob = create_as(w, t.bob_ctx, w.web, "bob files").await;
    assert_eq!(tstate(w, by_alice).await, None);
    assert_eq!(tstate(w, by_bob).await.unwrap().0, "pending");
    let created = |v: &Vec<Value>| v[0].get("triage").cloned();
    assert_eq!(created(&events(w, by_alice, "created").await), None);
    assert_eq!(
        created(&events(w, by_bob, "created").await),
        Some(json!("pending"))
    );
    // describe's creation.enters_triage follows the caller.
    let ds = describe::tracker_schema(&w.app, a, "WEB", "Bug")
        .await
        .unwrap();
    assert!(!ds.creation.enters_triage);
    let ds = describe::tracker_schema(&w.app, t.bob_ctx, "WEB", "Bug")
        .await
        .unwrap();
    assert!(ds.creation.enters_triage);
    // AUTH has Triage off: nothing enters.
    let in_auth = create_as(w, t.bob_ctx, w.auth_p, "auth").await;
    assert_eq!(tstate(w, in_auth).await, None);

    // ------------------------------------------------ issue detail, filter, view column
    let d = service::get(&w.app, t.bob_ctx, &key(w, by_bob).await)
        .await
        .unwrap();
    let tr = d.triage.unwrap();
    assert_eq!((tr.state.as_str(), tr.can_decide), ("pending", false));
    let d = service::get(&w.app, a, &key(w, by_bob).await)
        .await
        .unwrap();
    assert!(d.triage.unwrap().can_decide);
    assert!(
        service::get(&w.app, a, &key(w, by_alice).await)
            .await
            .unwrap()
            .triage
            .is_none()
    );
    let run = |where_: Value| {
        let w = &t.w;
        async move {
            let r = views::preview(
                &w.app,
                a,
                &PreviewInput {
                    def: json!({"kind": "list", "columns": ["key", "triage"],
                                "query": {"where": where_}}),
                    scope: None,
                    project_id: Some("WEB".into()),
                    cursor: None,
                    limit: None,
                    lane: None,
                    swimlane: None,
                    override_: None,
                },
            )
            .await
            .unwrap();
            serde_json::to_value(r).unwrap()
        }
    };
    let r = run(json!({"field": "triage", "op": "eq", "value": "pending"})).await;
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{r}");
    assert_eq!(items[0]["id"], json!(by_bob));
    assert_eq!(items[0]["values"]["triage"], "pending");
    let r = run(json!({"field": "triage", "op": "in", "value": ["none"]})).await;
    let ids: Vec<&Value> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["id"])
        .collect();
    assert!(ids.contains(&&json!(by_alice)) && !ids.contains(&&json!(by_bob)));
    assert_eq!(
        r["items"].as_array().unwrap()[0]["values"]["triage"],
        Value::Null
    );
    let e = views::preview(
        &w.app,
        a,
        &PreviewInput {
            def: json!({"kind": "list", "query": {"where":
                {"field": "triage", "op": "eq", "value": "maybe"}}}),
            scope: None,
            project_id: Some("WEB".into()),
            cursor: None,
            limit: None,
            lane: None,
            swimlane: None,
            override_: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidFilter);

    // ------------------------------------------------ T-M1-22: the four decisions
    let i_acc = create_as(w, t.bob_ctx, w.web, "accept me").await;
    let i_dec = create_as(w, t.bob_ctx, w.web, "decline me").await;
    let i_dup = create_as(w, t.bob_ctx, w.web, "duplicate me").await;
    let i_orig = create_as(w, a, w.web, "the original").await;
    let i_snz = create_as(w, t.bob_ctx, w.web, "snooze me").await;
    // accept with a field change, a transition and a note: lock_version +1 once.
    let before = lv(w, i_acc).await;
    cmd(
        w,
        a,
        &doc(
            vec![
                decide(i_acc, "accept"),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i_acc), "set": {"subject": "accepted"}}),
                transition(i_acc, w.doing),
                note(i_acc, "looks real", false),
            ],
            vec![pre(i_acc, before)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(lv(w, i_acc).await, before + 1);
    assert_eq!(
        tstate(w, i_acc).await,
        Some(("accepted".into(), false, false))
    );
    let ev = events(w, i_acc, "triage_changed").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(
        (&ev[0]["old"], &ev[0]["new"], &ev[0]["self_triage"]),
        (&json!("pending"), &json!("accepted"), &json!(false))
    );
    // decline: to the closed status.
    let before = lv(w, i_dec).await;
    cmd(
        w,
        a,
        &doc(
            vec![decide(i_dec, "decline"), transition(i_dec, w.done)],
            vec![pre(i_dec, before)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(lv(w, i_dec).await, before + 1);
    assert_eq!(tstate(w, i_dec).await.unwrap().0, "declined");
    // duplicate: the relation comes with the decision; both ends +1.
    let (b1, b2) = (lv(w, i_dup).await, lv(w, i_orig).await);
    let mut dd = decide(i_dup, "duplicate");
    dd["duplicate_of"] = tgt(i_orig);
    cmd(
        w,
        a,
        &doc(
            vec![
                dd.clone(),
                transition(i_dup, w.done),
                note(i_dup, "same", false),
            ],
            vec![pre(i_dup, b1), pre(i_orig, b2)],
        ),
    )
    .await
    .unwrap();
    assert_eq!((lv(w, i_dup).await, lv(w, i_orig).await), (b1 + 1, b2 + 1));
    assert_eq!(tstate(w, i_dup).await.unwrap().0, "duplicate");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_duplicates WHERE source_issue_id = '{i_dup}' AND target_issue_id = '{i_orig}'"
            )
        )
        .await,
        1
    );
    // snooze: still pending, out of the default inbox.
    let before = lv(w, i_snz).await;
    let mut sz = decide(i_snz, "snooze");
    sz["snooze_until"] = json!((chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339());
    cmd(w, a, &doc(vec![sz], vec![pre(i_snz, before)]))
        .await
        .unwrap();
    assert_eq!(lv(w, i_snz).await, before + 1);
    assert_eq!(tstate(w, i_snz).await.unwrap().0, "pending");
    let ev = events(w, i_snz, "triage_changed").await;
    assert_eq!(
        (&ev[0]["old"], &ev[0]["new"]),
        (&json!("pending"), &json!("pending"))
    );
    assert!(ev[0]["snoozed_until"].is_string());
    let k_snz = key(w, i_snz).await;
    assert!(
        !inbox(w, a, "WEB", ListQuery::default())
            .await
            .contains(&k_snz)
    );
    let with = ListQuery {
        include_snoozed: true,
        ..Default::default()
    };
    let l = triage::project_inbox(&w.app, a, "WEB", &with)
        .await
        .unwrap();
    let it = l.items.iter().find(|i| i.key == k_snz).unwrap();
    assert!(it.snoozed_until.is_some());
    assert!(l.can_decide == Some(true));
    assert_eq!(
        l.settings
            .as_ref()
            .unwrap()
            .decline_status
            .as_ref()
            .unwrap()
            .id,
        w.done
    );
    // The filter value snoozed.
    let r = run(json!({"field": "triage", "op": "eq", "value": "snoozed"})).await;
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    // unsnooze, then unsnooze again: not snoozed any more.
    let l1 = lv(w, i_snz).await;
    cmd(
        w,
        a,
        &doc(vec![decide(i_snz, "unsnooze")], vec![pre(i_snz, l1)]),
    )
    .await
    .unwrap();
    assert!(
        inbox(w, a, "WEB", ListQuery::default())
            .await
            .contains(&k_snz)
    );
    let e = cmd(
        w,
        a,
        &doc(vec![decide(i_snz, "unsnooze")], vec![pre(i_snz, l1 + 1)]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::TriageStateInvalid);
    assert_eq!(details(&e)["state"], "pending");

    // ------------------------------------------------ T-M1-23: structure
    let i23 = create_as(w, t.bob_ctx, w.web, "structure").await;
    let l23 = lv(w, i23).await;
    let bad = |ops: Vec<Value>, pres: Vec<Value>| doc(ops, pres);
    for (d, why) in [
        (
            bad(
                vec![
                    {
                        let mut x = decide(i23, "duplicate");
                        x["duplicate_of"] = tgt(i_orig);
                        x
                    },
                    transition(i23, w.done),
                    json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(i23), "target": tgt(i_orig)}),
                ],
                vec![pre(i23, l23), pre(i_orig, lv(w, i_orig).await)],
            ),
            "duplicate with a duplicate_of op",
        ),
        (
            bad(
                vec![decide(i23, "accept"), decide(i23, "snooze")],
                vec![pre(i23, l23)],
            ),
            "two triage_decide",
        ),
        (
            bad(
                vec![
                    {
                        let mut x = decide(i23, "snooze");
                        x["snooze_until"] = json!("2099-01-01T00:00:00Z");
                        x
                    },
                    transition(i23, w.doing),
                ],
                vec![pre(i23, l23)],
            ),
            "snooze with a transition",
        ),
        (
            bad(
                vec![decide(i23, "duplicate"), transition(i23, w.done)],
                vec![pre(i23, l23)],
            ),
            "duplicate without duplicate_of",
        ),
        (
            bad(
                vec![decide(i23, "accept"), note(i_orig, "elsewhere", false)],
                vec![pre(i23, l23), pre(i_orig, lv(w, i_orig).await)],
            ),
            "an op on another issue",
        ),
        (
            bad(
                vec![{
                    let mut x = decide(i23, "accept");
                    x["self_triage"] = json!(false);
                    x
                }],
                vec![pre(i23, l23)],
            ),
            "self_triage is not a parameter (T-D11)",
        ),
    ] {
        let e = cmd(w, a, &d).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "{why}: {e:#}");
    }
    // R27: a transition to an open status, or a decline without a
    // transition of an open issue, is a semantic error.
    let e = cmd(
        w,
        a,
        &doc(vec![decide(i23, "decline")], vec![pre(i23, l23)]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "triage_requires_close");
    let e = cmd(
        w,
        a,
        &doc(
            vec![decide(i23, "decline"), transition(i23, w.doing)],
            vec![pre(i23, l23)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "triage_requires_close");
    // Snooze range: 1 hour to 90 days.
    for until in [
        chrono::Utc::now() + chrono::Duration::minutes(10),
        chrono::Utc::now() + chrono::Duration::days(91),
    ] {
        let mut x = decide(i23, "snooze");
        x["snooze_until"] = json!(until.to_rfc3339());
        let e = cmd(w, a, &doc(vec![x], vec![pre(i23, l23)]))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue);
        assert_eq!(details(&e)["kind"], "snooze_range");
    }
    assert_eq!(lv(w, i23).await, l23, "nothing written");

    // ------------------------------------------------ T-M1-24: two accepts at once
    let l24 = lv(w, i23).await;
    let d24 = doc(vec![decide(i23, "accept")], vec![pre(i23, l24)]);
    let (r1, r2) = common::wait_for("two racing decisions", common::WAIT_TIMEOUT, async {
        tokio::join!(cmd(w, a, &d24), cmd(w, t.dave_ctx, &d24))
    })
    .await;
    let err = match (r1, r2) {
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => e,
        other => panic!("one wins: {other:?}"),
    };
    assert_eq!(code(&err), ErrorCode::Conflict);
    let e = cmd(
        w,
        t.dave_ctx,
        &doc(vec![decide(i23, "accept")], vec![pre(i23, l24 + 1)]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::TriageStateInvalid);
    assert_eq!(details(&e)["state"], "accepted");
    // An issue that never entered: state none.
    let e = cmd(
        w,
        a,
        &doc(
            vec![decide(by_alice, "accept")],
            vec![pre(by_alice, lv(w, by_alice).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::TriageStateInvalid);
    assert_eq!(details(&e)["state"], "none");

    // ------------------------------------------------ T-M1-25: permission, workflow
    let i25 = create_as(w, t.bob_ctx, w.web, "perm").await;
    let l25 = lv(w, i25).await;
    let e = cmd(
        w,
        t.bob_ctx,
        &doc(vec![decide(i25, "accept")], vec![pre(i25, l25)]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = cmd(
        w,
        t.frank_ctx,
        &doc(
            vec![decide(i25, "decline"), transition(i25, w.done)],
            vec![pre(i25, l25)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidTransition, "{e:#}");
    // frank may accept (no transition needed).
    cmd(
        w,
        t.frank_ctx,
        &doc(vec![decide(i25, "accept")], vec![pre(i25, l25)]),
    )
    .await
    .unwrap();

    // ------------------------------------------------ T-M1-26: a private note ends a snooze
    let i26 = create_as(w, t.bob_ctx, w.web, "snooze then note").await;
    let mut sz = decide(i26, "snooze");
    sz["snooze_until"] = json!((chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339());
    cmd(w, a, &doc(vec![sz], vec![pre(i26, lv(w, i26).await)]))
        .await
        .unwrap();
    let k26 = key(w, i26).await;
    assert!(
        !inbox(w, a, "WEB", ListQuery::default())
            .await
            .contains(&k26)
    );
    // The same transaction timestamp: a write after the snooze moves updated_at.
    sqlx::query("SELECT pg_sleep(0.01)")
        .execute(&pool)
        .await
        .unwrap();
    cmd(
        w,
        t.dave_ctx,
        &doc(
            vec![note(i26, "secret detail", true)],
            vec![pre(i26, lv(w, i26).await)],
        ),
    )
    .await
    .unwrap();
    let l = triage::project_inbox(&w.app, t.erin_ctx, "WEB", &ListQuery::default())
        .await
        .unwrap();
    let it = l
        .items
        .iter()
        .find(|i| i.key == k26)
        .expect("back in the inbox");
    assert!(it.snoozed_until.is_none());
    assert!(!serde_json::to_string(&l).unwrap().contains("secret detail"));
    assert_eq!(l.can_decide, Some(false));

    // ------------------------------------------------ T-M1-27: duplicate targets
    let i27 = create_as(w, t.bob_ctx, w.web, "dup target checks").await;
    let l27 = lv(w, i27).await;
    let hidden_issue = create_as(w, t.bob_ctx, t.hidden, "hidden").await;
    let mut x = decide(i27, "duplicate");
    x["duplicate_of"] = tgt(hidden_issue);
    let e = cmd(
        w,
        a,
        &doc(
            vec![x, transition(i27, w.done)],
            vec![pre(i27, l27), pre(hidden_issue, lv(w, hidden_issue).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let mut x = decide(i27, "duplicate");
    x["duplicate_of"] = tgt(in_auth);
    let e = cmd(
        w,
        a,
        &doc(
            vec![x, transition(i27, w.done)],
            vec![pre(i27, l27), pre(in_auth, lv(w, in_auth).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RelationNotAllowed);
    assert_eq!(details(&e)["reason"], "cross_project");

    // ------------------------------------------------ T-M1-30: visibility, /triage, counts
    let private = {
        let r = cmd(
            w,
            t.bob_ctx,
            &doc(
                vec![json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                            "tracker_id": w.bug, "fields": {"subject": "private one", "is_private": true}})],
                vec![],
            ),
        )
        .await
        .unwrap();
        r.effects[0].issue_id
    };
    let kp = key(w, private).await;
    assert!(inbox(w, a, "WEB", ListQuery::default()).await.contains(&kp));
    assert!(
        !inbox(w, t.erin_ctx, "WEB", ListQuery::default())
            .await
            .contains(&kp),
        "erin cannot see private issues"
    );
    // AUTH has Triage on too, but alice has no triage_issues there.
    exec(
        &pool,
        &format!(
            "UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}' WHERE project_id = '{}'",
            w.done, w.auth_p
        ),
    )
    .await;
    let in_auth2 = create_as(w, t.bob_ctx, w.auth_p, "auth pending").await;
    assert_eq!(tstate(w, in_auth2).await.unwrap().0, "pending");
    let mine = triage::my_inbox(&w.app, a, &ListQuery::default())
        .await
        .unwrap();
    assert!(
        mine.items
            .iter()
            .all(|i| i.project.as_ref().unwrap().key == "WEB")
    );
    assert!(mine.items.iter().any(|i| i.key == kp));
    let bobs = triage::my_inbox(&w.app, t.bob_ctx, &ListQuery::default())
        .await
        .unwrap();
    assert!(bobs.items.is_empty(), "bob triages nowhere");
    // Pages: oldest entered first, cursor continues.
    let all = inbox(w, a, "WEB", ListQuery::default()).await;
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let l = triage::project_inbox(
            &w.app,
            a,
            "WEB",
            &ListQuery {
                limit: Some(2),
                cursor: cursor.take(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        paged.extend(l.items.into_iter().map(|i| i.key));
        match l.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, all);
    // A cursor of another query is refused.
    let c = triage::project_inbox(
        &w.app,
        a,
        "WEB",
        &ListQuery {
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .next_cursor
    .unwrap();
    let e = triage::project_inbox(
        &w.app,
        a,
        "WEB",
        &ListQuery {
            limit: Some(1),
            include_snoozed: true,
            cursor: Some(c),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    // R28: counts only count what the caller sees, and only where they triage.
    let c = triage::counts(&w.app, a).await.unwrap();
    assert_eq!(c.projects.len(), 1);
    assert_eq!(c.projects[0].project.key, "WEB");
    assert_eq!(c.projects[0].count as usize, all.len());
    assert!(!c.projects[0].more);
    assert!(
        triage::counts(&w.app, t.erin_ctx)
            .await
            .unwrap()
            .projects
            .is_empty()
    );
    exec(
        &pool,
        &format!(
            "UPDATE issues SET is_private = true WHERE id <> '{private}' AND project_id = '{}'",
            w.web
        ),
    )
    .await;
    // dave sees everything (visibility all): unchanged.
    let cd = triage::counts(&w.app, t.dave_ctx).await.unwrap();
    assert_eq!(cd.projects[0].count as usize, all.len());
    exec(
        &pool,
        &format!(
            "UPDATE issues SET is_private = false WHERE id <> '{private}' AND project_id = '{}'",
            w.web
        ),
    )
    .await;
    // Triage off: the inbox is empty, the pending rows stay and come back.
    let cur = triage::settings(&w.app, a, "WEB").await.unwrap();
    triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(
            cur.lock_version,
            false,
            "creator_without_triage",
            None,
            None,
        ),
    )
    .await
    .unwrap();
    assert!(inbox(w, a, "WEB", ListQuery::default()).await.is_empty());
    let none_created = create_as(w, t.bob_ctx, w.web, "while off").await;
    assert_eq!(tstate(w, none_created).await, None);
    enable(&t, "WEB", "creator_without_triage").await;
    assert_eq!(inbox(w, a, "WEB", ListQuery::default()).await, all);
    // all_new: a triager's own issue enters too.
    let cur = triage::settings(&w.app, a, "WEB").await.unwrap();
    triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(cur.lock_version, true, "all_new", None, Some(w.done)),
    )
    .await
    .unwrap();
    let all_new = create_as(w, a, w.web, "everyone enters").await;
    assert_eq!(tstate(w, all_new).await.unwrap().0, "pending");
    // A person deciding their own issue: no A5 flag (that is for agents).
    cmd(
        w,
        a,
        &doc(
            vec![decide(all_new, "accept")],
            vec![pre(all_new, lv(w, all_new).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        tstate(w, all_new).await,
        Some(("accepted".into(), false, false))
    );
    assert_eq!(
        events(w, all_new, "triage_changed").await[0]["self_triage"],
        json!(false)
    );

    // ------------------------------------------------ review queue shape (§4.3)
    for (d, tickable) in [
        ("accept", true),
        ("snooze", true),
        ("decline", false),
        ("duplicate", false),
        ("unsnooze", false),
    ] {
        let body = json!({"ops": [{"kind": "triage_decide", "target": tgt(i23), "decision": d}]});
        assert_eq!(
            review_queue::shape_reasons(&body).is_empty(),
            tickable,
            "{d}"
        );
    }
    let body = json!({"ops": [decide(i23, "accept"), note(i23, "x", false)]});
    assert!(
        !review_queue::shape_reasons(&body).is_empty(),
        "a note is never tickable"
    );
    let _ = (t.erin, t.tri, t.carol_ctx);
}

#[tokio::test]
async fn m1_3_triage_agents() {
    let pool = test_pool().await;
    faults::clear();
    let t = setup(&pool).await;
    let w = &t.w;
    let a = w.alice_ctx;
    enable(&t, "WEB", "creator_without_triage").await;
    exec(
        &pool,
        &format!(
            "UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}' WHERE project_id = '{}'",
            w.done, w.auth_p
        ),
    )
    .await;

    // ------------------------------------------------ T-M1-28: an existing delegation
    // (created without triage_decide) is not extended.
    let old = w.delegate(w.web).await.unwrap();
    let p = agents::delegation_policies(&pool, &a, old.id)
        .await
        .unwrap();
    assert!(p.available_commands.contains(&"triage_decide".to_string()));
    assert!(!p.commands.contains_key("triage_decide"));
    let (_, x_old) = w.token(old.context_id).await;
    // Agents' issues always enter (§3.4), whatever the rule.
    let by_x0 = agent_create(w, x_old, w.web, "agent filed").await;
    assert_eq!(tstate(w, by_x0).await.unwrap().0, "pending");
    assert_eq!(
        events(w, by_x0, "created").await[0]["triage"],
        json!("pending")
    );
    let e = submit(
        w,
        x_old,
        &doc(
            vec![decide(by_x0, "accept")],
            vec![pre(by_x0, lv(w, by_x0).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    // Nor does its triage_list show anything (no project given: every
    // project it may triage, i.e. none).
    let l = triage::agent_list(&w.app, x_old, TriageListParams::default())
        .await
        .unwrap();
    assert!(l.items.is_empty(), "{l:?}");
    // The delegation page offers it; adding it makes it work.
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &a,
        old.id,
        modes(&[("triage_decide", Direct)]),
    )
    .await
    .unwrap();
    applied(
        &submit(
            w,
            x_old,
            &doc(
                vec![decide(by_x0, "accept")],
                vec![pre(by_x0, lv(w, by_x0).await)],
            ),
        )
        .await
        .unwrap(),
    );
    agents::revoke_delegation(&pool, &a, old.id).await.unwrap();

    // ------------------------------------------------ a new delegation: direct by default
    let d = delegate_triage(w, w.client, w.web).await;
    let p = agents::delegation_policies(&pool, &a, d.id).await.unwrap();
    assert_eq!(p.commands.get("triage_decide"), Some(&Direct));
    let (_, x) = w.token(d.context_id).await;

    // T-M1-30a (t,f): X decides its own issue.
    let by_x = agent_create(w, x, w.web, "x filed").await;
    let ex = applied(
        &submit(
            w,
            x,
            &doc(
                vec![decide(by_x, "accept")],
                vec![pre(by_x, lv(w, by_x).await)],
            ),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        tstate(w, by_x).await,
        Some(("accepted".into(), true, false))
    );
    let ev = events(w, by_x, "triage_changed").await;
    assert_eq!(
        (&ev[0]["self_triage"], &ev[0]["same_sponsor"]),
        (&json!(true), &json!(false))
    );
    // The A5 filter field.
    let r = views::preview(
        &w.app,
        a,
        &PreviewInput {
            def: json!({"kind": "list", "columns": ["key", "triage"], "query": {"where":
                {"field": "triage_self_decided", "op": "eq", "value": true}}}),
            scope: None,
            project_id: Some("WEB".into()),
            cursor: None,
            limit: None,
            lane: None,
            swimlane: None,
            override_: None,
        },
    )
    .await
    .unwrap();
    let r = serde_json::to_value(r).unwrap();
    // by_x0 too: the same agent (client and sponsor) filed and accepted it.
    let ids: Vec<&Value> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["id"])
        .collect();
    assert_eq!(ids.len(), 2, "{r}");
    assert!(ids.contains(&&json!(by_x)) && ids.contains(&&json!(by_x0)));

    // (f,t): Y, another agent of the same person, decides X's issue.
    let client_y = agents::create_agent_client(&pool, "Second Agent", "policy-2026-01")
        .await
        .unwrap();
    agents::set_agent_client_enabled(&pool, "Second Agent", true)
        .await
        .unwrap();
    let dy = delegate_triage(w, client_y, w.web).await;
    let (_, y) = w.token(dy.context_id).await;
    let by_x2 = agent_create(w, x, w.web, "x filed again").await;
    applied(
        &submit(
            w,
            y,
            &doc(
                vec![decide(by_x2, "decline"), transition(by_x2, w.done)],
                vec![pre(by_x2, lv(w, by_x2).await)],
            ),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        tstate(w, by_x2).await,
        Some(("declined".into(), false, true))
    );
    // (f,f): a person decides X's issue.
    let by_x3 = agent_create(w, x, w.web, "x filed thrice").await;
    cmd(
        w,
        t.dave_ctx,
        &doc(
            vec![decide(by_x3, "accept")],
            vec![pre(by_x3, lv(w, by_x3).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        tstate(w, by_x3).await,
        Some(("accepted".into(), false, false))
    );
    // Snooze never sets the flags.
    let by_x4 = agent_create(w, x, w.web, "x snoozes").await;
    let mut sz = decide(by_x4, "snooze");
    sz["snooze_until"] = json!((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339());
    applied(
        &submit(w, x, &doc(vec![sz], vec![pre(by_x4, lv(w, by_x4).await)]))
            .await
            .unwrap(),
    );
    assert_eq!(
        tstate(w, by_x4).await,
        Some(("pending".into(), false, false))
    );
    // T-D11: the flags are not parameters.
    let mut bad = decide(by_x4, "accept");
    bad["self_triage"] = json!(false);
    let e = submit(w, x, &doc(vec![bad], vec![pre(by_x4, lv(w, by_x4).await)]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);

    // ------------------------------------------------ T-M1-30b: recent decisions
    let decided = |s: &str| ListQuery {
        decided: true,
        self_triage: Some(s.into()),
        ..Default::default()
    };
    let only = triage::project_inbox(&w.app, a, "WEB", &decided("only"))
        .await
        .unwrap();
    let keys: Vec<&str> = only.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert!(keys.contains(&key(w, by_x).await.as_str()));
    assert!(keys.contains(&key(w, by_x2).await.as_str()));
    let first = only.items.iter().find(|i| i.id == by_x2).unwrap();
    assert_eq!(first.decision.as_deref(), Some("declined"));
    assert_eq!(first.same_sponsor, Some(true));
    let by = first.decided_by.as_ref().unwrap();
    assert_eq!(
        (by.kind.as_str(), by.on_behalf_of.as_ref().unwrap().id),
        ("agent", w.alice)
    );
    // Newest first.
    assert!(only.items[0].decided_at >= only.items[1].decided_at);
    let excl = triage::project_inbox(&w.app, a, "WEB", &decided("exclude"))
        .await
        .unwrap();
    assert!(
        excl.items
            .iter()
            .all(|i| i.self_triage == Some(false) && i.same_sponsor == Some(false))
    );
    let users = triage::project_inbox(
        &w.app,
        a,
        "WEB",
        &ListQuery {
            decided: true,
            decided_by_kind: Some("user".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        users
            .items
            .iter()
            .all(|i| i.decided_by.as_ref().unwrap().kind == "user")
    );
    assert!(users.items.iter().any(|i| i.id == by_x3));
    // A self-triaged issue made private afterwards: erin (default
    // visibility) does not see it in recent decisions; agents never read
    // private issues (§3.5), so the agent cannot decide it either.
    let by_xp = agent_create(w, x, w.web, "x later private").await;
    applied(
        &submit(
            w,
            x,
            &doc(
                vec![decide(by_xp, "accept")],
                vec![pre(by_xp, lv(w, by_xp).await)],
            ),
        )
        .await
        .unwrap(),
    );
    let e = triage::project_inbox(&w.app, a, "WEB", &decided("sometimes"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    // ------------------------------------------------ T-M1-28: review and off
    let by_x5 = agent_create(w, x, w.web, "x review").await;
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &a,
        d.id,
        modes(&[("triage_decide", Review)]),
    )
    .await
    .unwrap();
    let r = submit(
        w,
        x,
        &doc(
            vec![decide(by_x5, "accept")],
            vec![pre(by_x5, lv(w, by_x5).await)],
        ),
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::ReviewRequired { .. }), "{r:?}");
    assert_eq!(tstate(w, by_x5).await.unwrap().0, "pending");
    // D9: alice approves X's proposal for its own issue. The decider is X
    // (the proposal's author), so it is self-triage.
    let by_x7 = agent_create(w, x, w.web, "x proposes").await;
    let r = submit(
        w,
        x,
        &doc(
            vec![decide(by_x7, "accept")],
            vec![pre(by_x7, lv(w, by_x7).await)],
        ),
    )
    .await
    .unwrap();
    let cs = match r {
        SubmitResult::ReviewRequired { proposal, .. } => proposal.changeset_id,
        other => panic!("{other:?}"),
    };
    let view = changesets::get(&w.app, a, cs, None).await.unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        a,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version: 1,
            proposal_hash: view.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert!(o.execution.unwrap().error.is_none());
    assert_eq!(
        tstate(w, by_x7).await,
        Some(("accepted".into(), true, false))
    );
    let decider: Uuid =
        sqlx::query_scalar("SELECT decided_by FROM issue_triage WHERE issue_id = $1")
            .bind(by_x7)
            .fetch_one(&pool)
            .await
            .unwrap();
    let author: Uuid = sqlx::query_scalar("SELECT author_id FROM issues WHERE id = $1")
        .bind(by_x7)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(decider, author, "the agent decided");
    agents::set_delegation_policies(&pool, &w.policy, &a, d.id, modes(&[("triage_decide", Off)]))
        .await
        .unwrap();
    let e = submit(
        w,
        x,
        &doc(
            vec![decide(by_x5, "accept")],
            vec![pre(by_x5, lv(w, by_x5).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(details(&e)["reason"], "policy");
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &a,
        d.id,
        modes(&[("triage_decide", Direct)]),
    )
    .await
    .unwrap();
    // A duplicate decision also needs duplicate_of's policy (the stricter wins).
    agents::set_delegation_policies(&pool, &w.policy, &a, d.id, modes(&[("duplicate_of", Off)]))
        .await
        .unwrap();
    let mut dd = decide(by_x5, "duplicate");
    dd["duplicate_of"] = tgt(by_x);
    let e = submit(
        w,
        x,
        &doc(
            vec![dd, transition(by_x5, w.done)],
            vec![pre(by_x5, lv(w, by_x5).await), pre(by_x, lv(w, by_x).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    agents::set_delegation_policies(
        &pool,
        &w.policy,
        &a,
        d.id,
        modes(&[("duplicate_of", Direct)]),
    )
    .await
    .unwrap();

    // ------------------------------------------------ E4w: outside the delegation
    // AUTH is not delegated to X: its issue is not found.
    let in_auth = create_as(w, t.bob_ctx, w.auth_p, "auth issue").await;
    let e = submit(
        w,
        x,
        &doc(
            vec![decide(in_auth, "accept")],
            vec![pre(in_auth, lv(w, in_auth).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    // An issue in a project alice cannot see.
    exec(
        &pool,
        &format!("UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}' WHERE project_id = '{}'", w.done, t.hidden),
    )
    .await;
    let hid = create_as(w, t.bob_ctx, t.hidden, "hidden").await;
    let e = submit(w, x, &doc(vec![decide(hid, "accept")], vec![pre(hid, 0)]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // AUTH delegated, but alice has no triage_issues there: forbidden_op.
    let da = delegate_triage(w, client_y, w.auth_p).await;
    let (_, xa) = w.token(da.context_id).await;
    let e = submit(
        w,
        xa,
        &doc(
            vec![decide(in_auth, "accept")],
            vec![pre(in_auth, lv(w, in_auth).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");

    // ------------------------------------------------ MCP triage_list
    let by_x6 = agent_create(w, x, w.web, "x lists").await;
    let l = triage::agent_list(
        &w.app,
        x,
        TriageListParams {
            project: Some("WEB".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(l.items.iter().any(|i| i.id == by_x6));
    assert_eq!(l.can_decide, Some(true));
    let it = l.items.iter().find(|i| i.id == by_x6).unwrap();
    assert_eq!(it.author.kind, "agent");
    assert_eq!(it.author.on_behalf_of.as_ref().unwrap().id, w.alice);
    let e = triage::agent_list(
        &w.app,
        x,
        TriageListParams {
            project: Some("HID".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);

    // ------------------------------------------------ T-M1-29, T-M1-30c: undo
    // Undo X's self-triaged accept: back to pending, flag cleared; the
    // decision's own event keeps the flag.
    let pv = undo::preview(&w.app, a, ex).await.unwrap();
    assert_eq!(pv.triage.len(), 1, "{pv:?}");
    assert!(pv.triage[0].restorable, "{pv:?}");
    assert_eq!(
        (pv.triage[0].before.as_str(), pv.triage[0].after.as_str()),
        ("pending", "accepted")
    );
    let before = lv(w, by_x).await;
    let u = undo::apply(&w.app, a, Uuid::now_v7(), ex, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(u.triage_restored, 1);
    assert_eq!(
        tstate(w, by_x).await,
        Some(("pending".into(), false, false))
    );
    assert_eq!(lv(w, by_x).await, before + 1);
    let ev = events(w, by_x, "triage_changed").await;
    assert_eq!(ev.len(), 2);
    assert_eq!(
        ev[0]["self_triage"],
        json!(true),
        "the decision keeps its flag"
    );
    assert_eq!(
        (&ev[1]["old"], &ev[1]["new"]),
        (&json!("accepted"), &json!("pending"))
    );
    assert!(
        inbox(w, a, "WEB", ListQuery::default())
            .await
            .contains(&key(w, by_x).await)
    );
    // Undo of the undo restores the decision (flags included).
    let pv2 = undo::preview(&w.app, a, u.execution_id).await.unwrap();
    assert!(pv2.triage[0].restorable, "{pv2:?}");
    let uu = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        u.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(uu.triage_restored, 1);
    assert_eq!(
        tstate(w, by_x).await,
        Some(("accepted".into(), true, false))
    );
    // The state equals what the first decision wrote, but the last write is
    // the undo of the undo's: undoing the decision again is changed_since.
    let pv = undo::preview(&w.app, a, ex).await.unwrap();
    assert_eq!(
        pv.triage[0].reason.as_deref(),
        Some("changed_since"),
        "{pv:?}"
    );
    // skip_triage keeps the decision: with nothing else, nothing to undo.
    let by_x8 = agent_create(w, x, w.web, "x skips").await;
    let ex8 = applied(
        &submit(
            w,
            x,
            &doc(
                vec![decide(by_x8, "accept")],
                vec![pre(by_x8, lv(w, by_x8).await)],
            ),
        )
        .await
        .unwrap(),
    );
    let e = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        ex8,
        &UndoInput {
            skip_triage: vec![by_x8],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    assert_eq!(tstate(w, by_x8).await.unwrap().0, "accepted");
    // Someone decided after the agent: changed_since, nothing to undo.
    let ex4 = sqlx::query_scalar::<_, Uuid>(
        "SELECT command_execution_id FROM issue_events WHERE issue_id = $1 AND kind = 'triage_changed'",
    )
    .bind(by_x4)
    .fetch_one(&pool)
    .await
    .unwrap();
    cmd(
        w,
        t.dave_ctx,
        &doc(
            vec![decide(by_x4, "accept")],
            vec![pre(by_x4, lv(w, by_x4).await)],
        ),
    )
    .await
    .unwrap();
    let pv = undo::preview(&w.app, a, ex4).await.unwrap();
    assert_eq!(pv.triage[0].reason.as_deref(), Some("changed_since"));
    let e = undo::apply(&w.app, a, Uuid::now_v7(), ex4, &UndoInput::default())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    assert_eq!(tstate(w, by_x4).await.unwrap().0, "accepted");
    // Last: making an issue private ends the agent's context (epoch).
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{by_xp}'"),
    )
    .await;
    let erin_only = triage::project_inbox(&w.app, t.erin_ctx, "WEB", &decided("only"))
        .await
        .unwrap();
    assert!(erin_only.items.iter().all(|i| i.id != by_xp));
    let alice_only = triage::project_inbox(&w.app, a, "WEB", &decided("only"))
        .await
        .unwrap();
    assert!(alice_only.items.iter().any(|i| i.id == by_xp));
    assert_eq!(alice_only.items.len(), erin_only.items.len() + 1);
    // A self-triaged decision in another project (AUTH) never shows in WEB's.
    exec(
        &pool,
        &format!(
            "UPDATE issue_triage SET state = 'accepted', self_triage = true, decided_at = now(),
                 decided_by = (SELECT decided_by FROM issue_triage WHERE issue_id = '{by_x0}')
             WHERE issue_id = '{in_auth}'"
        ),
    )
    .await;
    let again = triage::project_inbox(&w.app, a, "WEB", &decided("only"))
        .await
        .unwrap();
    assert_eq!(again.items.len(), alice_only.items.len());
    assert!(again.items.iter().all(|i| i.id != in_auth));
    let e = triage::my_inbox(
        &w.app,
        a,
        &ListQuery {
            decided: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let _ = (t.frank, t.bob_ctx);
}

/// §2.1: the rebuilt CHECK lists are the program's lists; the inbox keys
/// and the triage settings rows exist for every project.
#[tokio::test]
async fn m1_3_constraints() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let def = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    for c in [
        "delegation_command_policies_command_kind_check",
        "delegations_proposal_ops_check",
        "changeset_version_ops_kind_check",
    ] {
        let d = def(c).await;
        let n = d.matches("'").count() / 2;
        assert_eq!(n, agents::PROPOSAL_OPS.len(), "{c}: {d}");
        for op in agents::PROPOSAL_OPS {
            assert!(d.contains(&format!("'{op}'")), "{c} lacks {op}: {d}");
        }
    }
    let d = def("project_setting_events_kind_check").await;
    for k in [
        "tabs_updated",
        "tabs_reset",
        "repo_linked",
        "repo_unlinked",
        "pr_hidden",
        "pr_unhidden",
        "github_content_revoked",
        "triage_settings_updated",
    ] {
        assert!(d.contains(&format!("'{k}'")), "{k}: {d}");
    }
    let e = def("issue_triage_state_check").await;
    for s in ["pending", "accepted", "declined", "duplicate"] {
        assert!(e.contains(&format!("'{s}'")));
    }
    assert!(
        def("issues_id_project_uq")
            .await
            .starts_with("UNIQUE (id, project_id)")
    );
    // A settings row per project (trigger), never enabled by default.
    assert_eq!(
        count(&pool, "SELECT count(*) FROM projects p WHERE NOT EXISTS (SELECT 1 FROM project_triage_settings s WHERE s.project_id = p.id)").await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM project_triage_settings WHERE enabled"
        )
        .await,
        0
    );
    // The triggers: an agent or group is not a responsible person; an open
    // status is not a decline status.
    let e = sqlx::query(
        "UPDATE project_triage_settings SET decline_status_id = $1 WHERE project_id = $2",
    )
    .bind(t.w.doing)
    .bind(t.w.web)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("done status"), "{e}");
    // The A5 flags are only on decisions.
    let i = t.w.issue(t.w.web, "x").await;
    let e = sqlx::query(
        "INSERT INTO issue_triage (issue_id, project_id, state, entered_at, self_triage) VALUES ($1, $2, 'pending', now(), true)",
    )
    .bind(i)
    .bind(t.w.web)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("check"), "{e}");
    // The composite key: an issue_triage row names the issue's own project.
    let e = sqlx::query(
        "INSERT INTO issue_triage (issue_id, project_id, state, entered_at) VALUES ($1, $2, 'pending', now())",
    )
    .bind(i)
    .bind(t.w.auth_p)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(e.to_string().contains("foreign key"), "{e}");
    assert!(oxledger::api::COMMAND_FEATURES.contains(&"triage_decide"));
}

/// Settings: a replay returns that write's response; only changed values are
/// checked, stale ones are warnings (verification fixes 1 and 2c).
#[tokio::test]
async fn m1_3_settings_replay_and_stale() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let a = w.alice_ctx;
    let put =
        |lock_version, enabled, resp: Option<Uuid>, decline: Option<Uuid>| TriageSettingsPut {
            lock_version,
            enabled,
            entry_rule: "creator_without_triage".into(),
            responsible_user_id: resp,
            decline_status_id: decline,
        };
    let k1 = Uuid::now_v7();
    let first = triage::put_settings(
        &w.app,
        a,
        k1,
        "WEB",
        &put(0, true, Some(t.dave), Some(w.done)),
    )
    .await
    .unwrap();
    assert_eq!(first.lock_version, 1);
    assert_eq!(first.warnings.len(), 1, "{first:?}");
    // A second write changes the settings and clears the warning.
    let second = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(1, false, None, Some(w.done)),
    )
    .await
    .unwrap();
    assert_eq!(second.lock_version, 2);
    assert!(second.warnings.is_empty());
    // The first key again: the first response as it was.
    let replay = triage::put_settings(
        &w.app,
        a,
        k1,
        "WEB",
        &put(0, true, Some(t.dave), Some(w.done)),
    )
    .await
    .unwrap();
    assert_eq!(replay, first);
    // The same key with another body: refused.
    let e = triage::put_settings(&w.app, a, k1, "WEB", &put(0, false, None, Some(w.done)))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::IdempotencyKeyReused);

    // Stale values: dave leaves the project, the decline status is edited
    // into an open one (by hand; the app has no such edit).
    let s3 = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(2, true, Some(t.dave), Some(w.done)),
    )
    .await
    .unwrap();
    exec(&pool, &format!("DELETE FROM member_roles WHERE member_id IN (SELECT id FROM members WHERE principal_id = '{}' AND project_id = '{}')", t.dave, w.web)).await;
    exec(
        &pool,
        &format!(
            "DELETE FROM members WHERE principal_id = '{}' AND project_id = '{}'",
            t.dave, w.web
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE issue_statuses SET category = 'doing' WHERE id = '{}'",
            w.done
        ),
    )
    .await;
    let g = triage::settings(&w.app, a, "WEB").await.unwrap();
    let kinds: Vec<&str> = g.warnings.iter().map(|w| w.kind.as_str()).collect();
    assert!(kinds.contains(&"responsible_invalid"), "{kinds:?}");
    assert!(kinds.contains(&"decline_status_invalid"), "{kinds:?}");
    // Turning Triage off with the stale values unchanged: saved, warned.
    let off = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(s3.lock_version, false, Some(t.dave), Some(w.done)),
    )
    .await
    .unwrap();
    assert!(!off.enabled);
    assert!(off.warnings.iter().any(|w| w.kind == "responsible_invalid"));
    // Changing to an invalid value is still refused.
    let e = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(off.lock_version, false, Some(t.erin), Some(w.doing)),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "triage_decline_status_invalid");
    exec(
        &pool,
        &format!(
            "UPDATE issue_statuses SET category = 'done' WHERE id = '{}'",
            w.done
        ),
    )
    .await;
    let e = triage::put_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(off.lock_version, false, Some(Uuid::now_v7()), Some(w.done)),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "triage_responsible_invalid");
}

/// Verification fixes 2a, 2b and the counts' visibility (M2).
#[tokio::test]
async fn m1_3_closed_pending_snooze_slack_counts() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let a = w.alice_ctx;
    enable(&t, "WEB", "creator_without_triage").await;

    // 2a: closed by a plain transition while pending: still in the inbox,
    // and a decline needs no transition.
    let i = create_as(w, t.bob_ctx, w.web, "closed while waiting").await;
    cmd(
        w,
        t.dave_ctx,
        &doc(vec![transition(i, w.done)], vec![pre(i, lv(w, i).await)]),
    )
    .await
    .unwrap();
    assert!(
        inbox(w, a, "WEB", ListQuery::default())
            .await
            .contains(&key(w, i).await)
    );
    let before = lv(w, i).await;
    cmd(w, a, &doc(vec![decide(i, "decline")], vec![pre(i, before)]))
        .await
        .unwrap();
    assert_eq!(tstate(w, i).await.unwrap().0, "declined");
    assert_eq!(lv(w, i).await, before + 1);
    // A duplicate still needs its transition (structure).
    let j = create_as(w, t.bob_ctx, w.web, "dup closed").await;
    let orig = create_as(w, a, w.web, "orig").await;
    let mut dd = decide(j, "duplicate");
    dd["duplicate_of"] = tgt(orig);
    let e = cmd(
        w,
        a,
        &doc(
            vec![dd],
            vec![pre(j, lv(w, j).await), pre(orig, lv(w, orig).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);

    // 2b: a client's "now + 1 hour" passes; 58 minutes does not.
    let k = create_as(w, t.bob_ctx, w.web, "snooze an hour").await;
    let mut sz = decide(k, "snooze");
    sz["snooze_until"] = json!((chrono::Utc::now() + chrono::Duration::minutes(58)).to_rfc3339());
    let e = cmd(w, a, &doc(vec![sz.clone()], vec![pre(k, lv(w, k).await)]))
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "snooze_range");
    sz["snooze_until"] = json!((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
    cmd(w, a, &doc(vec![sz], vec![pre(k, lv(w, k).await)]))
        .await
        .unwrap();

    // M2: counts only what the caller sees. greg triages with default
    // visibility: bob's private pending issue is not counted for him.
    let tv = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('TriageDefault','default') RETURNING id",
    )
    .await;
    grant(&pool, tv, &["view_issues", "triage_issues"]).await;
    let greg = oxledger::auth::create_user(&pool, "greg", "greg", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, greg, tv).await;
    let greg_ctx = sign_in(&pool, &w.policy, "greg").await;
    let n = |c: triage::TriageCounts| c.projects.first().map(|p| p.count).unwrap_or(0);
    let g0 = n(triage::counts(&w.app, greg_ctx).await.unwrap());
    let a0 = n(triage::counts(&w.app, a).await.unwrap());
    assert_eq!(g0, a0);
    let r = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                        "tracker_id": w.bug, "fields": {"subject": "secret", "is_private": true}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(tstate(w, r.effects[0].issue_id).await.unwrap().0, "pending");
    assert_eq!(n(triage::counts(&w.app, a).await.unwrap()), a0 + 1);
    assert_eq!(n(triage::counts(&w.app, greg_ctx).await.unwrap()), g0);
    let gl = triage::project_inbox(&w.app, greg_ctx, "WEB", &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(gl.items.len() as i64, g0 as i64);
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "https://spa.test";

struct Browser {
    session: String,
    csrf: String,
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
    let cookie = lc
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{LOGIN_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let bytes = lc.into_body().collect().await.unwrap().to_bytes();
    let token = serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = r
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/login")
                .header(header::COOKIE, format!("{LOGIN_COOKIE}={cookie}"))
                .header("x-csrf-token", token)
                .header(header::ORIGIN, ORIGIN_SPA)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"login": login, "password": PW}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()?
                .strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(|x| x.split(';').next().unwrap().to_string())
        })
        .unwrap();
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
    key: Option<Uuid>,
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
    if let Some(k) = key {
        req = req.header("idempotency-key", k.to_string());
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

#[tokio::test]
async fn m1_3_http() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    );
    let alice = login(&r, "alice").await;
    let bob = login(&r, "bob").await;

    // 401 without a session, on every route.
    for path in [
        "/projects/WEB/triage",
        "/projects/WEB/triage-settings",
        "/triage",
        "/triage/counts",
    ] {
        let (st, v) = call(&r, None, "GET", path, None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{path}: {v}");
    }
    // Settings: read, 403 for bob, 404 for a hidden project, key required.
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/projects/WEB/triage-settings",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["can_manage"], true);
    assert_eq!(v["data"]["lock_version"], 0);
    let body = json!({"lock_version": 0, "enabled": true, "entry_rule": "creator_without_triage",
                      "responsible_user_id": null, "decline_status_id": w.done});
    let (st, v) = call(
        &r,
        Some(&bob),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(body.clone()),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "forbidden_op");
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/HID/triage-settings",
        Some(body.clone()),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let (st, _) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(st.as_u16() / 100, 4, "no Idempotency-Key");
    let ikey = Uuid::now_v7();
    let (st, first) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(body.clone()),
        Some(ikey),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["lock_version"], 1);
    let (st, again) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(body.clone()),
        Some(ikey),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(again["data"], first["data"]);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(body.clone()),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["details"]["current_lock_version"], 1);
    let mut bad = body.clone();
    bad["lock_version"] = json!(1);
    bad["surprise"] = json!(true);
    let (st, _) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/triage-settings",
        Some(bad),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st.as_u16() / 100, 4, "unknown fields refused");

    // Inbox: three pending, paged by one.
    let mut keys = Vec::new();
    for n in 0..3 {
        keys.push(key(w, create_as(w, t.bob_ctx, w.web, &format!("p{n}")).await).await);
    }
    let (st, v) = call(&r, Some(&bob), "GET", "/projects/HID/triage", None, None).await;
    assert_eq!(st, StatusCode::OK, "bob sees HID (no Triage there): {v}");
    assert!(v["data"]["items"].as_array().unwrap().is_empty());
    let (st, v) = call(&r, Some(&alice), "GET", "/projects/HID/triage", None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let mut got = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let q = match &cursor {
            Some(c) => format!("/projects/WEB/triage?limit=1&cursor={c}"),
            None => "/projects/WEB/triage?limit=1".into(),
        };
        let (st, v) = call(&r, Some(&alice), "GET", &q, None, None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        for i in v["data"]["items"].as_array().unwrap() {
            got.push(i["key"].as_str().unwrap().to_string());
        }
        match v["data"]["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(got, keys);
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/projects/WEB/triage?cursor=garbage",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "invalid_cursor");
    for q in [
        "/projects/WEB/triage?view=later",
        "/projects/WEB/triage?include_snoozed=yes",
        "/triage?view=decided",
    ] {
        let (st, v) = call(&r, Some(&alice), "GET", q, None, None).await;
        assert_eq!(v["error"]["code"], "invalid_argument", "{q}: {st} {v}");
    }
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/projects/WEB/triage?view=decided&self_triage=only",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = call(&r, Some(&alice), "GET", "/triage", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(v["data"]["items"][0]["project"]["key"], "WEB");
    let (st, v) = call(&r, Some(&alice), "GET", "/triage/counts", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["projects"][0]["count"], 3);
    let (_, v) = call(&r, Some(&bob), "GET", "/triage/counts", None, None).await;
    assert!(v["data"]["projects"].as_array().unwrap().is_empty());
    // The issue page carries the state.
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        &format!("/issues/{}", keys[0]),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["triage"]["state"], "pending");
    assert_eq!(v["data"]["triage"]["can_decide"], true);
}
