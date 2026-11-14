// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 §10 S8 release gate: the rows an earlier survey found untested
//! or only half tested. Needs PostgreSQL 18; see `common::test_pool`.
//!
//! | test fn | contract rows | the other half lives in |
//! |---|---|---|
//! | `t_d1_every_command_in_every_mode` | T-D1 (5 commands × direct/review/off, no extra effect) | s6_direct.rs:154 (update_fields direct), :313 (add_note review, transition off) |
//! | `t_d4_counter_lock_and_deadline` | T-D4 (both contexts queue on the held counter row; a wait past the deadline → error, no write) | s6_direct.rs:588 (two contexts, consecutive keys) |
//! | `t_s2_outbox_consumed_twice` | T-S2 | — |
//! | `e4w_2_validate_and_apply_details` | E4w-2 (invalid_value and precondition: missing vs. invisible in propose / validate / approve-and-apply) | s5_changesets.rs:720 (validate `conflict`) |
//! | `e4w_3_duplicates_and_hidden_paths` | E4w-3 | — |
//! | `e4w_4_private_note_events` | E4w-4 (any number of private note events, limits and `truncated`) | s6_read.rs:878 (one private note) |
//! | `e4w_5_unreadable_cf_precondition` | E4w-5 (true, false, null and a missing field answer the same) | s4_commands.rs:769 (code only) |
//! | `e4w_6_replay_after_access_lost` | E4w-6 (agent `command_submit` and approve-and-apply replays) | s4_commands.rs:1879 (`/commands` replay) |
//! | `t_v1_v2_v3_v6_issue_side_rules` | T-V1 (closed; zero effect), T-V2 (closed version), T-V3 (invisible project; no `details.kind`), T-V6 | s4_commands.rs:1404 (locked), :1456 (R2 on locked), :1412 (other / deleted / missing), :1425 (start > due in one set) |
//! | `t_v4_t_v27_close_vs_assign` | T-V4 (both orders, real concurrency), T-V27 | s4_versions.rs:1105 (close waits for a held FOR SHARE; close first, sequential) |
//! | `t_v8_next_version_and_day_boundary` | T-V8 (same date, undated only, locked only, UTC 15:59 / 16:00 for Asia/Taipei) | s4_versions.rs:844 (overdue open), versions.rs unit `next_version_rules` |
//! | `t_v14_t_v22_t_v31_agent_policies` | T-V14, T-V22 (update_version review / off; update origin), T-V31 | s6_versions.rs:190–410 (create direct / review / off, update direct), s6_direct.rs:114 (`available_commands`) |
//! | `t_v15_bearer_on_version_rest` | T-V15 | s6_versions.rs:179 (people cannot use the agent route) |
//! | `t_v23_delegator_without_manage_versions`, `t_v23_manage_versions_removed_later` | T-V23 | — |

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use ox_core::filter::FieldType;
use ox_core::visibility::Pred;
use oxledger::agents::{self, DelegationInput, DelegationView, PolicyInput};
use oxledger::api::{self, ApiConfig};
use oxledger::auth::{self, AuthContext, SessionPolicy};
use oxledger::changesets::{self, ApproveInput, SubmitResult, ValidateInput, faults};
use oxledger::commands::{self, CommandResult};
use oxledger::custom_fields::{self, NewField};
use oxledger::error::{ErrorCode, classify};
use oxledger::issue_view;
use oxledger::policy::PolicyMode::{self, Direct, Off, Review};
use oxledger::service::{self, SearchParams};
use oxledger::versions::{self, NewVersion, VersionPatch, VersionSet};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

