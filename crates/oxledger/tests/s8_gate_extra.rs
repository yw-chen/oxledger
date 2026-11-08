// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S8 gate: T-V13 (an old delegation and `fixed_version_id`),
//! T-V21 (mixed version / issue writes under concurrency) and E4w-7
//! (tracing and audit rows never carry hidden content). Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::agents::{self, PolicyInput};
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, SubmitResult};
use oxledger::commands;
use oxledger::describe::{self, DescribeParams, DescribeScope};
use oxledger::error::ErrorCode;
use oxledger::issue_view::{self, GetParams};
use oxledger::policy::PolicyMode::Direct;
use oxledger::service::{self, SearchParams};
use oxledger::versions::{self, NewVersion, VersionPatch, VersionSet};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;
use world::*;

async fn lv(pool: &sqlx::PgPool, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn set_doc(i: Uuid, lv: i32, set: Value) -> Value {
    json!({"command_schema_version": 1, "title": "agent change",
        "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": set}],
        "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

fn get(key: &str) -> GetParams {
    GetParams {
        key: key.into(),
        fields: None,
        citations: None,
        include: None,
        note_limit: None,
        event_limit: None,
    }
}

async fn editable(w: &World, who: AuthContext, key: &str) -> Vec<String> {
    describe::describe(
        &w.app,
        who,
        DescribeParams {
            scope: DescribeScope::IssueActions,
            project: None,
            tracker: None,
            issue_key: Some(key.into()),
        },
    )
    .await
    .unwrap()
    .issue_actions
    .unwrap()
    .fields
    .into_iter()
    .filter(|f| f.editable)
    .map(|f| f.key)
    .collect()
}

/// T-V13: a delegation without `fixed_version_id` (as every delegation from
/// before U39 is: T-V13b keeps them unchanged) does not offer it; the
/// delegation page lists it as addable; once added, the agent can set it.
#[tokio::test]
async fn old_delegation_and_fixed_version() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let v = versions::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        "WEB",
        &NewVersion {
            name: "v1".into(),
            effective_date: None,
            description: None,
            status: None,
        },
    )
    .await
    .unwrap()
    .version
    .id;
    let i = w.issue(w.web, "schedule me").await;
    let key: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let ctx = w.new_context().await;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    issue_view::get(&w.app, agent, get(&key)).await.unwrap();

    let fields = editable(&w, agent, &key).await;
    assert!(fields.contains(&"subject".to_string()));
    assert!(
        !fields.contains(&"fixed_version_id".to_string()),
        "describe does not offer it"
    );
    let e = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &set_doc(i, lv(&pool, i).await, json!({"fixed_version_id": v})),
        false,
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(lv(&pool, i).await, 0, "no effect");

    // The delegation page: the field is offered, the person adds it.
    let p = agents::delegation_policies(&pool, &w.alice_ctx, d.id)
        .await
        .unwrap();
    assert!(p.available_fields.contains(&"fixed_version_id".to_string()));
    // In the delegation's writable_fields but without a policy row: not offered.
    let writable = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sql).bind(d.id).execute(&pool).await.unwrap();
        }
    };
    writable("UPDATE delegations SET writable_fields = array_append(writable_fields, 'fixed_version_id') WHERE id = $1").await;
    let p = agents::delegation_policies(&pool, &w.alice_ctx, d.id)
        .await
        .unwrap();
    assert!(!p.available_fields.contains(&"fixed_version_id".to_string()));
    writable("UPDATE delegations SET writable_fields = array_remove(writable_fields, 'fixed_version_id') WHERE id = $1").await;
    assert!(!p.available_fields.contains(&"subject".to_string()));
    let p = agents::set_delegation_policies(
        &pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        PolicyInput {
            commands: BTreeMap::new(),
            fields: BTreeMap::from([("fixed_version_id".to_string(), Direct)]),
        },
    )
    .await
    .unwrap();
    assert!(!p.available_fields.contains(&"fixed_version_id".to_string()));
    assert!(
        editable(&w, agent, &key)
            .await
            .contains(&"fixed_version_id".to_string())
    );
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &set_doc(i, lv(&pool, i).await, json!({"fixed_version_id": v})),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    let now: Option<Uuid> = sqlx::query_scalar("SELECT fixed_version_id FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(now, Some(v));
    // A policy row without the writable_fields entry: still not offered.
    writable("UPDATE delegations SET writable_fields = array_remove(writable_fields, 'fixed_version_id') WHERE id = $1").await;
    let p = agents::delegation_policies(&pool, &w.alice_ctx, d.id)
        .await
        .unwrap();
    assert!(!p.available_fields.contains(&"fixed_version_id".to_string()));
}

