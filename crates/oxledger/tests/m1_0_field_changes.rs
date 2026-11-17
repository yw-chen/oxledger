// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.0: migrations 0019–0020 (permission names, tracker scope,
//! roles_epoch, week_start, the estimated_hours CHECK), the
//! `issue_field_changes` projection (engine writes, backfill) and T-M1-03
//! (a redacted event's projection rows are gone).
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::AuthContext;
use oxledger::changesets::{self, SubmitResult, faults};
use oxledger::commands;
use oxledger::perm;
use oxledger::redaction::{self, RedactInput, Target};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use world::*;

/// The 0020 backfill statement, verbatim.
const BACKFILL: &str =
    "INSERT INTO issue_field_changes (event_id, issue_id, field_key, old_ref, new_ref, at)
SELECT e.id, e.issue_id, r.field_key, r.old_ref, r.new_ref, e.at
FROM issue_events e, issue_field_change_rows(e.kind, e.payload) r
WHERE e.redaction_id IS NULL
  AND e.kind IN ('created', 'fields_changed', 'status_changed')";

type Row = (Uuid, Uuid, String, Option<Uuid>, Option<Uuid>, String);

async fn rows(pool: &PgPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT event_id, issue_id, field_key, old_ref, new_ref, at::text FROM issue_field_changes
         ORDER BY event_id, field_key",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// (kind, field_key, old_ref, new_ref) of one issue, in event order.
async fn changes(pool: &PgPool, i: Uuid) -> Vec<(String, String, Option<Uuid>, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT e.kind, c.field_key, c.old_ref, c.new_ref FROM issue_field_changes c
         JOIN issue_events e ON e.id = c.event_id
         WHERE c.issue_id = $1 ORDER BY e.seq, c.field_key",
    )
    .bind(i)
    .fetch_all(pool)
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

fn doc(ops: Vec<Value>, pres: Vec<(Uuid, i32)>) -> Value {
    json!({"command_schema_version": 1, "title": "w", "ops": ops,
           "preconditions": pres.iter().map(|(i, l)| json!({"target": tgt(*i), "lock_version": l}))
               .collect::<Vec<_>>()})
}

async fn human(w: &World, who: AuthContext, d: &Value) -> commands::CommandResult {
    commands::submit(&w.app, who, Uuid::now_v7(), d)
        .await
        .unwrap()
}

async fn transition(w: &World, i: Uuid, to: Uuid) {
    human(
        w,
        w.alice_ctx,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "transition", "target": tgt(i), "to_status_id": to}),
            ],
            vec![(i, lv(w, i).await)],
        ),
    )
    .await;
}