async fn lv(pool: &PgPool, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn vlv(pool: &PgPool, v: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
        .bind(v)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn key_of(pool: &PgPool, i: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn fixed_version(pool: &PgPool, i: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT fixed_version_id FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn events_of(pool: &PgPool, i: Uuid) -> i64 {
    count(
        pool,
        &format!("SELECT count(*) FROM issue_events WHERE issue_id = '{i}'"),
    )
    .await
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "s8 gate", "ops": ops, "preconditions": pre})
}

fn pre(i: Uuid, lv: i32) -> Value {
    json!({"target": tgt(i), "lock_version": lv})
}

/// One `update_fields` op on `i` with its current lock_version.
async fn set_doc(pool: &PgPool, i: Uuid, set: Value) -> Value {
    doc(
        vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": set})],
        vec![pre(i, lv(pool, i).await)],
    )
}

async fn human(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

async fn agent_submit(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<SubmitResult> {
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN).await
}

fn applied(r: &SubmitResult) -> Uuid {
    match r {
        SubmitResult::Applied { execution_id, .. } => *execution_id,
        other => panic!("expected applied, got {other:?}"),
    }
}

fn review_of(r: &SubmitResult) -> (Uuid, String) {
    match r {
        SubmitResult::ReviewRequired { reason, proposal } => {
            (proposal.changeset_id, reason.clone())
        }
        other => panic!("expected review_required, got {other:?}"),
    }
}

fn modes(c: &[(&str, PolicyMode)], f: &[(&str, PolicyMode)]) -> PolicyInput {
    PolicyInput {
        commands: c
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
        fields: f
            .iter()
            .map(|(k, m)| (k.to_string(), *m))
            .collect::<BTreeMap<_, _>>(),
    }
}

async fn set_policies(w: &World, d: Uuid, p: PolicyInput) -> anyhow::Result<agents::PolicyView> {
    agents::set_delegation_policies(&w.pool, &w.policy, &w.alice_ctx, d, p).await
}

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

/// The public shape of an error (code, message, details) with `subs` replaced.
fn shape(e: &anyhow::Error, subs: &[(String, &str)]) -> Value {
    let a = classify(e).unwrap_or_else(|| panic!("not an API error: {e:#}"));
    let mut s =
        json!({"code": a.code.as_str(), "message": a.message, "details": a.details}).to_string();
    for (from, to) in subs {
        s = s.replace(from.as_str(), to);
    }
    serde_json::from_str(&s).unwrap()
}

/// `v` with `subs` replaced and per-request values (ids, times, tokens) dropped.
fn scrub(v: &Value, subs: &[(String, &str)]) -> Value {
    const DROP: &[&str] = &[
        "changeset_id",
        "proposal_hash",
        "checked_at",
        "review_url",
        "expires_at",
        "op_id",
        "execution_id",
        "created_at",
        "finished_at",
        "consistent_after",
        "group_id",
    ];
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.retain(|k, _| !DROP.contains(&k.as_str()));
                m.values_mut().for_each(walk);
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let mut s = v.to_string();
    for (from, to) in subs {
        s = s.replace(from.as_str(), to);
    }
    let mut out: Value = serde_json::from_str(&s).unwrap();
    walk(&mut out);
    out
}

async fn version(
    w: &World,
    who: AuthContext,
    project: &str,
    name: &str,
    date: Option<String>,
) -> Uuid {
    versions::create(
        &w.app,
        who,
        Uuid::now_v7(),
        project,
        &NewVersion {
            name: name.into(),
            effective_date: date,
            description: None,
            status: None,
        },
    )
    .await
    .unwrap()
    .version
    .id
}

async fn set_version_status(w: &World, who: AuthContext, v: Uuid, status: &str) {
    versions::update(
        &w.app,
        who,
        Uuid::now_v7(),
        v,
        &VersionPatch {
            lock_version: vlv(&w.pool, v).await,
            set: VersionSet {
                status: Some(status.into()),
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
}

async fn today(pool: &PgPool) -> chrono::NaiveDate {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn day(t: chrono::NaiveDate, d: i64) -> String {
    (t + chrono::Duration::days(d)).to_string()
}

/// A project with issue tracking and the Bug tracker; members as given.
async fn project(w: &World, key: &str, members: &[Uuid]) -> Uuid {
    let p = id(
        &w.pool,
        &format!("INSERT INTO projects (key, name) VALUES ('{key}','{key}') RETURNING id"),
    )
    .await;
    exec(
        &w.pool,
        &format!(
            "INSERT INTO enabled_modules VALUES ('{p}', 'issue_tracking');
             INSERT INTO project_trackers VALUES ('{p}', '{}');",
            w.bug
        ),
    )
    .await;
    for m in members {
        member(&w.pool, p, *m, w.dev).await;
    }
    p
}

/// A version in a project nobody in the world can see.
async fn invisible_version(w: &World) -> Uuid {
    let ops = project(w, "OPS", &[]).await;
    id(
        &w.pool,
        &format!(
            "INSERT INTO versions (project_id, name, name_key, created_by, last_event_seq)
             VALUES ('{ops}', 'o1', 'o1', '{}', 0) RETURNING id",
            w.alice
        ),
    )
    .await
}

/// Waits until `n` transactions of this database wait on a lock.
async fn wait_blocked(pool: &PgPool, n: i64) {
    common::poll_until(
        &format!("{n} transactions to wait on a lock"),
        common::WAIT_TIMEOUT,
        async || {
            let c: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if c >= n {
                common::Break(())
            } else {
                common::Continue(format!("{c} waiting"))
            }
        },
    )
    .await
}

/// Holds `search_settings` FOR UPDATE: every write blocks on step 8b, after
/// its project / issue / version locks (§6.7, §17.6).
async fn hold_search_settings(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut g = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM search_settings FOR UPDATE")
        .execute(&mut *g)
        .await
        .unwrap();
    g
}

fn version_create(project: Uuid, fields: Value) -> Value {
    json!({"command_schema_version": 1, "title": "milestone",
           "ops": [{"op_id": op(), "kind": "create_version", "project_id": project, "fields": fields}]})
}

fn version_update(v: Uuid, lv: i32, set: Value) -> Value {
    json!({"command_schema_version": 1, "title": "milestone",
           "ops": [{"op_id": op(), "kind": "update_version", "target": {"type": "version", "id": v}, "set": set}],
           "preconditions": [{"target": {"type": "version", "id": v}, "lock_version": lv}]})
}

async fn delegate(
    w: &World,
    project: Uuid,
    ops: &[&str],
    fields: &[&str],
    policies: Option<PolicyInput>,
) -> DelegationView {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: project,
            proposal_ops: ops.iter().map(|s| s.to_string()).collect(),
            writable_fields: fields.iter().map(|s| s.to_string()).collect(),
            max_ops: None,
            expires_in_days: Some(30),
            policies,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap()
}

async fn approve(w: &World, cs: Uuid, key: Uuid) -> anyhow::Result<changesets::ApplyOutcome> {
    let view = changesets::get(&w.app, w.alice_ctx, cs, None).await?;
    changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        key,
        cs,
        &ApproveInput {
            version: view.current_version,
            proposal_hash: view.proposal_hash.clone(),
            review_mode: None,
        },
    )
    .await
}

async fn recent(w: &World, who: AuthContext, key: &str, limit: u32) -> Value {
    let r = issue_view::get(
        &w.app,
        who,
        serde_json::from_value(json!({"key": key, "fields": ["subject"],
            "include": ["recent_events", "recent_notes"], "event_limit": limit, "note_limit": 10}))
        .unwrap(),
    )
    .await
    .unwrap();
    let v = serde_json::to_value(r).unwrap();
    json!({"recent_events": v["recent_events"], "recent_notes": v["recent_notes"]})
}

fn kinds(r: &Value) -> Vec<String> {
    r["recent_events"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

/// A user with a role that sees only public issues (and their own).
async fn limited_user(w: &World, login: &str, perms: &[&str]) -> Uuid {
    let u = auth::create_user(&w.pool, login, login, PW, false)
        .await
        .unwrap();
    let role = id(
        &w.pool,
        &format!(
            "INSERT INTO roles (name, issues_visibility) VALUES ('{login}-role','default') RETURNING id"
        ),
    )
    .await;
    grant(&w.pool, role, perms).await;
    member(&w.pool, w.web, u, role).await;
    u
}

// ---------------------------------------------------------------- T-D1

fn d1_doc(w: &World, kind: &str, i: Uuid, j: Uuid, li: i32, lj: i32) -> Value {
    match kind {
        "create_issue" => doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug, "fields": {"subject": "d1 created"}}),
            ],
            vec![],
        ),
        "update_fields" => doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "d1 changed"}}),
            ],
            vec![pre(i, li)],
        ),
        "transition" => doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": w.doing}),
            ],
            vec![pre(i, li)],
        ),
        "add_note" => doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "d1 note"})],
            vec![pre(i, li)],
        ),
        "duplicate_of" => doc(
            vec![
                json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(i), "target": tgt(j)}),
            ],
            vec![pre(i, li), pre(j, lj)],
        ),
        _ => unreachable!(),
    }
}

/// issues in WEB, events, notes, duplicates, executions, changesets.
async fn footprint(w: &World) -> [i64; 6] {
    let mut out = [0; 6];
    for (n, sql) in [
        format!("SELECT count(*) FROM issues WHERE project_id = '{}'", w.web),
        "SELECT count(*) FROM issue_events".into(),
        "SELECT count(*) FROM issue_notes".into(),
        "SELECT count(*) FROM issue_duplicates".into(),
        "SELECT count(*) FROM command_executions".into(),
        "SELECT count(*) FROM changesets".into(),
    ]
    .iter()
    .enumerate()
    {
        out[n] = count(&w.pool, sql).await;
    }
    out
}

#[tokio::test]
async fn t_d1_every_command_in_every_mode() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    for kind in [
        "create_issue",
        "update_fields",
        "transition",
        "add_note",
        "duplicate_of",
    ] {
        for mode in [Direct, Review, Off] {
            let tag = format!("{kind}/{mode:?}");
            exec(&pool, "DELETE FROM agent_write_log").await;
            set_policies(&w, d.id, modes(&[(kind, mode)], &[]))
                .await
                .unwrap();
            let i = w.issue(w.web, "d1 source").await;
            let j = w.issue(w.web, "d1 target").await;
            let (li, lj) = (lv(&pool, i).await, lv(&pool, j).await);
            let dd = d1_doc(&w, kind, i, j, li, lj);
            let before = footprint(&w).await;
            let state = |pool: PgPool| async move {
                sqlx::query_as::<_, (String, Uuid, i32)>(
                    "SELECT subject, status_id, lock_version FROM issues WHERE id = $1",
                )
                .bind(i)
                .fetch_one(&pool)
                .await
                .unwrap()
            };
            let s0 = state(pool.clone()).await;
            let r = agent_submit(&w, agent, &dd).await;
            let after = footprint(&w).await;
            let delta: Vec<i64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
            let s1 = state(pool.clone()).await;
            match mode {
                Direct => {
                    let x = applied(r.as_ref().unwrap_or_else(|e| panic!("{tag}: {e:#}")));
                    assert_eq!(
                        count(
                            &pool,
                            &format!(
                                "SELECT count(*) FROM command_executions WHERE id = '{x}' AND origin = 'agent_direct'"
                            )
                        )
                        .await,
                        1,
                        "{tag}: an execution"
                    );
                    let want_issues = i64::from(kind == "create_issue");
                    let want_notes = i64::from(kind == "add_note");
                    let want_dups = i64::from(kind == "duplicate_of");
                    assert_eq!(
                        (delta[0], delta[2], delta[3], delta[4], delta[5]),
                        (want_issues, want_notes, want_dups, 1, 0),
                        "{tag}: exactly the command's effect"
                    );
                    match kind {
                        "update_fields" => assert_eq!(s1.0, "d1 changed", "{tag}"),
                        "transition" => assert_eq!(s1.1, w.doing, "{tag}"),
                        _ => {}
                    }
                    if kind == "create_issue" {
                        assert_eq!(s1, s0, "{tag}: other issues untouched");
                    }
                }
                Review => {
                    let (cs, reason) =
                        review_of(r.as_ref().unwrap_or_else(|e| panic!("{tag}: {e:#}")));
                    assert_eq!(reason, "policy", "{tag}");
                    assert_eq!(
                        delta,
                        [0, 0, 0, 0, 0, 1],
                        "{tag}: only the proposal is stored"
                    );
                    assert_eq!(s1, s0, "{tag}");
                    let state: String =
                        sqlx::query_scalar("SELECT state FROM changesets WHERE id = $1")
                            .bind(cs)
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                    assert_eq!(state, "proposed", "{tag}");
                }
                Off => {
                    let e = r.err().unwrap_or_else(|| panic!("{tag}: written"));
                    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{tag}: {e:#}");
                    assert_eq!(delta, [0; 6], "{tag}: nothing stored");
                    assert_eq!(s1, s0, "{tag}");
                }
            }
        }
    }
    faults::clear();
}