/// T-V21: random closes, reopens, deletes, assignments and date changes from
/// four workers. No deadlock or unexpected error; the final state keeps R4
/// (date order) and R5 (no visible issue in a deleted version). R1 under
/// races is T-V4 / T-V27 (both orders, with gates).
#[tokio::test]
async fn mixed_version_and_issue_writes() {
    let pool = test_pool().await;
    let w = Arc::new(World::build(&pool).await);
    grant(&pool, w.dev, &["manage_versions"]).await;
    let mut issues = Vec::new();
    for _ in 0..6 {
        issues.push(w.issue(w.auth_p, "stress").await);
    }
    let vs: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    for n in 0..4 {
        let v = versions::create(
            &w.app,
            w.alice_ctx,
            Uuid::now_v7(),
            "AUTH",
            &NewVersion {
                name: format!("s{n}"),
                effective_date: Some("2027-01-01".into()),
                description: None,
                status: None,
            },
        )
        .await
        .unwrap();
        vs.lock().unwrap().push(v.version.id);
    }
    let allowed = [
        ErrorCode::VersionNotOpen,
        ErrorCode::InvalidValue,
        ErrorCode::VersionInUse,
        ErrorCode::Conflict,
        ErrorCode::PreconditionFailed,
        ErrorCode::NotFound,
        ErrorCode::VersionNameTaken,
    ];
    let mut workers = Vec::new();
    for seed in 0..4u64 {
        let w = w.clone();
        let issues = issues.clone();
        let vs = vs.clone();
        workers.push(tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(0x5eed + seed);
            let mut seen: BTreeMap<String, usize> = BTreeMap::new();
            for step in 0..60 {
                let i = issues[rng.random_range(0..issues.len())];
                let v = {
                    let l = vs.lock().unwrap();
                    l[rng.random_range(0..l.len())]
                };
                let vlv: i32 =
                    sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
                        .bind(v)
                        .fetch_one(&w.pool)
                        .await
                        .unwrap();
                let r: anyhow::Result<()> = match rng.random_range(0..6) {
                    0 | 1 => {
                        let lv = lv(&w.pool, i).await;
                        let d = json!({"command_schema_version": 1,
                            "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                                     "set": {"fixed_version_id": v}}],
                            "preconditions": [{"target": tgt(i), "lock_version": lv}]});
                        commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
                            .await
                            .map(|_| ())
                    }
                    2 => {
                        let lv = lv(&w.pool, i).await;
                        let a = rng.random_range(1..28);
                        let b = rng.random_range(1..28);
                        let d = json!({"command_schema_version": 1,
                            "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i),
                                     "set": {"start_date": format!("2027-02-{a:02}"),
                                             "due_date": format!("2027-02-{b:02}")}}],
                            "preconditions": [{"target": tgt(i), "lock_version": lv}]});
                        commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
                            .await
                            .map(|_| ())
                    }
                    3 | 4 => {
                        let status = ["open", "locked", "closed"][rng.random_range(0..3)];
                        versions::update(
                            &w.app,
                            w.alice_ctx,
                            Uuid::now_v7(),
                            v,
                            &VersionPatch {
                                lock_version: vlv,
                                set: VersionSet {
                                    status: Some(status.into()),
                                    ..Default::default()
                                },
                            },
                        )
                        .await
                        .map(|_| ())
                    }
                    _ => {
                        let r = versions::delete(&w.app, w.alice_ctx, Uuid::now_v7(), v, vlv)
                            .await
                            .map(|_| ());
                        if r.is_ok() {
                            let n = versions::create(
                                &w.app,
                                w.alice_ctx,
                                Uuid::now_v7(),
                                "AUTH",
                                &NewVersion {
                                    name: format!("w{seed}-{step}"),
                                    effective_date: None,
                                    description: None,
                                    status: None,
                                },
                            )
                            .await
                            .unwrap();
                            let mut l = vs.lock().unwrap();
                            l.retain(|x| *x != v);
                            l.push(n.version.id);
                        }
                        r
                    }
                };
                let k = match &r {
                    Ok(()) => "ok".to_string(),
                    Err(e) => {
                        let c = code(e);
                        assert!(allowed.contains(&c), "unexpected {c:?}: {e:#}");
                        c.as_str().to_string()
                    }
                };
                *seen.entry(k).or_default() += 1;
            }
            seen
        }));
    }
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for h in workers {
        let seen = tokio::time::timeout(Duration::from_secs(120), h)
            .await
            .expect("no deadlock: workers finish")
            .unwrap();
        for (k, n) in seen {
            *total.entry(k).or_default() += n;
        }
    }
    assert!(total.get("ok").copied().unwrap_or(0) > 20, "{total:?}");
    // R4.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issues WHERE start_date IS NOT NULL AND due_date IS NOT NULL AND start_date > due_date"
        )
        .await,
        0
    );
    // R5: every issue here is visible to alice, so none is in a deleted version.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issues i JOIN versions v ON v.id = i.fixed_version_id WHERE v.deleted_at IS NOT NULL"
        )
        .await,
        0
    );
    // R1 on the final state: an issue in a version shares its project.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issues i JOIN versions v ON v.id = i.fixed_version_id WHERE v.project_id <> i.project_id"
        )
        .await,
        0
    );
}