/// `ever_in` as design 05 §3.2 defines it, until M1.1's Filter has it: the
/// current value is in the set, or a projection row's old/new is.
async fn ever_in_status(pool: &PgPool, i: Uuid, set: &[Uuid]) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM issues x WHERE x.id = $1 AND (x.status_id = ANY($2)
                OR EXISTS (SELECT 1 FROM issue_field_changes c
                           WHERE c.issue_id = x.id AND c.field_key = 'status_id'
                             AND (c.old_ref = ANY($2) OR c.new_ref = ANY($2)))))",
    )
    .bind(i)
    .bind(set)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn constraint(pool: &PgPool, name: &str) -> (String, bool) {
    sqlx::query_as(
        "SELECT pg_get_constraintdef(oid), convalidated FROM pg_constraint WHERE conname = $1",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn m1_0_migrations() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;

    // Permission names and the tracker scope: the code's lists are the DB's.
    let (known, _) = constraint(&pool, "role_permissions_known").await;
    for p in perm::PERMISSIONS {
        assert!(
            known.contains(&format!("'{p}'")),
            "{p} missing from {known}"
        );
    }
    assert_eq!(known.matches("::text").count(), perm::PERMISSIONS.len());
    let (scope, valid) = constraint(&pool, "role_permissions_tracker_scope").await;
    assert!(valid, "no existing row broke it, so it was validated");
    for p in perm::TRACKER_SCOPED {
        assert!(
            scope.contains(&format!("'{p}'")),
            "{p} missing from {scope}"
        );
    }
    assert_eq!(scope.matches("::text").count(), perm::TRACKER_SCOPED.len());
    let refused = sqlx::query(
        "INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ($1, 'log_time', false)",
    )
    .bind(w.dev)
    .execute(&pool)
    .await;
    assert!(refused.is_err(), "a time permission is not per tracker");
    let refused = sqlx::query(
        "UPDATE role_permissions SET all_trackers = false WHERE role_id = $1 AND permission = 'manage_issue_relations'",
    )
    .bind(w.dev)
    .execute(&pool)
    .await;
    assert!(refused.is_err());
    grant(
        &pool,
        w.dev,
        &["log_time", "triage_issues", "manage_public_views"],
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO role_permissions (role_id, permission, all_trackers) VALUES ('{}', 'edit_own_issues', false)",
            w.dev
        ),
    )
    .await;

    // roles_epoch: time_entries_visibility counts, a no-op does not.
    let (web0, auth0) = (epoch(&pool, w.web).await, epoch(&pool, w.auth_p).await);
    exec(
        &pool,
        &format!(
            "UPDATE roles SET time_entries_visibility = 'all' WHERE id = '{}'",
            w.dev
        ),
    )
    .await;
    assert_eq!(epoch(&pool, w.web).await, web0);
    exec(
        &pool,
        &format!(
            "UPDATE roles SET time_entries_visibility = 'own' WHERE id = '{}'",
            w.dev
        ),
    )
    .await;
    assert_eq!(epoch(&pool, w.web).await, web0 + 1);
    assert_eq!(epoch(&pool, w.auth_p).await, auth0 + 1);
    exec(
        &pool,
        &format!(
            "UPDATE roles SET issues_visibility = 'default' WHERE id = '{}'",
            w.dev
        ),
    )
    .await;
    assert_eq!(
        epoch(&pool, w.web).await,
        web0 + 2,
        "issues_visibility still counts"
    );
    assert!(
        sqlx::query("UPDATE roles SET time_entries_visibility = 'mine'")
            .execute(&pool)
            .await
            .is_err()
    );

    // week_start
    let ws: i16 = sqlx::query_scalar("SELECT week_start FROM org_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ws, 1);
    assert!(
        sqlx::query("UPDATE org_settings SET week_start = 8")
            .execute(&pool)
            .await
            .is_err()
    );

    // estimated_hours range in the DB too.
    let i = w.issue(w.web, "range").await;
    for bad in ["-0.01", "100000", "1.234"] {
        let r = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE issues SET estimated_hours = {bad} WHERE id = '{i}'"
        )))
        .execute(&pool)
        .await;
        // 1.234 rounds to 1.23 in numeric(7,2): stored, but never written by the engine.
        assert_eq!(r.is_err(), bad != "1.234", "{bad}");
    }

    // The event kinds keep 0014's and add triage_changed.
    let (kinds, _) = constraint(&pool, "issue_events_kind_check").await;
    for k in [
        "created",
        "fields_changed",
        "status_changed",
        "note_added",
        "duplicate_added",
        "note_retracted",
        "duplicate_removed",
        "triage_changed",
    ] {
        assert!(
            kinds.contains(&format!("'{k}'")),
            "{k} missing from {kinds}"
        );
    }
    let (rules, _) = constraint(&pool, "workflow_field_rules_field_key_check").await;
    assert!(rules.contains("estimated_hours"), "{rules}");
    let (pol, _) = constraint(&pool, "delegation_field_policies_field_key_check").await;
    assert!(pol.contains("'estimated_hours'"), "{pol}");
}