// ---------------------------------------------------------------- T-D4

#[tokio::test]
async fn t_d4_counter_lock_and_deadline() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let bob_ctx = sign_in(&pool, &w.policy, "bob").await;
    let db_ = agents::create_delegation(
        &pool,
        &w.policy,
        &bob_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: vec!["create_issue".into()],
            writable_fields: vec!["subject".into()],
            max_ops: None,
            expires_in_days: None,
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
    let bob_agent = {
        let c = agents::issue_credential(&pool, &w.policy, &bob_ctx, db_.context_id, "t", None)
            .await
            .unwrap();
        auth::authenticate_bearer(&pool, &c.token)
            .await
            .unwrap()
            .unwrap()
    };
    let create = |s: &str| {
        doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug, "fields": {"subject": s}}),
            ],
            vec![],
        )
    };
    let counter = |pool: PgPool, web: Uuid| async move {
        sqlx::query_scalar::<_, i32>("SELECT issue_counter FROM projects WHERE id = $1")
            .bind(web)
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    // Both contexts queue on the held counter row at the same time.
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR UPDATE")
        .bind(w.web)
        .execute(&mut *gate)
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for (a, s) in [(agent, "ctx a"), (bob_agent, "ctx b")] {
        let (app, dd) = (w.app.clone(), create(s));
        tasks.push(tokio::spawn(async move {
            changesets::command_submit(&app, a, Uuid::now_v7(), &dd, false, ORIGIN).await
        }));
    }
    wait_blocked(&pool, 2).await;
    gate.commit().await.unwrap();
    for t in tasks {
        applied(&common::join("a queued write", t).await.unwrap());
    }
    let nums: Vec<i32> =
        sqlx::query_scalar("SELECT number FROM issue_keys WHERE project_id = $1 ORDER BY number")
            .bind(w.web)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(nums, [1, 2], "no deadlock, unique consecutive keys");

    // A wait on the counter lock that crosses the delegation deadline.
    exec(&pool, "DELETE FROM agent_write_log").await;
    exec(
        &pool,
        &format!(
            "UPDATE delegations SET expires_at = clock_timestamp() + interval '1500 milliseconds' WHERE id = '{}'",
            d.id
        ),
    )
    .await;
    let c0 = counter(pool.clone(), w.web).await;
    let (issues0, execs0) = (
        count(&pool, "SELECT count(*) FROM issues").await,
        count(&pool, "SELECT count(*) FROM command_executions").await,
    );
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR UPDATE")
        .bind(w.web)
        .execute(&mut *gate)
        .await
        .unwrap();
    let key = Uuid::now_v7();
    let pending = {
        let (app, dd) = (w.app.clone(), create("too late"));
        tokio::spawn(async move {
            changesets::command_submit(&app, agent, key, &dd, false, ORIGIN).await
        })
    };
    wait_blocked(&pool, 1).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    gate.commit().await.unwrap();
    let e = common::join("the pending write", pending)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable, "{e:#}");
    assert_eq!(counter(pool.clone(), w.web).await, c0, "no key taken");
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, issues0);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM command_executions").await,
        execs0
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM operation_requests WHERE operation_id = '{key}'")
        )
        .await,
        0,
        "a failure leaves no operation record"
    );
    faults::clear();
}

// ---------------------------------------------------------------- T-S2