/// A `tracing` writer that keeps everything written to it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// E4w-7: what a caller cannot read never reaches the logs (at TRACE, with
/// every crate) or the audit rows, whatever the caller tries.
#[tokio::test]
async fn hidden_content_stays_out_of_logs_and_audit() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    exec(
        &pool,
        "UPDATE roles SET issues_visibility = 'default' WHERE name = 'Dev'",
    )
    .await;
    let secret = "HIDDENE4W7";
    // bob's private issue with a private note; alice reads neither.
    let i = w.issue(w.auth_p, "placeholder").await;
    exec(
        &pool,
        &format!(
            "UPDATE issues SET is_private = true, subject = '{secret} subject',
                    description = '{secret} body' WHERE id = '{i}'"
        ),
    )
    .await;
    exec(
        &pool,
        &format!("INSERT INTO outbox (topic, entity_id) VALUES ('issue', '{i}')"),
    )
    .await;
    w.app.index.sync_all(&pool).await.unwrap();
    let key: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    let audit0 = count(&pool, "SELECT count(*) FROM auth_events").await;

    let buf = Captured::default();
    let sink = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::info!("capture on");

    let mut errors = Vec::new();
    errors.push(format!(
        "{:#}",
        issue_view::get(&w.app, w.alice_ctx, get(&key))
            .await
            .unwrap_err()
    ));
    let r = service::search(
        &w.app,
        w.alice_ctx,
        SearchParams {
            q: Some(secret.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(r.items.is_empty());
    for d in [
        set_doc(i, 0, json!({"subject": "mine now"})),
        json!({"command_schema_version": 1,
               "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "hi"}]}),
    ] {
        let e = commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &d)
            .await
            .unwrap_err();
        errors.push(format!("{e:#} {e:?}"));
    }
    // An agent of alice's tries the same.
    let ctx = w.new_context().await;
    w.delegate(w.auth_p).await.unwrap();
    let (_, agent) = w.token(ctx).await;
    errors.push(format!(
        "{:#}",
        issue_view::get(&w.app, agent, get(&key)).await.unwrap_err()
    ));
    let e = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &set_doc(i, 0, json!({"subject": "agent"})),
        false,
        ORIGIN,
    )
    .await
    .unwrap_err();
    errors.push(format!("{e:#} {e:?}"));
    tracing::info!("capture off");
    drop(guard);

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("capture on") && logs.contains("capture off"));
    assert!(
        !logs.contains(secret),
        "tracing leaked hidden content:\n{logs}"
    );
    for e in &errors {
        assert!(!e.contains(secret), "error leaked: {e}");
    }
    let leaked = count(
        &pool,
        &format!("SELECT count(*) FROM auth_events WHERE details::text LIKE '%{secret}%'"),
    )
    .await;
    assert_eq!(leaked, 0, "audit rows");
    assert!(count(&pool, "SELECT count(*) FROM auth_events").await >= audit0);
}
