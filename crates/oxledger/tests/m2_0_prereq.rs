// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.0 (prerequisites): migration 0032 against the code's lists
//! (T-M2-96), the `format` of descriptions and notes in the command engine,
//! its events and undo (R-M9, T-M2-14a), the `issue_event_mentions` rows
//! (R-MN1–R-MN4, T-M2-03a/17/18/21) and the runtime pause that replaced
//! `POST /agent-contexts/{id}/suspend|resume`.
//!
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::{self, SubmitResult, faults};
use oxledger::commands::{self, EVENT_ORIGINS, EXECUTION_ORIGINS, TEXT_FORMATS};
use oxledger::error::{ErrorCode, classify};
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::undo::{self, UndoInput};
use oxledger::{ai_team, notifications, perm};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use world::*;

/// Every test resets the database (World::build): one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The quoted values of a CHECK, sorted and without repeats.
async fn check_values(pool: &PgPool, name: &str) -> Vec<String> {
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

fn sorted(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
    v.sort();
    v.dedup();
    v
}

fn reason(e: &anyhow::Error) -> String {
    classify(e)
        .and_then(|a| a.details)
        .and_then(|d| d.get("reason").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "m2.0", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn desc(w: &World, i: Uuid) -> (String, String) {
    sqlx::query_as("SELECT description, description_format FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn human(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<commands::CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

async fn agent_submit(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<Uuid> {
    match changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN).await? {
        SubmitResult::Applied { execution_id, .. } => Ok(execution_id),
        other => panic!("expected applied, got {other:?}"),
    }
}

fn set_desc(i: Uuid, text: &str, format: Option<&str>) -> Value {
    let mut o = json!({"op_id": op(), "kind": "update_fields", "target": tgt(i),
                       "set": {"description": text}});
    if let Some(f) = format {
        o["format"] = json!(f);
    }
    o
}

fn note(i: Uuid, body: &str, format: Option<&str>) -> Value {
    let mut o = json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body});
    if let Some(f) = format {
        o["format"] = json!(f);
    }
    o
}

/// An issue whose description predates M2 (`plain`).
async fn plain_issue(w: &World, text: &str) -> Uuid {
    let i = w.issue(w.web, "legacy").await;
    sqlx::query("UPDATE issues SET description = $2, description_format = 'plain' WHERE id = $1")
        .bind(i)
        .bind(text)
        .execute(&w.pool)
        .await
        .unwrap();
    i
}

async fn mentioned(pool: &PgPool, issue: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT e.kind, p.login FROM issue_event_mentions m
         JOIN issue_events e ON e.id = m.event_id JOIN principals p ON p.id = m.user_id
         WHERE e.issue_id = $1 ORDER BY e.seq, p.login",
    )
    .bind(issue)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn pair(kind: &str, login: &str) -> (String, String) {
    (kind.to_string(), login.to_string())
}

#[tokio::test]
async fn t_m2_96_checks_match_code() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    let _w = World::build(&pool).await;
    assert_eq!(
        check_values(&pool, "role_permissions_known").await,
        sorted(perm::PERMISSIONS)
    );
    assert_eq!(
        check_values(&pool, "notifications_reasons_check").await,
        sorted(&notifications::REASONS)
    );
    assert_eq!(
        check_values(&pool, "issue_events_origin_check").await,
        sorted(&EVENT_ORIGINS)
    );
    assert_eq!(
        check_values(&pool, "command_executions_origin_check").await,
        sorted(&EXECUTION_ORIGINS)
    );
    for c in [
        "issues_description_format_check",
        "issue_notes_body_format_check",
    ] {
        assert_eq!(check_values(&pool, c).await, sorted(&TEXT_FORMATS), "{c}");
    }
    // A4 (a): no export permission in M2.
    assert!(!perm::PERMISSIONS.contains(&"export_issues"));
    // New rows default to markdown; the columns were added as plain (existing
    // content keeps its format).
    let defaults: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_default::text FROM information_schema.columns
         WHERE (table_name, column_name) IN (('issues','description_format'),('issue_notes','body_format'))
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        defaults,
        [
            ("issue_notes".into(), "'markdown'::text".into()),
            ("issues".into(), "'markdown'::text".into())
        ]
    );
    // An integration execution is the sponsor's, never a delegation's.
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'command_executions_integration_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for part in [
        "on_behalf_of_id IS NOT NULL",
        "delegation_id IS NULL",
        "context_id IS NULL",
    ] {
        assert!(def.contains(part), "{def}");
    }
    // fixed_by events (M2.6): the system acting for the sponsor, through the engine.
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'issue_events_check1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(def.contains("system_operation = 'fixed_by'::text"), "{def}");
}