#[tokio::test]
async fn t_s2_outbox_consumed_twice() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let mut ids = Vec::new();
    for s in ["outboxprobe one", "outboxprobe two", "outboxprobe three"] {
        let r = human(
            &w,
            w.alice_ctx,
            &doc(
                vec![
                    json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug, "fields": {"subject": s}}),
                ],
                vec![],
            ),
        )
        .await
        .unwrap();
        ids.push(r.effects[0].issue_id);
    }
    let renamed = ids[0];
    human(
        &w,
        w.alice_ctx,
        &set_doc(
            &pool,
            renamed,
            json!({"subject": "outboxprobe renamed zanzibar"}),
        )
        .await,
    )
    .await
    .unwrap();
    w.app.index.sync_all(&pool).await.unwrap();
    let hits = |q: &str| {
        let mut v: Vec<Uuid> = w
            .app
            .index
            .search(q, &Pred::True, 100)
            .unwrap()
            .into_iter()
            .map(|h| h.issue_id)
            .collect();
        v.sort();
        v
    };
    let mut want = ids.clone();
    want.sort();
    assert_eq!(hits("outboxprobe"), want);
    let domain = |pool: PgPool| async move {
        sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            "SELECT (SELECT count(*) FROM issues), (SELECT count(*) FROM issue_events),
                    (SELECT COALESCE(sum(lock_version), 0) FROM issues),
                    (SELECT count(*) FROM command_executions), (SELECT count(*) FROM issue_keys)",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let d0 = domain(pool.clone()).await;

    // Consume everything again: the cursor forgotten, every message twice more,
    // two consumers at once on the same index.
    exec(
        &pool,
        "DELETE FROM outbox_cursors WHERE consumer LIKE 'search%';
         INSERT INTO outbox (topic, entity_id) SELECT 'issue', id FROM issues;
         INSERT INTO outbox (topic, entity_id) SELECT 'issue', id FROM issues;",
    )
    .await;
    // Both consumers must see the new rows: one `sync` skips rows at or
    // above H, and an older transaction elsewhere on the cluster holds H
    // back (G21). Wait until H passes them, then race the two.
    let newest: String = sqlx::query_scalar("SELECT max(xid)::text FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    common::wait_horizon_past(&pool, newest.parse().unwrap()).await;
    let (a, b) = common::wait_for("two consumers at once", common::WAIT_TIMEOUT, async {
        tokio::join!(w.app.index.sync(&pool), w.app.index.sync(&pool))
    })
    .await;
    assert!(a.unwrap() + b.unwrap() > 0);
    w.app.index.sync_all(&pool).await.unwrap();
    assert_eq!(hits("outboxprobe"), want, "one document per issue");
    assert_eq!(hits("zanzibar"), vec![renamed]);
    assert!(
        hits("one").iter().all(|i| *i != renamed),
        "the old subject is gone"
    );
    assert_eq!(domain(pool.clone()).await, d0, "no domain effect");
    // The same through the search service: one item per issue.
    let r = service::search(
        &w.app,
        w.alice_ctx,
        SearchParams {
            q: Some("outboxprobe".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut keys: Vec<String> = r.items.iter().map(|i| i.key.clone()).collect();
    keys.sort();
    let n = keys.len();
    keys.dedup();
    assert_eq!((n, keys.len()), (3, 3));
    // A rebuild replays from the start: still one each.
    w.app.index.rebuild(&pool).await.unwrap();
    assert_eq!(hits("outboxprobe"), want);
    assert_eq!(domain(pool.clone()).await, d0);
}

// ---------------------------------------------------------------- E4w-2

/// (label, document for a missing target, the same for an invisible one, substitutions).
type Pair = (&'static str, Value, Value, Vec<(String, &'static str)>);

#[tokio::test]
async fn e4w_2_validate_and_apply_details() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let hidden_v = invisible_version(&w).await;
    let missing_v = Uuid::now_v7();
    let budget = custom_fields::create_field(
        &pool,
        NewField {
            name: "Budget",
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let cf_hidden = format!("cf:{budget}");
    let cf_missing = "cf:999999".to_string();
    let i = w.issue(w.web, "target").await;
    exec(
        &pool,
        &format!("INSERT INTO issue_cf (issue_id, field_id, v_num) VALUES ('{i}', {budget}, 5)"),
    )
    .await;
    let d = delegate(
        &w,
        w.web,
        &["update_fields", "add_note"],
        &["subject", "fixed_version_id"],
        None,
    )
    .await;
    let (_, agent) = w.token(d.context_id).await;
    let l = lv(&pool, i).await;

    let pairs: Vec<Pair> = vec![
        (
            "invalid_value: version",
            doc(
                vec![
                    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"fixed_version_id": missing_v}}),
                ],
                vec![pre(i, l)],
            ),
            doc(
                vec![
                    json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"fixed_version_id": hidden_v}}),
                ],
                vec![pre(i, l)],
            ),
            vec![(missing_v.to_string(), "V"), (hidden_v.to_string(), "V")],
        ),
        (
            "precondition: unreadable field",
            doc(
                vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "x"})],
                vec![
                    json!({"target": tgt(i), "lock_version": l, "field_values": {&cf_missing: {"int": "5"}}}),
                ],
            ),
            doc(
                vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "x"})],
                vec![
                    json!({"target": tgt(i), "lock_version": l, "field_values": {&cf_hidden: {"int": "5"}}}),
                ],
            ),
            vec![(cf_missing.clone(), "CF"), (cf_hidden.clone(), "CF")],
        ),
    ];
    for (tag, a_doc, b_doc, subs) in pairs {
        exec(&pool, "DELETE FROM agent_write_log").await;
        let ra = changesets::propose(&w.app, agent, Uuid::now_v7(), &a_doc, ORIGIN).await;
        let rb = changesets::propose(&w.app, agent, Uuid::now_v7(), &b_doc, ORIGIN).await;
        match (ra, rb) {
            (Err(ea), Err(eb)) => {
                assert_eq!(shape(&ea, &subs), shape(&eb, &subs), "{tag}: propose");
            }
            (Ok(pa), Ok(pb)) => {
                let (va, vb) = (
                    serde_json::to_value(&pa).unwrap(),
                    serde_json::to_value(&pb).unwrap(),
                );
                assert_eq!(scrub(&va, &subs), scrub(&vb, &subs), "{tag}: propose");
                let val = |cs: Uuid| {
                    let app = w.app.clone();
                    let who = w.alice_ctx;
                    async move {
                        match changesets::validate(&app, who, cs, &ValidateInput { version: 1 })
                            .await
                        {
                            Ok(v) => serde_json::to_value(v).unwrap(),
                            Err(e) => shape(&e, &[]),
                        }
                    }
                };
                let (xa, xb) = (val(pa.changeset_id).await, val(pb.changeset_id).await);
                assert_eq!(scrub(&xa, &subs), scrub(&xb, &subs), "{tag}: validate");
                assert_ne!(xa["status"], "passed", "{tag}: {xa}");
                let app_ = |cs: Uuid| {
                    let w = &w;
                    async move {
                        match approve(w, cs, Uuid::now_v7()).await {
                            Ok(o) => json!({"pending": o.pending, "execution": o.execution}),
                            Err(e) => shape(&e, &[]),
                        }
                    }
                };
                let (oa, ob) = (app_(pa.changeset_id).await, app_(pb.changeset_id).await);
                assert_eq!(scrub(&oa, &subs), scrub(&ob, &subs), "{tag}: approve");
            }
            (a, b) => panic!("{tag}: different outcomes {a:?} / {b:?}"),
        }
        assert_eq!(lv(&pool, i).await, l, "{tag}: nothing written");
    }
    faults::clear();
}

// ---------------------------------------------------------------- E4w-3

#[tokio::test]
async fn e4w_3_duplicates_and_hidden_paths() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let erin = limited_user(
        &w,
        "erin",
        &[
            "view_issues",
            "edit_issues",
            "add_issue_notes",
            "manage_issue_relations",
        ],
    )
    .await;
    let e = AuthContext::test_user(erin);
    let mk_private = |subject: &'static str| {
        let pool = pool.clone();
        let (web, bug, new, bob) = (w.web, w.bug, w.new, w.bob);
        async move {
            oxledger::test_support::create_issue(
                &pool,
                oxledger::test_support::NewIssue {
                    project_id: web,
                    tracker_id: bug,
                    status_id: new,
                    subject,
                    description: "",
                    author_id: bob,
                    assigned_to_id: None,
                    is_private: true,
                    parent_id: None,
                },
            )
            .await
            .unwrap()
            .0
        }
    };
    let (a1, b1) = (w.issue(w.web, "a1").await, w.issue(w.web, "b1").await);
    let (a2, b2) = (w.issue(w.web, "a2").await, w.issue(w.web, "b2").await);
    let h = mk_private("hidden middle").await;
    // B1 → H → A1, through an issue erin cannot see.
    exec(
        &pool,
        &format!(
            "INSERT INTO issue_duplicates (source_issue_id, target_issue_id) VALUES ('{b1}','{h}'), ('{h}','{a1}')"
        ),
    )
    .await;
    let dup = |s: Uuid, t: Uuid| {
        let pool = pool.clone();
        async move {
            doc(
                vec![
                    json!({"op_id": op(), "kind": "duplicate_of", "source": tgt(s), "target": tgt(t)}),
                ],
                vec![pre(s, lv(&pool, s).await), pre(t, lv(&pool, t).await)],
            )
        }
    };
    let r1 = human(&w, e, &dup(a1, b1).await).await;
    let r2 = human(&w, e, &dup(a2, b2).await).await;
    let mut subs = Vec::new();
    for (x, tag) in [(a1, "A"), (b1, "B"), (a2, "A"), (b2, "B")] {
        subs.push((x.to_string(), tag));
        subs.push((key_of(&pool, x).await, tag));
    }
    let norm = |r: anyhow::Result<CommandResult>| match r {
        Ok(r) => scrub(&serde_json::to_value(r).unwrap(), &subs),
        Err(e) => shape(&e, &subs),
    };
    let (n1, n2) = (norm(r1), norm(r2));
    assert_eq!(n1, n2, "the hidden path changes nothing");
    assert_eq!(n2["status"], "succeeded", "{n2}");
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM issue_duplicates WHERE (source_issue_id, target_issue_id) IN (('{a1}','{b1}'), ('{a2}','{b2}'))"
            )
        )
        .await,
        2
    );

    // One end becomes invisible: the other end's history has no trace of it.
    let ctrl = w.issue(w.web, "control").await;
    let (c, d) = (w.issue(w.web, "c").await, w.issue(w.web, "d").await);
    human(&w, e, &dup(c, d).await).await.unwrap();
    let (ka2, kb2, kc, kd, kctrl) = (
        key_of(&pool, a2).await,
        key_of(&pool, b2).await,
        key_of(&pool, c).await,
        key_of(&pool, d).await,
        key_of(&pool, ctrl).await,
    );
    let ids_before = (recent(&w, e, &ka2, 20).await, recent(&w, e, &kd, 20).await);
    assert!(
        ids_before.0.to_string().contains("duplicate"),
        "visible ends show the relation: {}",
        ids_before.0
    );
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id IN ('{b2}', '{c}')"),
    )
    .await;
    let control = kinds(&recent(&w, e, &kctrl, 20).await);
    for (key, gone, gone_id) in [(&ka2, &kb2, b2), (&kd, &kc, c)] {
        let r = recent(&w, e, key, 20).await;
        assert_eq!(kinds(&r), control, "{key}: no event, no placeholder: {r}");
        let s = r.to_string();
        assert!(
            !s.contains(gone.as_str()) && !s.contains(&gone_id.to_string()),
            "{key}: {s}"
        );
    }
}

