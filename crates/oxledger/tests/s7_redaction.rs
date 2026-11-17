// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S7: D12 redaction (§7.5, §16.7, §17.13, §18.2): T-S3, T-S4,
//! T-S5, T-S6, T-S8, T-S9, T-X21, T-X22, T-D9, T-D13, T-D14, T-V19, the
//! derived-copy half of T-G24 (s6b_github_read covers the epoch half and
//! this file the copies) and setting events (B16). Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, DelegationInput};
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput, GroupRef, ProposeResult, SubmitResult};
use oxledger::commands;
use oxledger::error::ErrorCode;
use oxledger::issue_view::{self, GetParams};
use oxledger::project_tabs;
use oxledger::redaction::{self, RedactInput, Redacted, Target};
use oxledger::service::{self, SearchParams};
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::undo::{self, UndoInput};
use oxledger::versions::{self, NewVersion};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use world::*;

struct Env {
    w: World,
    /// System administrator, no membership.
    admin: AuthContext,
    carol: AuthContext,
    bob: AuthContext,
}

/// alice manages both projects; dave is a system administrator.
async fn setup(pool: &sqlx::PgPool) -> Env {
    let w = World::build(pool).await;
    faults::clear();
    let mgr = id(
        pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Mgr','all') RETURNING id",
    )
    .await;
    grant(
        pool,
        mgr,
        &["view_issues", "manage_project", "manage_versions"],
    )
    .await;
    for p in [w.web, w.auth_p] {
        let m: Uuid = sqlx::query_scalar(
            "SELECT id FROM members WHERE project_id = $1 AND principal_id = $2",
        )
        .bind(p)
        .bind(w.alice)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
            .bind(m)
            .bind(mgr)
            .execute(pool)
            .await
            .unwrap();
    }
    // carol reads both projects too: the same audience, so proposals
    // across them are allowed (U1); she does not manage either.
    member(pool, w.auth_p, w.carol, w.dev).await;
    auth::create_user(pool, "dave", "dave", PW, true)
        .await
        .unwrap();
    Env {
        admin: sign_in(pool, &w.policy, "dave").await,
        carol: sign_in(pool, &w.policy, "carol").await,
        bob: sign_in(pool, &w.policy, "bob").await,
        w,
    }
}