#[tokio::test]
async fn t_m2_14a_format() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let a = w.alice_ctx;

    // create_issue: omitted = markdown; plain when asked.
    let mk = |fmt: Option<&str>| {
        let mut o = json!({"op_id": op(), "kind": "create_issue", "project_id": w.web,
                           "tracker_id": w.bug, "fields": {"subject": "s", "description": "d"}});
        if let Some(f) = fmt {
            o["format"] = json!(f);
        }
        doc(vec![o], vec![])
    };
    let c1 = human(&w, a, &mk(None)).await.unwrap();
    let i1 = c1.effects[0].issue_id;
    assert_eq!(desc(&w, i1).await.1, "markdown");
    let c2 = human(&w, a, &mk(Some("plain"))).await.unwrap();
    let i2 = c2.effects[0].issue_id;
    assert_eq!(desc(&w, i2).await.1, "plain");
    let created: Value = sqlx::query_scalar(
        "SELECT payload FROM issue_events WHERE issue_id = $1 AND kind = 'created'",
    )
    .bind(i2)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created["description_format"], "plain", "{created}");
    let bad = human(&w, a, &mk(Some("textile"))).await.unwrap_err();
    assert_eq!(code(&bad), ErrorCode::InvalidRequest, "{bad:#}");

    // T-M2-14a, the person: updating a plain description without format keeps plain.
    let p = plain_issue(&w, "old text").await;
    human(
        &w,
        a,
        &doc(
            vec![set_desc(p, "new text", None)],
            vec![(p, lv(&w, p).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(desc(&w, p).await, ("new text".into(), "plain".into()));
    // format goes with a description.
    let lone = json!({"op_id": op(), "kind": "update_fields", "target": tgt(p),
                      "set": {"subject": "x"}, "format": "markdown"});
    let e = human(&w, a, &doc(vec![lone], vec![(p, lv(&w, p).await)]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest, "{e:#}");
    // Converting is explicit; the event records it.
    human(
        &w,
        a,
        &doc(
            vec![set_desc(p, "new text", Some("markdown"))],
            vec![(p, lv(&w, p).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(desc(&w, p).await, ("new text".into(), "markdown".into()));
    let ev: Value = sqlx::query_scalar(
        "SELECT payload FROM issue_events WHERE issue_id = $1 AND kind = 'fields_changed'
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(p)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ev["description_format"],
        json!({"old": "plain", "new": "markdown"}),
        "{ev}"
    );

    // Notes: omitted = markdown, plain when asked.
    human(
        &w,
        a,
        &doc(
            vec![note(p, "n1", None), note(p, "n2", Some("plain"))],
            vec![(p, lv(&w, p).await)],
        ),
    )
    .await
    .unwrap();
    let formats: Vec<(String, String)> = sqlx::query_as(
        "SELECT body, body_format FROM issue_notes WHERE issue_id = $1 ORDER BY body",
    )
    .bind(p)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        formats,
        [
            ("n1".into(), "markdown".into()),
            ("n2".into(), "plain".into())
        ]
    );

    // T-M2-14a, the agent: only markdown, and a plain description needs it spelled out.
    let d = w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(d.context_id).await;
    let q = plain_issue(&w, "legacy").await;
    let e = agent_submit(
        &w,
        agent,
        &doc(
            vec![set_desc(q, "by agent", None)],
            vec![(q, lv(&w, q).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(reason(&e), "agent_format");
    let e = agent_submit(
        &w,
        agent,
        &doc(
            vec![set_desc(q, "by agent", Some("plain"))],
            vec![(q, lv(&w, q).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "agent_format", "{e:#}");
    let e = agent_submit(
        &w,
        agent,
        &doc(
            vec![note(q, "x", Some("plain"))],
            vec![(q, lv(&w, q).await)],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "agent_format", "{e:#}");
    assert_eq!(desc(&w, q).await, ("legacy".into(), "plain".into()));
    // create_issue: plain refused, omitted is markdown.
    let issues = count(&pool, "SELECT count(*) FROM issues").await;
    let e = agent_submit(&w, agent, &mk(Some("plain")))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(reason(&e), "agent_format", "{e:#}");
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, issues);
    agent_submit(&w, agent, &mk(None)).await.unwrap();
    let made: String =
        sqlx::query_scalar("SELECT description_format FROM issues ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(made, "markdown");
    // A markdown description: omitting format keeps markdown, which is fine.
    agent_submit(
        &w,
        agent,
        &doc(
            vec![set_desc(i1, "agent md", None)],
            vec![(i1, lv(&w, i1).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(desc(&w, i1).await, ("agent md".into(), "markdown".into()));

    let by_agent = |i: Uuid| {
        let pool = w.pool.clone();
        async move {
            let mut c = pool.acquire().await.unwrap();
            oxledger::markdown::description_by_agent(&mut c, i)
                .await
                .unwrap()
        }
    };
    // Undo restores the text and the format.
    assert!(!by_agent(q).await, "a person's description");
    let ex = agent_submit(
        &w,
        agent,
        &doc(
            vec![set_desc(q, "converted", Some("markdown"))],
            vec![(q, lv(&w, q).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(desc(&w, q).await, ("converted".into(), "markdown".into()));
    // R-M3: an agent's direct write (agent_direct, no proposal) is agent text.
    assert!(by_agent(q).await, "agent_direct description");
    let pv = undo::preview(&w.app, a, ex).await.unwrap();
    let pv = serde_json::to_value(&pv).unwrap();
    assert!(
        !pv.to_string().contains(commands::DESCRIPTION_FORMAT_KEY),
        "the format row is not a field of its own: {pv}"
    );
    let u = undo::apply(&w.app, a, Uuid::now_v7(), ex, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(u.fields_restored, 1);
    assert_eq!(desc(&w, q).await, ("legacy".into(), "plain".into()));

    // Design 06 R-M3 (decisions M2.1): an undo keeps the restored version's
    // author. The person's text back: a person's; the undo undone (the
    // agent's text back): the agent's again.
    assert!(!by_agent(q).await);
    let u2 = undo::apply(
        &w.app,
        a,
        Uuid::now_v7(),
        u.execution_id,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u2.fields_restored, 1);
    assert_eq!(desc(&w, q).await, ("converted".into(), "markdown".into()));
    assert!(by_agent(q).await, "the agent's text back is the agent's");
    // Two agent writes, the second undone by the person: the first agent
    // version is back, and it is still the agent's.
    for t in ["agent one", "agent two"] {
        let ex = agent_submit(
            &w,
            agent,
            &doc(vec![set_desc(i1, t, None)], vec![(i1, lv(&w, i1).await)]),
        )
        .await
        .unwrap();
        if t == "agent two" {
            undo::apply(&w.app, a, Uuid::now_v7(), ex, &UndoInput::default())
                .await
                .unwrap();
        }
    }
    assert_eq!(desc(&w, i1).await.0, "agent one");
    assert!(
        by_agent(i1).await,
        "an undo back to agent text keeps the agent"
    );
}

#[tokio::test]
async fn t_m2_mentions() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // dave exists but is not in WEB.
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let i = w.issue(w.web, "talk").await;

    // T-M2-03a: code, links and the author are skipped; unknown users are not recorded.
    let body = "hi @bob, @Carol and @alice. `@carol` [see](https://x.test/@bob) @dave @nobody";
    human(
        &w,
        a,
        &doc(vec![note(i, body, None)], vec![(i, lv(&w, i).await)]),
    )
    .await
    .unwrap();
    assert_eq!(
        mentioned(&pool, i).await,
        [pair("note_added", "bob"), pair("note_added", "carol")]
    );
    // plain text is scanned as it is (code spans are not code there).
    let j = w.issue(w.web, "plain").await;
    human(
        &w,
        a,
        &doc(
            vec![note(j, "see `@carol`", Some("plain"))],
            vec![(j, lv(&w, j).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(mentioned(&pool, j).await, [pair("note_added", "carol")]);

    // T-M2-17: a description edit records only people it newly mentions.
    human(
        &w,
        a,
        &doc(
            vec![set_desc(i, "for @bob", None)],
            vec![(i, lv(&w, i).await)],
        ),
    )
    .await
    .unwrap();
    human(
        &w,
        a,
        &doc(
            vec![set_desc(i, "for @bob and @carol", None)],
            vec![(i, lv(&w, i).await)],
        ),
    )
    .await
    .unwrap();
    let m = mentioned(&pool, i).await;
    assert_eq!(
        m.iter()
            .filter(|(k, _)| k == "fields_changed")
            .collect::<Vec<_>>(),
        [
            &pair("fields_changed", "bob"),
            &pair("fields_changed", "carol")
        ],
        "one row each, from the edit that added them: {m:?}"
    );
    let per_event: Vec<i64> = sqlx::query_scalar(
        "SELECT count(m.user_id) FROM issue_events e LEFT JOIN issue_event_mentions m ON m.event_id = e.id
         WHERE e.issue_id = $1 AND e.kind = 'fields_changed' GROUP BY e.seq ORDER BY e.seq",
    )
    .bind(i)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(per_event, [1, 1]);

    // T-M2-18 (R-MN4): an agent's mentions are not recorded.
    let d = w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(d.context_id).await;
    let k = w.issue(w.web, "agent").await;
    agent_submit(
        &w,
        agent,
        &doc(vec![note(k, "ping @bob", None)], vec![(k, lv(&w, k).await)]),
    )
    .await
    .unwrap();
    assert!(mentioned(&pool, k).await.is_empty());

    // T-M2-21: redacting the note removes its rows.
    grant(&pool, w.dev, &["manage_project"]).await;
    let (note_id, key): (Uuid, String) = sqlx::query_as(
        "SELECT n.id, k.key FROM issue_notes n JOIN issue_keys k ON k.issue_id = n.issue_id AND k.is_current
         WHERE n.issue_id = $1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    redaction::redact(
        &w.app,
        a,
        &w.policy,
        Uuid::now_v7(),
        &Target::Note { key, note: note_id },
        &RedactInput {
            reason_code: "personal_data".into(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        mentioned(&pool, i)
            .await
            .iter()
            .all(|(k, _)| k != "note_added")
    );
    // Redacting an event (the edit that added carol) removes its rows only.
    let (ev, key): (Uuid, String) = sqlx::query_as(
        "SELECT e.id, k.key FROM issue_events e JOIN issue_keys k ON k.issue_id = e.issue_id AND k.is_current
         WHERE e.issue_id = $1 AND e.kind = 'fields_changed' ORDER BY e.seq DESC LIMIT 1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    redaction::redact(
        &w.app,
        a,
        &w.policy,
        Uuid::now_v7(),
        &Target::IssueEvent { key, event: ev },
        &RedactInput {
            reason_code: "personal_data".into(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(mentioned(&pool, i).await, [pair("fields_changed", "bob")]);
    // An agent principal can never be a mention row.
    let e = sqlx::query(
        "INSERT INTO issue_event_mentions (event_id, user_id)
         SELECT e.id, $1 FROM issue_events e LIMIT 1",
    )
    .bind(d.agent_principal_id)
    .execute(&pool)
    .await
    .expect_err("agent as mention");
    assert!(e.as_database_error().is_some(), "{e}");
}

#[tokio::test]
async fn runtime_suspend_resume() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let d = w.delegate(w.web).await.unwrap();
    let (_t, agent) = w.token(d.context_id).await;
    let i = w.issue(w.web, "pause").await;
    let rt = d.runtime_id;

    // Only the sponsor: anyone else gets not_found, exactly as for a
    // runtime that does not exist (no existence oracle).
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let e = ai_team::suspend_runtime(&w.app, bob, rt).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    let missing = Uuid::now_v7();
    let e2 = ai_team::suspend_runtime(&w.app, bob, missing)
        .await
        .unwrap_err();
    assert_eq!(
        format!("{:?}", classify(&e)),
        format!("{:?}", classify(&e2))
    );
    let e = ai_team::resume_runtime(&w.app, &w.policy, bob, rt)
        .await
        .unwrap_err();
    let e2 = ai_team::resume_runtime(&w.app, &w.policy, bob, missing)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    assert_eq!(
        format!("{:?}", classify(&e)),
        format!("{:?}", classify(&e2))
    );

    ai_team::suspend_runtime(&w.app, w.alice_ctx, rt)
        .await
        .unwrap();
    let v = ai_team::runtimes(&w.app, w.alice_ctx).await.unwrap();
    let r = v.iter().find(|r| r.id == rt).unwrap();
    assert!(r.suspended_at.is_some());
    assert_eq!(r.suspension_reason.as_deref(), Some("manual"));
    let e = agent_submit(
        &w,
        agent,
        &doc(vec![note(i, "paused?", None)], vec![(i, lv(&w, i).await)]),
    )
    .await
    .unwrap_err();
    assert!(classify(&e).is_some(), "{e:#}");
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_notes").await, 0);

    // Resume needs a step-up (02 §16.6): a stale one is refused.
    let (_, session) = w.alice_ctx.require_human_session().unwrap();
    let stale = |ago: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE browser_sessions SET reauthenticated_at = now() - interval '{ago}' WHERE id = $1"
            )))
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    stale("16 minutes").await;
    let e = ai_team::resume_runtime(&w.app, &w.policy, w.alice_ctx, rt)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ReauthenticationRequired, "{e:#}");
    let v = ai_team::runtimes(&w.app, w.alice_ctx).await.unwrap();
    assert!(
        v.iter()
            .find(|r| r.id == rt)
            .unwrap()
            .suspended_at
            .is_some()
    );
    stale("0 minutes").await;
    ai_team::resume_runtime(&w.app, &w.policy, w.alice_ctx, rt)
        .await
        .unwrap();
    let v = ai_team::runtimes(&w.app, w.alice_ctx).await.unwrap();
    let r = v.iter().find(|r| r.id == rt).unwrap();
    assert!(r.suspended_at.is_none() && r.suspension_reason.is_none());
    agent_submit(
        &w,
        agent,
        &doc(vec![note(i, "back", None)], vec![(i, lv(&w, i).await)]),
    )
    .await
    .unwrap();

    // changed_projects (what ContextView gave): a source project whose
    // epoch moved since the context read it.
    let changed = |w: &World| {
        let (app, ctx) = (w.app.clone(), w.alice_ctx);
        async move {
            ai_team::runtimes(&app, ctx)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.id == rt)
                .unwrap()
                .changed_projects
        }
    };
    assert!(changed(&w).await.is_empty());
    sqlx::query("UPDATE projects SET security_epoch = security_epoch + 1 WHERE id = $1")
        .bind(w.web)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(changed(&w).await, [w.web]);
}

#[tokio::test]
async fn t_m2_mentions_cap() {
    let _serial = SERIAL.lock().await;
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    // 25 assignable users in WEB; one note names all of them.
    let mut logins = Vec::new();
    for n in 1..=25 {
        let login = format!("m{n:02}");
        let u: Uuid = sqlx::query_scalar(
            "INSERT INTO principals (kind, name, login) VALUES ('user', $1, $1) RETURNING id",
        )
        .bind(&login)
        .fetch_one(&pool)
        .await
        .unwrap();
        member(&pool, w.web, u, w.dev).await;
        logins.push(login);
    }
    let body = logins
        .iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(" ");
    let i = w.issue(w.web, "crowd").await;
    human(
        &w,
        w.alice_ctx,
        &doc(vec![note(i, &body, None)], vec![(i, lv(&w, i).await)]),
    )
    .await
    .unwrap();
    let m = mentioned(&pool, i).await;
    assert_eq!(m.len(), oxledger::mentions::MAX_MENTIONS, "{m:?}");
    assert_eq!(m.len(), 20);
    // The first twenty in the text.
    assert_eq!(
        m.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>(),
        logins[..20].to_vec()
    );
}