// ---------------------------------------------------------------- E4w-4

#[tokio::test]
async fn e4w_4_private_note_events() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dave = limited_user(&w, "dave", &["view_issues"]).await;
    let dv = AuthContext::test_user(dave);
    let r = human(
        &w,
        w.alice_ctx,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug, "fields": {"subject": "e4w-4"}}),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    let (i, key) = (r.effects[0].issue_id, r.effects[0].key.clone());
    let note = |body: String, private: bool| {
        let pool = pool.clone();
        let w = &w;
        async move {
            let d = doc(
                vec![
                    json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": body, "is_private": private}),
                ],
                vec![pre(i, lv(&pool, i).await)],
            );
            human(w, w.alice_ctx, &d).await.unwrap();
        }
    };
    note("public one".into(), false).await;
    note("public two".into(), false).await;
    let limits = [2u32, 3, 20];
    let mut base = Vec::new();
    for l in limits {
        base.push(recent(&w, dv, &key, l).await);
    }
    assert_eq!(base[0]["recent_events"]["truncated"], true);
    assert_eq!(base[1]["recent_events"]["truncated"], false);
    assert_eq!(kinds(&base[2]), ["note_added", "note_added", "created"]);
    let mut total = 0;
    for n in [1, 4, 20] {
        for k in 0..n {
            note(format!("private {total}-{k}"), true).await;
        }
        total += n;
        for (l, b) in limits.iter().zip(&base) {
            assert_eq!(
                &recent(&w, dv, &key, *l).await,
                b,
                "{total} private notes, event_limit {l}"
            );
        }
        // Someone who reads private notes does see them.
        let all = recent(&w, w.alice_ctx, &key, 20).await;
        assert_eq!(
            kinds(&all).len(),
            (3 + total).min(20),
            "alice sees the private events"
        );
    }
}

// ---------------------------------------------------------------- E4w-5

#[tokio::test]
async fn e4w_5_unreadable_cf_precondition() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let budget = custom_fields::create_field(
        &pool,
        NewField {
            name: "Budget",
            ty: FieldType::Int,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let cf = format!("cf:{budget}");
    let i = w.issue(w.web, "budgeted").await;
    exec(
        &pool,
        &format!("INSERT INTO issue_cf (issue_id, field_id, v_num) VALUES ('{i}', {budget}, 5)"),
    )
    .await;
    let l = lv(&pool, i).await;
    let n0 = count(&pool, "SELECT count(*) FROM issue_notes").await;
    let mut shapes = Vec::new();
    for (key, value) in [
        (cf.clone(), json!({"int": "5"})),
        (cf.clone(), json!({"int": "6"})),
        (cf.clone(), Value::Null),
        ("cf:999999".to_string(), json!({"int": "5"})),
    ] {
        let d = doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "probe"})],
            vec![json!({"target": tgt(i), "lock_version": l, "field_values": {&key: value}})],
        );
        let r = human(&w, w.alice_ctx, &d).await;
        let e = r.err().unwrap_or_else(|| panic!("{key}={value}: accepted"));
        shapes.push(scrub(&shape(&e, &[(key.clone(), "CF")]), &[]));
    }
    assert!(
        shapes.windows(2).all(|p| p[0] == p[1]),
        "true, false, null and missing look the same: {shapes:?}"
    );
    assert_eq!(shapes[0]["code"], "invalid_value");
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_notes").await, n0);
}

// ---------------------------------------------------------------- E4w-6

#[tokio::test]
async fn e4w_6_replay_after_access_lost() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;

    // command_submit: the agent replays after the issue became unreadable to it.
    let i = w.issue(w.web, "soon private").await;
    let ki = key_of(&pool, i).await;
    let key = Uuid::now_v7();
    let dd = note_doc(i, lv(&pool, i).await, "seen once");
    let r = changesets::command_submit(&w.app, agent, key, &dd, false, ORIGIN)
        .await
        .unwrap();
    let SubmitResult::Applied { effects, .. } = &r else {
        panic!("{r:?}")
    };
    assert_eq!(effects.len(), 1);
    // Making the issue private also moves WEB's epoch and closes the
    // context; the triggers are bypassed first so the replay's own
    // visibility re-check answers.
    exec(
        &pool,
        &format!(
            "BEGIN; SET LOCAL session_replication_role = replica;
             UPDATE issues SET is_private = true WHERE id = '{i}'; COMMIT;"
        ),
    )
    .await;
    let notes = count(&pool, "SELECT count(*) FROM issue_notes").await;
    let no_leak = |s: String| {
        assert!(
            !s.contains(&ki) && !s.contains(&i.to_string()),
            "the old result leaks: {s}"
        )
    };
    match changesets::command_submit(&w.app, agent, key, &dd, false, ORIGIN).await {
        Ok(again) => {
            assert_eq!(applied(&again), applied(&r), "a replay");
            let SubmitResult::Applied { effects, .. } = &again else {
                unreachable!()
            };
            assert!(effects.is_empty(), "no effect shown: {again:?}");
            no_leak(serde_json::to_string(&again).unwrap());
        }
        Err(e) => assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}"),
    }
    // The real path: the epoch moves and the replay is refused outright.
    let other = w.issue(w.web, "other").await;
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{other}'"),
    )
    .await;
    let e = changesets::command_submit(&w.app, agent, key, &dd, false, ORIGIN)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable, "{e:#}");
    no_leak(format!("{e:#} {}", details(&e)));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issue_notes").await,
        notes,
        "not run twice"
    );
    // A fresh context for the approval part.
    let ctx = w.new_context().await;
    delegate(&w, w.web, &["update_fields"], &["subject"], None).await;
    let (_, agent) = w.token(ctx).await;

    // approve-and-apply: the owner replays after losing the project.
    let j = w.issue(w.web, "approved").await;
    let kj = key_of(&pool, j).await;
    let p = changesets::propose(
        &w.app,
        agent,
        Uuid::now_v7(),
        &subject_doc(j, lv(&pool, j).await, "approved change"),
        ORIGIN,
    )
    .await
    .unwrap();
    let ka = Uuid::now_v7();
    let o = approve(&w, p.changeset_id, ka).await.unwrap();
    let ex = o.execution.clone().unwrap();
    assert_eq!(ex.status, "succeeded", "{ex:?}");
    assert_eq!(ex.effects.len(), 1);
    let hash = changesets::get(&w.app, w.alice_ctx, p.changeset_id, None)
        .await
        .unwrap()
        .proposal_hash;
    // alice leaves WEB. The membership triggers would also move WEB's epoch
    // and close the agent's context (then the answer is context_unavailable
    // before any replay); they are bypassed here so the replay's own
    // visibility re-check is what answers.
    exec(
        &pool,
        &format!(
            "BEGIN;
             SET LOCAL session_replication_role = replica;
             DELETE FROM member_roles WHERE member_id IN (SELECT id FROM members WHERE project_id = '{web}' AND principal_id = '{a}');
             DELETE FROM members WHERE project_id = '{web}' AND principal_id = '{a}';
             COMMIT;",
            web = w.web,
            a = w.alice
        ),
    )
    .await;
    let r = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        ka,
        p.changeset_id,
        &ApproveInput {
            version: 1,
            proposal_hash: hash,
            review_mode: None,
        },
    )
    .await;
    match r {
        Ok(o) => {
            let s = json!({"pending": o.pending, "execution": o.execution}).to_string();
            assert!(
                !s.contains(&kj) && !s.contains(&j.to_string()),
                "the old result is not shown: {s}"
            );
        }
        Err(e) => assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}"),
    }
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changeset_executions").await,
        1,
        "not applied twice"
    );
    faults::clear();
}