async fn mk(w: &World, project: Uuid, description: &str) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: project,
            tracker_id: w.bug,
            status_id: w.new,
            subject: "work",
            description,
            author_id: w.bob,
            assigned_to_id: None,
            is_private: false,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn description(w: &World, i: Uuid) -> String {
    sqlx::query_scalar("SELECT description FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// alice adds a note (human command); its id.
async fn note(w: &World, i: Uuid, body: &str, private: bool) -> Uuid {
    let mut d = note_doc(i, lv(w, i).await, body);
    d["ops"][0]["is_private"] = json!(private);
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
        .await
        .unwrap();
    sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(i)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

fn set(i: Uuid, lv: i32, k: &str, v: Value) -> Value {
    json!({"command_schema_version": 1, "title": "write",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {k: v}}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

/// A fresh agent context with both projects delegated, after it read `key`.
async fn agent_after_reading(w: &World, key: &str) -> AuthContext {
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    w.delegate(w.auth_p).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    issue_view::get(&w.app, agent, notes_of(key)).await.unwrap();
    agent
}

fn notes_of(key: &str) -> GetParams {
    GetParams {
        key: key.into(),
        fields: None,
        citations: None,
        include: Some(vec!["recent_notes".into(), "recent_events".into()]),
        note_limit: Some(10),
        event_limit: Some(20),
    }
}

async fn propose(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<ProposeResult> {
    changesets::propose(&w.app, agent, Uuid::now_v7(), d, ORIGIN).await
}

async fn direct(w: &World, agent: AuthContext, d: &Value) -> Uuid {
    match changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN)
        .await
        .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("not applied: {other:?}"),
    }
}

fn why() -> RedactInput {
    RedactInput {
        reason_code: "confidential".into(),
        reason: "customer data pasted by mistake".into(),
        attachments: Vec::new(),
    }
}

async fn redact(e: &Env, who: AuthContext, t: Target) -> anyhow::Result<Redacted> {
    redaction::redact(&e.w.app, who, &e.w.policy, Uuid::now_v7(), &t, &why()).await
}

fn note_t(key: &str, note: Uuid) -> Target {
    Target::Note {
        key: key.into(),
        note,
    }
}

async fn states(w: &World, cs: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT pc.state FROM changeset_versions v JOIN proposal_contents pc ON pc.id = v.content_id
         WHERE v.changeset_id = $1 ORDER BY v.version",
    )
    .bind(cs)
    .fetch_all(&w.pool)
    .await
    .unwrap()
}

/// Every place §7.5 lists, as text, for a leak check.
async fn leaks(w: &World, needle: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM issue_notes WHERE body LIKE '%' || $1 || '%')
              + (SELECT count(*) FROM issue_events WHERE payload::text LIKE '%' || $1 || '%')
              + (SELECT count(*) FROM proposal_contents WHERE body::text LIKE '%' || $1 || '%')
              + (SELECT count(*) FROM changeset_groups WHERE label LIKE '%' || $1 || '%')
              + (SELECT count(*) FROM execution_field_changes
                 WHERE before_value::text LIKE '%' || $1 || '%' OR after_value::text LIKE '%' || $1 || '%')
              + (SELECT count(*) FROM issue_event_redactions WHERE reason LIKE '%' || $1 || '%')",
    )
    .bind(needle)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------- notes, proposals

#[tokio::test]
async fn note_redaction_and_proposals() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (w88, k88) = mk(w, w.web, "").await;
    let (a1, _) = mk(w, w.auth_p, "").await;
    let (a2, _) = mk(w, w.auth_p, "").await;
    let n = note(w, w88, "SECRET-W88 token abc", false).await;
    let agent = agent_after_reading(w, &k88).await;

    // One to WEB itself (first: a proposal to AUTH adds AUTH to R, and
    // WEB's wider audience would then be refused), one already applied
    // (AUTH, R ∋ WEB), one still open with a group label (T-X21).
    let web = propose(
        w,
        agent,
        &note_doc(w88, lv(w, w88).await, "SECRET-W88 reply"),
    )
    .await
    .unwrap();
    let done = propose(
        w,
        agent,
        &set(a2, lv(w, a2).await, "subject", json!("plain")),
    )
    .await
    .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        done.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: done.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(o.execution.unwrap().status, "succeeded");
    let done_exec = o.execution_id;
    let open = changesets::propose_in_group(
        &w.app,
        agent,
        Uuid::now_v7(),
        &set(a1, lv(w, a1).await, "subject", json!("SECRET-W88 copied")),
        Some(&GroupRef::Label("SECRET-W88 group".into())),
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(leaks(w, "SECRET-W88").await > 0);

    // Who may: not carol (WEB member, no manage_project); not without step-up.
    let err = redact(&e, e.carol, note_t(&k88, n)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);
    let (_, session) = w.alice_ctx.require_human_session().unwrap();
    exec(
        &pool,
        &format!("UPDATE browser_sessions SET reauthenticated_at = now() - interval '16 minutes' WHERE id = '{session}'"),
    )
    .await;
    let err = redact(&e, w.alice_ctx, note_t(&k88, n)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::ReauthenticationRequired);
    exec(
        &pool,
        &format!("UPDATE browser_sessions SET reauthenticated_at = now() WHERE id = '{session}'"),
    )
    .await;
    // Input checks.
    let bad = RedactInput {
        reason_code: "because".into(),
        reason: "x".into(),
        attachments: Vec::new(),
    };
    let err = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        Uuid::now_v7(),
        &note_t(&k88, n),
        &bad,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidValue);

    let before = epoch(&pool, w.web).await;
    let auth_before = epoch(&pool, w.auth_p).await;
    let key = Uuid::now_v7();
    let r = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        key,
        &note_t(&k88, n),
        &why(),
    )
    .await
    .unwrap();
    assert!(r.changed);
    assert_eq!(r.security_epoch, before + 1);
    assert_eq!(
        epoch(&pool, w.auth_p).await,
        auth_before,
        "only the note's project"
    );

    // The note reads as redacted; nothing anywhere still holds the text (T-S6, T-X21).
    let v = issue_view::get(&w.app, w.alice_ctx, notes_of(&k88))
        .await
        .unwrap();
    let shown = v
        .recent_notes
        .unwrap()
        .items
        .into_iter()
        .find(|x| x.note_id == n)
        .unwrap();
    assert!(shown.redacted);
    assert!(!shown.body.contains("SECRET"));
    assert_eq!(leaks(w, "SECRET-W88").await, 0, "T-S6 / T-X21");
    // §16.7: an approved changeset's execution copy goes by its version's
    // snapshot (AUTH issue, R ∋ WEB).
    let vals: Vec<(Value, Value)> = sqlx::query_as(
        "SELECT before_value, after_value FROM execution_field_changes WHERE execution_id = $1",
    )
    .bind(done_exec)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!vals.is_empty());
    assert!(
        vals.iter()
            .all(|(b, a)| *b == json!(redaction::REDACTED) && *a == json!(redaction::REDACTED))
    );
    for cs in [done.changeset_id, open.changeset_id, web.changeset_id] {
        assert!(
            states(w, cs).await.iter().all(|s| s == "redacted"),
            "every version of {cs}"
        );
        let view = changesets::get(&w.app, w.alice_ctx, cs, None)
            .await
            .unwrap();
        assert!(view.proposal.is_none());
        assert_eq!(view.content_unavailable.as_deref(), Some("redacted"));
    }
    let label: (Option<String>, String) = sqlx::query_as(
        "SELECT g.label, g.label_state FROM changesets c JOIN changeset_groups g ON g.id = c.group_id WHERE c.id = $1",
    )
    .bind(open.changeset_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(label, (None, "redacted".to_string()), "T-X21 group label");
    // The record names the reason, never the text.
    let rec: (String, Uuid) = sqlx::query_as(
        "SELECT reason_code, redacted_by FROM issue_event_redactions WHERE note_id = $1",
    )
    .bind(n)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rec, ("confidential".to_string(), w.alice));

    // T-S5: the open proposal cannot be approved any more.
    let err = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        open.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: open.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::ApprovalInvalidated, "T-S5: {err:#}");
    // T-S8: the agent re-submits what it remembers with the same context.
    let err = propose(
        w,
        agent,
        &set(a1, lv(w, a1).await, "subject", json!("SECRET-W88 again")),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::ContextUnavailable, "T-S8");

    // Replay returns the first answer; a second redaction changes nothing.
    let again = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        key,
        &note_t(&k88, n),
        &why(),
    )
    .await
    .unwrap();
    assert_eq!(again, r, "idempotent replay");
    let twice = redact(&e, w.alice_ctx, note_t(&k88, n)).await.unwrap();
    assert!(!twice.changed);
    assert_eq!(twice.security_epoch, before + 1, "no second bump");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_event_redactions").await,
        1
    );
    // Someone who cannot read the issue gets not_found (no hint it exists).
    let p = note(w, w88, "private SECRET-P", true).await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'view_private_notes'",
    )
    .await;
    let err = redact(&e, e.bob, note_t(&k88, p)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- events, versions, settings

#[tokio::test]
async fn events_versions_and_settings() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (a1, k1) = mk(w, w.auth_p, "").await;
    // A description the human later regrets (the event keeps the old value).
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &set(
            a1,
            lv(w, a1).await,
            "description",
            json!("SENSITIVE-D vendor"),
        ),
    )
    .await
    .unwrap();
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &set(a1, lv(w, a1).await, "description", json!("clean")),
    )
    .await
    .unwrap();
    // The ids come from the read API (`event_id`); the seq stays internal.
    let shown = issue_view::get(&w.app, w.alice_ctx, notes_of(&k1))
        .await
        .unwrap()
        .recent_events
        .unwrap()
        .items;
    let seqs: Vec<Uuid> = shown
        .iter()
        .filter(|x| {
            x.detail
                .as_ref()
                .is_some_and(|d| d.to_string().contains("SENSITIVE-D"))
        })
        .map(|x| x.event_id)
        .collect();
    assert_eq!(seqs.len(), 2, "set and then replaced");
    let agent = agent_after_reading(w, &k1).await;
    let p = propose(w, agent, &set(a1, lv(w, a1).await, "subject", json!("x")))
        .await
        .unwrap();
    let e0 = epoch(&pool, w.auth_p).await;

    // T-V19 style: a failure in the derived step leaves nothing behind.
    faults::arm("redact.before_derived", Action::Fail);
    let t = Target::IssueEvent {
        key: k1.clone(),
        event: seqs[0],
    };
    redact(&e, w.alice_ctx, t.clone()).await.unwrap_err();
    assert_eq!(epoch(&pool, w.auth_p).await, e0);
    assert_eq!(states(w, p.changeset_id).await, vec!["available"]);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_event_redactions").await,
        0
    );
    for s in &seqs {
        let r = redact(
            &e,
            w.alice_ctx,
            Target::IssueEvent {
                key: k1.clone(),
                event: *s,
            },
        )
        .await
        .unwrap();
        assert!(r.changed);
    }
    assert_eq!(epoch(&pool, w.auth_p).await, e0 + 2);
    assert_eq!(states(w, p.changeset_id).await, vec!["redacted"]);
    // The stored payload itself is replaced, not only the view (§7.5).
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_events WHERE payload::text LIKE '%SENSITIVE-D%'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_events WHERE payload = '\"[已遮蔽]\"'::jsonb AND redaction_id IS NOT NULL"
        )
        .await,
        2
    );
    // C3: reason length.
    let long = RedactInput {
        reason_code: "other".into(),
        reason: "x".repeat(501),
        attachments: Vec::new(),
    };
    let err = redaction::redact(&w.app, w.alice_ctx, &w.policy, Uuid::now_v7(), &t, &long)
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidValue);
    let v = issue_view::get(&w.app, w.alice_ctx, notes_of(&k1))
        .await
        .unwrap();
    let events = v.recent_events.unwrap().items;
    assert!(events.iter().filter(|x| x.redacted).count() == 2);
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("SENSITIVE-D")
    );
    // An unknown event, and an issue you cannot see.
    let err = redact(
        &e,
        w.alice_ctx,
        Target::IssueEvent {
            key: k1.clone(),
            event: Uuid::now_v7(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);

    // A version event (§17.13): the administrator, not a member.
    let ver = versions::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        "AUTH",
        &NewVersion {
            name: "v1".into(),
            effective_date: None,
            description: Some("VSECRET plan".into()),
            status: None,
        },
    )
    .await
    .unwrap();
    let vid = ver.version.id;
    let r = redact(
        &e,
        e.admin,
        Target::VersionEvent {
            version: vid,
            seq: 1,
        },
    )
    .await
    .unwrap();
    assert!(r.changed);
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM version_events WHERE version_id = $1 AND seq = 1")
            .bind(vid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(payload, json!(redaction::REDACTED));
    let ev = versions::events(&w.app, w.alice_ctx, vid, None, None)
        .await
        .unwrap();
    assert!(ev.items[0].redacted);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM execution_version_changes WHERE after_value::text LIKE '%VSECRET%'"
        )
        .await,
        0
    );

    // A setting event (B16): the tab title is user input too.
    let s = project_tabs::get(&w.app, w.alice_ctx, "AUTH")
        .await
        .unwrap();
    let mut tabs: Vec<Value> = s
        .tabs
        .iter()
        .map(|t| serde_json::to_value(t).unwrap())
        .collect();
    tabs.push(json!({"kind": "query", "title": "TSECRET", "query": "status:open"}));
    project_tabs::put(
        &w.app,
        w.alice_ctx,
        ORIGIN,
        Uuid::now_v7(),
        "AUTH",
        &serde_json::from_value(
            json!({"lock_version": s.lock_version, "tabs": tabs, "default_tab": "timeline"}),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ev: Uuid = sqlx::query_scalar(
        "SELECT id FROM project_setting_events WHERE kind = 'tabs_updated' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let t = Target::SettingEvent {
        project: "AUTH".into(),
        event: ev,
    };
    let err = redact(&e, e.bob, t.clone()).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);
    assert!(redact(&e, w.alice_ctx, t).await.unwrap().changed);
    let (payload, rid): (Value, Option<Uuid>) =
        sqlx::query_as("SELECT payload, redaction_id FROM project_setting_events WHERE id = $1")
            .bind(ev)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(payload, json!(redaction::REDACTED));
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM project_setting_event_redactions WHERE id = '{}'",
                rid.unwrap()
            )
        )
        .await,
        1
    );
    let listed = project_tabs::events(&w.app, w.alice_ctx, "AUTH", None)
        .await
        .unwrap();
    assert!(!serde_json::to_string(&listed).unwrap().contains("TSECRET"));
}