#[tokio::test]
async fn m1_0_field_changes() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_transitions VALUES ('{}','{}','{}','{}','always')",
            w.bug, w.dev, w.doing, w.done
        ),
    )
    .await;
    let prio: Uuid = sqlx::query_scalar("SELECT id FROM issue_priorities WHERE is_default")
        .fetch_one(&pool)
        .await
        .unwrap();

    // ------------------------------------------------ the engine writes the rows
    let r = human(
        &w,
        a,
        &doc(
            vec![json!({"op_id": op(), "kind": "create_issue", "project_id": w.web, "tracker_id": w.bug,
                        "fields": {"subject": "projected", "assigned_to_id": w.bob}})],
            vec![],
        ),
    )
    .await;
    let i = r.effects[0].issue_id;
    assert_eq!(
        changes(&pool, i).await,
        [
            ("created".into(), "assigned_to_id".into(), None, Some(w.bob)),
            ("created".into(), "priority_id".into(), None, Some(prio)),
            ("created".into(), "status_id".into(), None, Some(w.new)),
        ]
    );
    // A subject change has no projection; an assignee change does.
    human(
        &w,
        a,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "s2"}}),
            ],
            vec![(i, 0)],
        ),
    )
    .await;
    assert_eq!(changes(&pool, i).await.len(), 3);
    human(
        &w,
        a,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i),
                       "set": {"assigned_to_id": w.carol, "estimated_hours": "2"}}),
            ],
            vec![(i, lv(&w, i).await)],
        ),
    )
    .await;
    transition(&w, i, w.doing).await;
    let c = changes(&pool, i).await;
    assert_eq!(
        c[3..],
        [
            (
                "fields_changed".into(),
                "assigned_to_id".into(),
                Some(w.bob),
                Some(w.carol)
            ),
            (
                "status_changed".into(),
                "status_id".into(),
                Some(w.new),
                Some(w.doing)
            ),
        ]
    );
    // `at` is the event's.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issue_field_changes c JOIN issue_events e ON e.id = c.event_id
             WHERE c.at <> e.at OR c.issue_id <> e.issue_id"
        )
        .await,
        0
    );
    // An agent's direct write goes through the same statement.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let r = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"assigned_to_id": null}}),
            ],
            vec![(i, lv(&w, i).await)],
        ),
        false,
        ORIGIN,
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    assert_eq!(
        changes(&pool, i).await.last().unwrap(),
        &(
            "fields_changed".into(),
            "assigned_to_id".into(),
            Some(w.carol),
            None
        )
    );

    // ------------------------------------------------ the backfill gives the same rows
    let live = rows(&pool).await;
    assert!(!live.is_empty());
    exec(&pool, "DELETE FROM issue_field_changes").await;
    exec(&pool, BACKFILL).await;
    assert_eq!(rows(&pool).await, live);

    // ------------------------------------------------ T-M1-03: ever_in after D12
    // A → B → C (New → Doing → Done).
    transition(&w, i, w.done).await;
    assert!(ever_in_status(&pool, i, &[w.new]).await);
    assert!(ever_in_status(&pool, i, &[w.doing]).await);
    assert!(!ever_in_status(&pool, i, &[Uuid::now_v7()]).await);
    // alice may redact (manage_project); the epoch bump does not matter here.
    grant(&pool, w.dev, &["manage_project"]).await;
    let key: String =
        sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap();
    let event = |kind: &'static str, old: Option<Uuid>| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM issue_events WHERE issue_id = $1 AND kind = $2
                   AND ($3::text IS NULL OR payload->>'old' = $3::text) ORDER BY seq LIMIT 1",
            )
            .bind(i)
            .bind(kind)
            .bind(old.map(|u| u.to_string()))
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let redact = |event: Uuid| {
        let (w, key) = (&w, key.clone());
        async move {
            redaction::redact(
                &w.app,
                w.alice_ctx,
                &w.policy,
                Uuid::now_v7(),
                &Target::IssueEvent { key, event },
                &RedactInput {
                    reason_code: "confidential".into(),
                    reason: "wrong status history".into(),
                    attachments: Vec::new(),
                },
            )
            .await
            .unwrap()
        }
    };
    let a_to_b = event("status_changed", Some(w.new)).await;
    let created = event("created", None).await;
    let r = redact(a_to_b).await;
    assert!(r.changed);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_field_changes WHERE event_id = '{a_to_b}'")
        )
        .await,
        0
    );
    // The created event still says the issue started as New.
    assert!(ever_in_status(&pool, i, &[w.new]).await);
    redact(created).await;
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_field_changes WHERE event_id = '{created}'")
        )
        .await,
        0
    );
    assert!(
        !ever_in_status(&pool, i, &[w.new]).await,
        "redacted: no hit"
    );
    // Doing is still known from Doing → Done, which was not redacted.
    assert!(ever_in_status(&pool, i, &[w.doing]).await);
    // Rebuilding the projection does not bring the redacted rows back.
    let kept = rows(&pool).await;
    exec(&pool, "DELETE FROM issue_field_changes").await;
    exec(&pool, BACKFILL).await;
    assert_eq!(rows(&pool).await, kept);
}