// ---------------------------------------------------------------- T-V1, T-V2, T-V3, T-V6

#[tokio::test]
async fn t_v1_v2_v3_v6_issue_side_rules() {
    let pool = test_pool().await;
    exec(&pool, "UPDATE org_settings SET timezone = 'Asia/Taipei'").await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let a = w.alice_ctx;
    let t = today(&pool).await;
    let v_open = version(&w, a, "WEB", "open one", Some(day(t, 30))).await;
    let v_locked = version(&w, a, "WEB", "locked one", Some(day(t, 31))).await;
    let v_closed = version(&w, a, "WEB", "closed one", Some(day(t, 32))).await;
    set_version_status(&w, a, v_locked, "locked").await;
    set_version_status(&w, a, v_closed, "closed").await;

    // T-V1: open works; locked and closed → version_not_open with zero effect.
    let i = w.issue(w.web, "v1").await;
    for (v, status) in [(v_locked, "locked"), (v_closed, "closed")] {
        let (l, ev) = (lv(&pool, i).await, events_of(&pool, i).await);
        let e = human(
            &w,
            a,
            &set_doc(&pool, i, json!({"fixed_version_id": v})).await,
        )
        .await
        .unwrap_err();
        assert_eq!(
            (code(&e), details(&e)["status"].clone()),
            (ErrorCode::VersionNotOpen, json!(status))
        );
        assert_eq!(
            (
                lv(&pool, i).await,
                events_of(&pool, i).await,
                fixed_version(&pool, i).await
            ),
            (l, ev, None),
            "{status}: zero effect"
        );
    }
    human(
        &w,
        a,
        &set_doc(&pool, i, json!({"fixed_version_id": v_open})).await,
    )
    .await
    .unwrap();
    assert_eq!(fixed_version(&pool, i).await, Some(v_open));

    // T-V2: an issue already in a closed version keeps working.
    let i2 = w.issue(w.web, "v2").await;
    let v_soon = version(&w, a, "WEB", "closing", Some(day(t, 40))).await;
    human(
        &w,
        a,
        &set_doc(&pool, i2, json!({"fixed_version_id": v_soon})).await,
    )
    .await
    .unwrap();
    set_version_status(&w, a, v_soon, "closed").await;
    let r = human(
        &w,
        a,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i2), "set": {"subject": "v2 renamed"}}),
                json!({"op_id": op(), "kind": "transition", "target": tgt(i2), "to_status_id": w.doing}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i2), "body": "still fine"}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i2), "set": {"fixed_version_id": v_soon}}),
            ],
            vec![pre(i2, lv(&pool, i2).await)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        r.items
            .iter()
            .map(|x| x.status.as_str())
            .collect::<Vec<_>>(),
        ["applied", "applied", "applied", "no_effect"]
    );
    let (subject, status): (String, Uuid) =
        sqlx::query_as("SELECT subject, status_id FROM issues WHERE id = $1")
            .bind(i2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((subject.as_str(), status), ("v2 renamed", w.doing));
    assert_eq!(fixed_version(&pool, i2).await, Some(v_soon));
    // Each change alone too.
    for set in [
        json!({"subject": "alone"}),
        json!({"fixed_version_id": v_soon}),
    ] {
        let r = human(&w, a, &set_doc(&pool, i2, set.clone()).await)
            .await
            .unwrap();
        let want = if set.get("fixed_version_id").is_some() {
            "no_effect"
        } else {
            "applied"
        };
        assert_eq!(r.items[0].status, want, "{set}");
    }

    // T-V3: another project's, a deleted, a missing and an invisible project's
    // version all answer the same invalid_value, without details.kind.
    let v_auth = version(&w, a, "AUTH", "auth one", None).await;
    let v_gone = version(&w, a, "WEB", "gone", None).await;
    versions::delete(&w.app, a, Uuid::now_v7(), v_gone, vlv(&pool, v_gone).await)
        .await
        .unwrap();
    let v_hidden = invisible_version(&w).await;
    let i3 = w.issue(w.web, "v3").await;
    let mut shapes = Vec::new();
    for v in [v_auth, v_gone, Uuid::now_v7(), v_hidden] {
        let e = human(
            &w,
            a,
            &set_doc(&pool, i3, json!({"fixed_version_id": v})).await,
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue, "{v}");
        assert!(details(&e).get("kind").is_none(), "{v}: {}", details(&e));
        shapes.push(scrub(&shape(&e, &[(v.to_string(), "V")]), &[]));
    }
    assert!(shapes.windows(2).all(|p| p[0] == p[1]), "{shapes:?}");
    // carol (WEB only) naming AUTH's version: the same answer.
    let carol = AuthContext::test_user(w.carol);
    let e = human(
        &w,
        carol,
        &set_doc(&pool, i3, json!({"fixed_version_id": v_auth})).await,
    )
    .await
    .unwrap_err();
    assert_eq!(
        scrub(&shape(&e, &[(v_auth.to_string(), "V")]), &[]),
        shapes[0]
    );
    assert_eq!(lv(&pool, i3).await, 0);

    // T-V6: the final state of the document decides date_order.
    let i6 = w.issue(w.web, "v6").await;
    human(
        &w,
        a,
        &set_doc(&pool, i6, json!({"due_date": "2026-11-05"})).await,
    )
    .await
    .unwrap();
    let l6 = lv(&pool, i6).await;
    let e = human(
        &w,
        a,
        &set_doc(&pool, i6, json!({"start_date": "2026-11-10"})).await,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["kind"].clone()),
        (ErrorCode::InvalidValue, json!("date_order"))
    );
    assert_eq!(lv(&pool, i6).await, l6, "nothing written");
    let r = human(
        &w,
        a,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i6), "set": {"start_date": "2026-11-10"}}),
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i6), "set": {"due_date": "2026-11-20"}}),
            ],
            vec![pre(i6, l6)],
        ),
    )
    .await;
    let r = r.unwrap_or_else(|e| panic!("T-V6 legal final state refused: {e:#}"));
    assert_eq!(r.status, "succeeded");
    let dates: (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
        sqlx::query_as("SELECT start_date, due_date FROM issues WHERE id = $1")
            .bind(i6)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dates,
        (
            Some("2026-11-10".parse().unwrap()),
            Some("2026-11-20".parse().unwrap())
        )
    );
}

// ---------------------------------------------------------------- T-V4, T-V27

enum Closer {
    Person(AuthContext),
    Agent(AuthContext),
}

async fn close_task(
    w: &World,
    closer: &Closer,
    v: Uuid,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let app = w.app.clone();
    let l = vlv(&w.pool, v).await;
    match *closer {
        Closer::Person(who) => tokio::spawn(async move {
            versions::update(
                &app,
                who,
                Uuid::now_v7(),
                v,
                &VersionPatch {
                    lock_version: l,
                    set: VersionSet {
                        status: Some("closed".into()),
                        ..Default::default()
                    },
                },
            )
            .await
            .map(|_| ())
        }),
        Closer::Agent(agent) => tokio::spawn(async move {
            let d = version_update(v, l, json!({"status": "closed"}));
            changesets::command_submit(&app, agent, Uuid::now_v7(), &d, false, ORIGIN)
                .await
                .map(|r| {
                    applied(&r);
                })
        }),
    }
}