// ---------------------------------------------------------------- undo (T-D9, T-D13, T-D14)

#[tokio::test]
async fn undo_and_redaction() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (w1, kw1) = mk(w, w.web, "").await;
    let (a1, _) = mk(w, w.auth_p, "").await;

    // T-D9: WEB note content written straight into AUTH.description.
    let n1 = note(w, w1, "SECRET-D9 key", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let ex = direct(
        w,
        agent,
        &set(a1, lv(w, a1).await, "description", json!("SECRET-D9 key")),
    )
    .await;
    redact(&e, w.alice_ctx, note_t(&kw1, n1)).await.unwrap();
    let pv = undo::preview(&w.app, w.alice_ctx, ex).await.unwrap();
    let f = &pv.fields[0];
    assert_eq!(
        (f.restorable, f.reason.as_deref()),
        (false, Some("redacted"))
    );
    assert!(
        !format!("{:?}{:?}", f.before, f.after).contains("SECRET"),
        "T-D9"
    );
    // The AUTH field and its event keep the text until someone edits and
    // redacts them (§7.5 last paragraph); the execution copy is gone.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM execution_field_changes WHERE after_value::text LIKE '%SECRET-D9%'"
        )
        .await,
        0
    );

    // T-D13: E sets AUTH.description X → Y; a person undoes E; then WEB's note is redacted.
    let (a2, _) = mk(w, w.auth_p, "SECRET-X original").await;
    let n2 = note(w, w1, "source note", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let ex = direct(
        w,
        agent,
        &set(a2, lv(w, a2).await, "description", json!("Y")),
    )
    .await;
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        ex,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(description(w, a2).await, "SECRET-X original");
    redact(&e, w.alice_ctx, note_t(&kw1, n2)).await.unwrap();
    let pv = undo::preview(&w.app, w.alice_ctx, u.execution_id)
        .await
        .unwrap();
    assert!(
        pv.fields
            .iter()
            .all(|f| f.reason.as_deref() == Some("redacted")),
        "{pv:?}"
    );
    assert!(
        !format!(
            "{:?}",
            pv.fields
                .iter()
                .map(|f| (&f.before, &f.after))
                .collect::<Vec<_>>()
        )
        .contains("SECRET-X")
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM execution_field_changes WHERE before_value::text LIKE '%SECRET-X%' OR after_value::text LIKE '%SECRET-X%'"
        )
        .await,
        0,
        "T-D13: the undo's own copy too"
    );

    // T-D14 (a): the redaction holds the project; the undo waits, then reads the marker.
    let (a3, _) = mk(w, w.auth_p, "").await;
    let n3 = note(w, w1, "n3", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let ex = direct(
        w,
        agent,
        &set(a3, lv(w, a3).await, "description", json!("v1")),
    )
    .await;
    let g = Arc::new(Gate::default());
    faults::arm("redact.before_derived", Action::Block(g.clone()));
    let (app, policy, alice, k) = (w.app.clone(), w.policy.clone(), w.alice_ctx, kw1.clone());
    let red = tokio::spawn(async move {
        redaction::redact(
            &app,
            alice,
            &policy,
            Uuid::now_v7(),
            &note_t(&k, n3),
            &why(),
        )
        .await
    });
    g.reached.notified().await;
    let app = w.app.clone();
    let und = tokio::spawn(async move {
        undo::apply(&app, alice, Uuid::now_v7(), ex, &UndoInput::default()).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!und.is_finished(), "undo waits for the redaction");
    g.release.notify_one();
    common::join("the redaction", red).await.unwrap();
    let err = common::join("the undo", und).await.unwrap_err();
    assert_eq!(code(&err).as_str(), "undo_not_applicable", "{err:#}");
    let details = err
        .downcast_ref::<oxledger::error::ApiError>()
        .and_then(|x| x.details.clone())
        .unwrap_or_default();
    assert!(details.to_string().contains("redacted"), "{details}");
    assert_eq!(description(w, a3).await, "v1", "nothing written");

    // T-D14 (b): the undo holds the projects; the redaction waits and then clears its copy.
    let (a4, _) = mk(w, w.auth_p, "SECRET-B before").await;
    let n4 = note(w, w1, "n4", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let ex = direct(
        w,
        agent,
        &set(a4, lv(w, a4).await, "description", json!("v2")),
    )
    .await;
    let g = Arc::new(Gate::default());
    faults::arm("undo.after_projects", Action::Block(g.clone()));
    let app = w.app.clone();
    let und = tokio::spawn(async move {
        undo::apply(&app, alice, Uuid::now_v7(), ex, &UndoInput::default()).await
    });
    g.reached.notified().await;
    let (app, policy, k) = (w.app.clone(), w.policy.clone(), kw1.clone());
    let red = tokio::spawn(async move {
        redaction::redact(
            &app,
            alice,
            &policy,
            Uuid::now_v7(),
            &note_t(&k, n4),
            &why(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!red.is_finished(), "the redaction waits for the undo");
    g.release.notify_one();
    let u = common::join("the undo", und).await.unwrap();
    common::join("the redaction", red).await.unwrap();
    assert_eq!(description(w, a4).await, "SECRET-B before");
    let vals: Vec<(Value, Value)> = sqlx::query_as(
        "SELECT before_value, after_value FROM execution_field_changes WHERE execution_id = $1",
    )
    .bind(u.execution_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!vals.is_empty());
    assert!(
        vals.iter()
            .all(|(b, a)| *b == json!(redaction::REDACTED) && *a == json!(redaction::REDACTED)),
        "T-D14: the undo's new copy is cleared"
    );
}

// ---------------------------------------------------------------- propose vs redaction (T-S9, T-X22)

#[tokio::test]
async fn propose_races_redaction() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (w1, kw1) = mk(w, w.web, "").await;
    let (a1, _) = mk(w, w.auth_p, "").await;

    // Redaction first: the cross-project propose waits on WEB, then finds the epoch moved.
    let n = note(w, w1, "SECRET-R1", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let g = Arc::new(Gate::default());
    faults::arm("redact.before_derived", Action::Block(g.clone()));
    let (app, policy, alice, k) = (w.app.clone(), w.policy.clone(), w.alice_ctx, kw1.clone());
    let red = tokio::spawn(async move {
        redaction::redact(&app, alice, &policy, Uuid::now_v7(), &note_t(&k, n), &why()).await
    });
    g.reached.notified().await;
    let d = set(a1, lv(w, a1).await, "subject", json!("SECRET-R1"));
    let app = w.app.clone();
    let pr =
        tokio::spawn(
            async move { changesets::propose(&app, agent, Uuid::now_v7(), &d, ORIGIN).await },
        );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pr.is_finished(), "propose waits on WEB");
    g.release.notify_one();
    common::join("the redaction", red).await.unwrap();
    let err = common::join("the proposal", pr).await.unwrap_err();
    assert_eq!(
        code(&err),
        ErrorCode::ContextUnavailable,
        "T-X22 redaction first"
    );
    assert_eq!(leaks(w, "SECRET-R1").await, 0);

    // Propose first: the redaction waits, then its scan clears the new proposal.
    let n = note(w, w1, "SECRET-R2", false).await;
    let agent = agent_after_reading(w, &kw1).await;
    let g = Arc::new(Gate::default());
    faults::arm("propose.after_sources", Action::Block(g.clone()));
    let d = set(a1, lv(w, a1).await, "subject", json!("SECRET-R2"));
    let app = w.app.clone();
    let pr =
        tokio::spawn(
            async move { changesets::propose(&app, agent, Uuid::now_v7(), &d, ORIGIN).await },
        );
    g.reached.notified().await;
    let (app, policy, k) = (w.app.clone(), w.policy.clone(), kw1.clone());
    let red = tokio::spawn(async move {
        redaction::redact(&app, alice, &policy, Uuid::now_v7(), &note_t(&k, n), &why()).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!red.is_finished(), "the redaction waits for the propose");
    g.release.notify_one();
    let p = common::join("the proposal", pr).await.unwrap();
    common::join("the redaction", red).await.unwrap();
    assert_eq!(states(w, p.changeset_id).await, vec!["redacted"], "T-S9");
    assert_eq!(leaks(w, "SECRET-R2").await, 0);
}

// ---------------------------------------------------------------- read exits (T-S3, T-S4)

#[tokio::test]
async fn search_never_shows_redacted_text() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (a1, k1) = mk(w, w.auth_p, "").await;
    let n = note(w, a1, "zebraword private", true).await;
    note(w, a1, "quokkaword public", false).await;
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &set(a1, lv(w, a1).await, "description", json!("platypusword")),
    )
    .await
    .unwrap();
    w.app.index.sync(&pool).await.unwrap();
    let hits = |q: &'static str| {
        let app = w.app.clone();
        let who = w.alice_ctx;
        async move {
            service::search(
                &app,
                who,
                SearchParams {
                    q: Some(q.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .items
            .len()
        }
    };
    // T-S4: private notes are not in the shared document. Public notes
    // are since M1.7 (design 05 §3.9, T-M1-63).
    assert_eq!(hits("zebraword").await, 0);
    assert_eq!(hits("quokkaword").await, 1);
    assert_eq!(hits("platypusword").await, 1);
    // The description changes; the event with the old value is redacted
    // before the index catches up: no hit, no count (T-S3).
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &set(a1, lv(w, a1).await, "description", json!("plain")),
    )
    .await
    .unwrap();
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND payload::text LIKE '%platypusword%' ORDER BY seq LIMIT 1",
    )
    .bind(a1)
    .fetch_one(&pool)
    .await
    .unwrap();
    redact(
        &e,
        w.alice_ctx,
        Target::IssueEvent {
            key: k1.clone(),
            event,
        },
    )
    .await
    .unwrap();
    redact(&e, w.alice_ctx, note_t(&k1, n)).await.unwrap();
    assert_eq!(
        hits("platypusword").await,
        0,
        "T-S3: DB re-check before the index"
    );
    w.app.index.sync(&pool).await.unwrap();
    assert_eq!(hits("platypusword").await, 0);
    assert_eq!(hits("zebraword").await, 0);
}

// ---------------------------------------------------------------- admin-only, archived, versions

#[tokio::test]
async fn admin_only_archived_and_version_copies() {
    let pool = test_pool().await;
    let e = setup(&pool).await;
    let w = &e.w;
    let (w1, kw1) = mk(w, w.web, "").await;

    // A private note's event looks missing to someone who cannot read the note.
    let (a1, k1) = mk(w, w.auth_p, "").await;
    let p = note(w, a1, "private one", true).await;
    let pe: Uuid = sqlx::query_scalar("SELECT id FROM issue_events WHERE note_id = $1")
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'view_private_notes'",
    )
    .await;
    let mut shapes = Vec::new();
    for ev in [pe, Uuid::now_v7()] {
        let err = redact(
            &e,
            e.bob,
            Target::IssueEvent {
                key: k1.clone(),
                event: ev,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code(&err), ErrorCode::NotFound);
        shapes.push(format!("{err:#}"));
    }
    assert_eq!(shapes[0], shapes[1], "same shape as a missing event");

    // T-V30 / §17.13: an agent writes a WEB note into a version description.
    let n = note(w, w1, "SECRET-V30 roadmap", false).await;
    let ctx = w.new_context().await;
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: ["add_note", "create_version", "update_version"]
                .map(String::from)
                .to_vec(),
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    let (_, agent) = w.token(ctx).await;
    issue_view::get(&w.app, agent, notes_of(&kw1))
        .await
        .unwrap();
    let d = json!({"command_schema_version": 1, "title": "milestone",
        "ops": [{"op_id": op(), "kind": "create_version", "project_id": w.web,
                 "fields": {"name": "v9", "description": "SECRET-V30 roadmap"}}]});
    let ex = direct(w, agent, &d).await;
    let copies = |sql: &'static str| {
        let pool = pool.clone();
        async move { count(&pool, sql).await }
    };
    let q = "SELECT count(*) FROM execution_version_changes WHERE before_value::text LIKE '%SECRET-V30%' OR after_value::text LIKE '%SECRET-V30%'";
    assert!(copies(q).await > 0, "the copy exists before");
    redact(&e, w.alice_ctx, note_t(&kw1, n)).await.unwrap();
    assert_eq!(copies(q).await, 0, "T-V30: execution_version_changes");
    let pv = undo::preview(&w.app, w.alice_ctx, ex).await.unwrap();
    assert!(
        !format!("{pv:?}").contains("SECRET-V30"),
        "T-V30: undo preview"
    );

    // C2 / T-PT17: an administrator-only setting event (a PR revoke).
    let op_req: Uuid = sqlx::query_scalar("SELECT id FROM operation_requests LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let ev: Uuid = sqlx::query_scalar(
        "INSERT INTO project_setting_events (project_id, kind, actor_id, operation_request_id, payload)
         VALUES ($1, 'github_content_revoked', $2, $3, '{\"pr_ids\": []}') RETURNING id",
    )
    .bind(w.auth_p)
    .bind(w.alice)
    .bind(op_req)
    .fetch_one(&pool)
    .await
    .unwrap();
    let t = Target::SettingEvent {
        project: "AUTH".into(),
        event: ev,
    };
    let e0 = epoch(&pool, w.auth_p).await;
    let err = redact(&e, w.alice_ctx, t.clone()).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound, "a manager does not see it");
    assert_eq!(epoch(&pool, w.auth_p).await, e0, "and nothing happened");
    assert!(redact(&e, e.admin, t).await.unwrap().changed);
    let mine = project_tabs::events(&w.app, w.alice_ctx, "AUTH", None)
        .await
        .unwrap();
    assert!(
        mine.items.iter().all(|i| i.id != ev),
        "still administrator-only"
    );
    let all = project_tabs::events(&w.app, e.admin, "AUTH", None)
        .await
        .unwrap();
    let it = all.items.iter().find(|i| i.id == ev).unwrap();
    assert!(it.redacted);

    // C9: in an archived project only an administrator redacts.
    let n = note(w, w1, "archived text", false).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'archived' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    let err = redact(&e, w.alice_ctx, note_t(&kw1, n)).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    assert!(redact(&e, e.admin, note_t(&kw1, n)).await.unwrap().changed);
}