async fn assign_task(
    w: &World,
    i: Uuid,
    v: Uuid,
) -> tokio::task::JoinHandle<anyhow::Result<CommandResult>> {
    let app = w.app.clone();
    let who = w.alice_ctx;
    let d = set_doc(&w.pool, i, json!({"fixed_version_id": v})).await;
    tokio::spawn(async move { commands::submit(&app, who, Uuid::now_v7(), &d).await })
}

async fn status_of(pool: &PgPool, v: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM versions WHERE id = $1")
        .bind(v)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn t_v4_t_v27_close_vs_assign() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let ctx = w.new_context().await;
    delegate(&w, w.web, &["update_version"], &[], None).await;
    let (_, agent) = w.token(ctx).await;

    for (tag, closer) in [
        ("T-V4", Closer::Person(bob)),
        ("T-V27", Closer::Agent(agent)),
    ] {
        exec(&pool, "DELETE FROM agent_write_log").await;
        // Assign first: the issue write holds the version FOR SHARE.
        let v = version(&w, w.alice_ctx, "WEB", &format!("{tag} a"), None).await;
        let i = w.issue(w.web, "assigned first").await;
        let gate = hold_search_settings(&pool).await;
        let assign = assign_task(&w, i, v).await;
        wait_blocked(&pool, 1).await;
        let close = close_task(&w, &closer, v).await;
        wait_blocked(&pool, 2).await;
        gate.commit().await.unwrap();
        common::join("the assignment", assign).await.unwrap();
        close
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{tag} assign first: close failed {e:#}"));
        assert_eq!(fixed_version(&pool, i).await, Some(v), "{tag}: stays in");
        assert_eq!(status_of(&pool, v).await, "closed", "{tag}");

        // Close first: the assignment waits, then sees the closed version.
        let v = version(&w, w.alice_ctx, "WEB", &format!("{tag} b"), None).await;
        let i = w.issue(w.web, "closed first").await;
        let gate = hold_search_settings(&pool).await;
        let close = close_task(&w, &closer, v).await;
        wait_blocked(&pool, 1).await;
        let assign = assign_task(&w, i, v).await;
        wait_blocked(&pool, 2).await;
        gate.commit().await.unwrap();
        close
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{tag} close first: {e:#}"));
        let e = common::join("the assignment", assign).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::VersionNotOpen, "{tag}: {e:#}");
        assert_eq!(
            (fixed_version(&pool, i).await, lv(&pool, i).await),
            (None, 0),
            "{tag}: never assigned after the close"
        );
    }
    faults::clear();
}

// ---------------------------------------------------------------- T-V8

/// A fixed-offset POSIX zone that shows the Asia/Taipei wall clock of the
/// UTC instant `at`, at the current real time `now`.
fn zone_as_if(now: chrono::DateTime<chrono::Utc>, at: chrono::DateTime<chrono::Utc>) -> String {
    let ms = (at - now).num_milliseconds() + 8 * 3600 * 1000;
    // Round up: the shifted clock never runs behind `at`.
    let secs = ms.div_euclid(1000) + i64::from(ms.rem_euclid(1000) != 0);
    let (sign, a) = if secs >= 0 { ('-', secs) } else { ('+', -secs) };
    format!(
        "XXX{sign}{:02}:{:02}:{:02}",
        a / 3600,
        a % 3600 / 60,
        a % 60
    )
}

#[tokio::test]
async fn t_v8_next_version_and_day_boundary() {
    let pool = test_pool().await;
    exec(&pool, "UPDATE org_settings SET timezone = 'Asia/Taipei'").await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let a = w.alice_ctx;
    let next = |project: &'static str| {
        let app = w.app.clone();
        async move {
            let l = versions::list(&app, a, project, None, Some("next"))
                .await
                .unwrap();
            (l.today.clone(), serde_json::to_value(&l.next).unwrap())
        }
    };
    let t = today(&pool).await;

    // Two open versions on the same date.
    version(&w, a, "AUTH", "same a", Some(day(t, 9))).await;
    version(&w, a, "AUTH", "same b", Some(day(t, 9))).await;
    let (_, n) = next("AUTH").await;
    assert_eq!(
        (n["outcome"].clone(), n["reason"].clone()),
        (json!("ambiguous"), json!("same_date"))
    );
    assert_eq!(n["candidates"].as_array().unwrap().len(), 2);
    // Only undated open versions → none, with the undated listed.
    project(&w, "UND", &[w.alice]).await;
    version(&w, a, "UND", "Backlog", None).await;
    let (_, n) = next("UND").await;
    assert_eq!(n["outcome"], "none");
    assert_eq!(n["undated"][0]["name"], "Backlog");
    // Only a locked version → none, nothing undated.
    project(&w, "LCK", &[w.alice]).await;
    let lk = version(&w, a, "LCK", "frozen", Some(day(t, 5))).await;
    set_version_status(&w, a, lk, "locked").await;
    let (_, n) = next("LCK").await;
    assert_eq!(n["outcome"], "none");
    assert_eq!(n["undated"], json!([]));

    // The day boundary: Asia/Taipei turns the date at 16:00 UTC.
    project(&w, "TZB", &[w.alice]).await;
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let base = now.date_naive();
    let at = |d: chrono::NaiveDate, h: u32, m: u32| d.and_hms_opt(h, m, 0).unwrap().and_utc();
    let s1 = [base.pred_opt().unwrap(), base, base.succ_opt().unwrap()]
        .into_iter()
        .map(|d| at(d, 15, 59))
        .min_by_key(|s| (*s - now).num_seconds().abs())
        .unwrap();
    let s2 = s1 + chrono::Duration::minutes(1);
    let d_s = s1.date_naive();
    let due = version(&w, a, "TZB", "due that day", Some(d_s.to_string())).await;
    exec(
        &pool,
        &format!(
            "UPDATE org_settings SET timezone = '{}'",
            zone_as_if(now, s1)
        ),
    )
    .await;
    let (today1, n1) = next("TZB").await;
    exec(
        &pool,
        &format!(
            "UPDATE org_settings SET timezone = '{}'",
            zone_as_if(now, s2)
        ),
    )
    .await;
    let (today2, n2) = next("TZB").await;
    exec(&pool, "UPDATE org_settings SET timezone = 'Asia/Taipei'").await;
    assert_eq!(
        today1,
        d_s.to_string(),
        "UTC 15:59 is still that day in Taipei"
    );
    assert_eq!(
        today2,
        d_s.succ_opt().unwrap().to_string(),
        "UTC 16:00 is the next day in Taipei"
    );
    assert_eq!(n1["outcome"], "resolved");
    assert_eq!(n1["version"]["id"], json!(due));
    assert_eq!(
        (n2["outcome"].clone(), n2["reason"].clone()),
        (json!("ambiguous"), json!("overdue_open_exists"))
    );
    assert_eq!(n2["candidates"][0]["overdue"], true);
}

// ---------------------------------------------------------------- T-V14, T-V22, T-V31

#[tokio::test]
async fn t_v14_t_v22_t_v31_agent_policies() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;

    // T-V31: a delegation without the version commands (as before U39).
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let p = agents::delegation_policies(&pool, &w.alice_ctx, d.id)
        .await
        .unwrap();
    assert!(p.available_commands.contains(&"create_version".to_string()));
    let e = agent_submit(&w, agent, &version_create(w.web, json!({"name": "v31"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(count(&pool, "SELECT count(*) FROM versions").await, 0);
    // One click on the delegation page adds it.
    let p = set_policies(
        &w,
        d.id,
        modes(
            &[("create_version", Direct), ("update_version", Direct)],
            &[],
        ),
    )
    .await
    .unwrap();
    assert!(!p.available_commands.contains(&"create_version".to_string()));
    let r = agent_submit(&w, agent, &version_create(w.web, json!({"name": "v31"})))
        .await
        .unwrap();
    applied(&r);
    let SubmitResult::Applied { versions: ve, .. } = &r else {
        unreachable!()
    };
    let v = ve[0].version_id;

    // T-V22: update_version direct, review, off.
    exec(&pool, "DELETE FROM agent_write_log").await;
    applied(
        &agent_submit(
            &w,
            agent,
            &version_update(v, 0, json!({"description": "direct"})),
        )
        .await
        .unwrap(),
    );
    let last = |pool: PgPool| async move {
        sqlx::query_as::<_, (i64, String, Option<Uuid>)>(
            "SELECT seq, origin, approver_id FROM version_events WHERE version_id = $1 ORDER BY seq DESC LIMIT 1",
        )
        .bind(v)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let (seq, origin, approver) = last(pool.clone()).await;
    assert_eq!((seq, origin.as_str(), approver), (2, "agent_direct", None));
    set_policies(&w, d.id, modes(&[("update_version", Review)], &[]))
        .await
        .unwrap();
    let r = agent_submit(
        &w,
        agent,
        &version_update(v, 1, json!({"description": "reviewed"})),
    )
    .await
    .unwrap();
    let (cs, reason) = review_of(&r);
    assert_eq!(reason, "policy");
    assert_eq!(vlv(&pool, v).await, 1, "nothing written before approval");
    let o = approve(&w, cs, Uuid::now_v7()).await.unwrap();
    let ex = o.execution.unwrap();
    assert!(ex.error.is_none(), "{ex:?}");
    let (seq, origin, approver) = last(pool.clone()).await;
    assert_eq!(
        (seq, origin.as_str(), approver),
        (3, "external_agent", Some(w.alice))
    );
    let desc: String = sqlx::query_scalar("SELECT description FROM versions WHERE id = $1")
        .bind(v)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(desc, "reviewed");
    set_policies(&w, d.id, modes(&[("update_version", Off)], &[]))
        .await
        .unwrap();
    let cs_before = count(&pool, "SELECT count(*) FROM changesets").await;
    let e = agent_submit(
        &w,
        agent,
        &version_update(v, 2, json!({"description": "off"})),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["reason"].clone()),
        (ErrorCode::ForbiddenOp, json!("policy"))
    );
    assert_eq!(vlv(&pool, v).await, 2);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changesets").await,
        cs_before
    );

    // T-V14: a new delegation whose fixed_version_id policy is review.
    let ctx2 = w.new_context().await;
    let d2 = delegate(
        &w,
        w.web,
        &["create_issue"],
        &["subject", "fixed_version_id"],
        Some(modes(&[], &[("fixed_version_id", Review)])),
    )
    .await;
    let (_, agent2) = w.token(ctx2).await;
    let _ = d2;
    let issues = count(&pool, "SELECT count(*) FROM issues").await;
    let create = |fields: Value| {
        doc(
            vec![
                json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug, "fields": fields}),
            ],
            vec![],
        )
    };
    let r = agent_submit(
        &w,
        agent2,
        &create(json!({"subject": "in v31", "fixed_version_id": v})),
    )
    .await
    .unwrap();
    assert_eq!(review_of(&r).1, "policy");
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, issues);
    applied(
        &agent_submit(&w, agent2, &create(json!({"subject": "no version"})))
            .await
            .unwrap(),
    );
    faults::clear();
}

// ---------------------------------------------------------------- T-V15

#[tokio::test]
async fn t_v15_bearer_on_version_rest() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let v = version(&w, w.alice_ctx, "WEB", "existing", None).await;
    let ctx = w.new_context().await;
    delegate(&w, w.web, &["create_version", "update_version"], &[], None).await;
    let (token, _) = w.token(ctx).await;
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    for (method, path, body) in [
        (
            "POST",
            format!("/api/v1/projects/{}/versions", w.web),
            json!({"name": "by bearer"}),
        ),
        (
            "POST",
            "/api/v1/projects/WEB/versions".to_string(),
            json!({"name": "by bearer"}),
        ),
        (
            "PATCH",
            format!("/api/v1/versions/{v}"),
            json!({"lock_version": 0, "set": {"name": "renamed by bearer"}}),
        ),
    ] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&path)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, ORIGIN)
                    .header("idempotency-key", Uuid::now_v7().to_string())
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let b: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(
            (status, b["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("unauthenticated")),
            "{method} {path}: {b}"
        );
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM versions").await, 1);
    assert_eq!(vlv(&pool, v).await, 0);
}

// ---------------------------------------------------------------- T-V23

#[tokio::test]
async fn t_v23_delegator_without_manage_versions() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    // alice has no manage_versions: the version commands get no policy rows.
    let d = delegate(
        &w,
        w.web,
        &["add_note", "create_version", "update_version"],
        &[],
        None,
    )
    .await;
    let (_, agent) = w.token(d.context_id).await;
    let e = agent_submit(&w, agent, &version_create(w.web, json!({"name": "v23"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    // Nor can she add them.
    let e = set_policies(&w, d.id, modes(&[("create_version", Direct)], &[]))
        .await
        .unwrap_err();
    assert_eq!(
        (code(&e), details(&e)["reason"].clone()),
        (ErrorCode::ForbiddenOp, json!("permission"))
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM versions").await, 0);
    faults::clear();
}

#[tokio::test]
async fn t_v23_manage_versions_removed_later() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let d = delegate(&w, w.web, &["create_version", "update_version"], &[], None).await;
    let (_, agent) = w.token(d.context_id).await;
    let r = agent_submit(&w, agent, &version_create(w.web, json!({"name": "before"})))
        .await
        .unwrap();
    applied(&r);
    let SubmitResult::Applied { versions: ve, .. } = &r else {
        unreachable!()
    };
    let v = ve[0].version_id;
    let without = |sql: String| {
        let pool = pool.clone();
        async move {
            exec(
                &pool,
                &format!("BEGIN; SET LOCAL session_replication_role = replica; {sql}; COMMIT;"),
            )
            .await
        }
    };
    let revoke = format!(
        "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'manage_versions'",
        w.dev
    );
    // The write itself checks the delegator's current manage_versions: with
    // the epoch triggers bypassed (the context stays valid) → forbidden_op.
    without(revoke.clone()).await;
    let e = agent_submit(&w, agent, &version_create(w.web, json!({"name": "after"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    let e = agent_submit(&w, agent, &version_update(v, 0, json!({"name": "renamed"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp, "{e:#}");
    assert_eq!(count(&pool, "SELECT count(*) FROM versions").await, 1);
    assert_eq!(vlv(&pool, v).await, 0);
    // The real path: a role_permissions change moves every project's epoch,
    // so the next write is refused even earlier (the context is closed).
    without(format!(
        "INSERT INTO role_permissions VALUES ('{}', 'manage_versions')",
        w.dev
    ))
    .await;
    exec(&pool, "DELETE FROM agent_write_log").await;
    agent_submit(
        &w,
        agent,
        &version_update(v, 0, json!({"name": "allowed again"})),
    )
    .await
    .unwrap();
    exec(&pool, &revoke).await;
    let e = agent_submit(&w, agent, &version_update(v, 1, json!({"name": "renamed"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ContextUnavailable, "{e:#}");
    assert_eq!(vlv(&pool, v).await, 1);
    faults::clear();
}
