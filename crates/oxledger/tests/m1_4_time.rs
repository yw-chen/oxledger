// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.4: time tracking (§2.5, §3.5, §3.10.2, §4.3, §7.4).
//! T-M1-31 … T-M1-44 with 39a and 39b, the lock-order stress test, the
//! agent limits (E4w), the CHECK lists of 0023 against the code's
//! constants, and the HTTP routes. Needs PostgreSQL 18; see
//! `common::test_pool`.

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
use oxledger::auth::{AuthContext, SessionPolicy};
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput, SubmitResult};
use oxledger::commands::{self, CommandResult};
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode;
use oxledger::redaction::{self, RedactInput, Target};
use oxledger::time_reports::{self, ListQuery, SummaryQuery};
use oxledger::undo::{self, UndoInput};
use oxledger::views::{self, NewView, ViewRunParams};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

struct T {
    w: World,
    /// view_time_entries (all), log_time, edit_time_entries, log_time_for_other_users.
    lead: Uuid,
    /// view_time_entries (own), log_time, edit_own_time_entries.
    own: Uuid,
    bob_ctx: AuthContext,
    carol_ctx: AuthContext,
    dave: Uuid,
    dave_ctx: AuthContext,
    dev_activity: Uuid,
    meeting: Uuid,
}

async fn setup(pool: &sqlx::PgPool) -> T {
    let w = World::build(pool).await;
    exec(
        pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    // Dev (everyone's base role) gets nothing about time; time comes from
    // the two roles below.
    let lead = id(
        pool,
        "INSERT INTO roles (name, issues_visibility, time_entries_visibility) VALUES ('Lead','all','all') RETURNING id",
    )
    .await;
    grant(
        pool,
        lead,
        &[
            "view_issues",
            "view_time_entries",
            "log_time",
            "edit_time_entries",
            "log_time_for_other_users",
        ],
    )
    .await;
    let own = id(
        pool,
        "INSERT INTO roles (name, issues_visibility, time_entries_visibility) VALUES ('Own','all','own') RETURNING id",
    )
    .await;
    grant(
        pool,
        own,
        &[
            "view_issues",
            "view_time_entries",
            "log_time",
            "edit_own_time_entries",
        ],
    )
    .await;
    // alice leads WEB; bob and carol log their own time there.
    for (u, r) in [(w.alice, lead), (w.bob, own), (w.carol, own)] {
        let m: Uuid = sqlx::query_scalar(
            "SELECT id FROM members WHERE project_id = $1 AND principal_id = $2",
        )
        .bind(w.web)
        .bind(u)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO member_roles VALUES ($1, $2)")
            .bind(m)
            .bind(r)
            .execute(pool)
            .await
            .unwrap();
    }
    let dave = oxledger::auth::create_user(pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(pool, w.web, dave, w.dev).await;
    let bob_ctx = sign_in(pool, &w.policy, "bob").await;
    let carol_ctx = sign_in(pool, &w.policy, "carol").await;
    let dave_ctx = sign_in(pool, &w.policy, "dave").await;
    let dev_activity = id(
        pool,
        "SELECT id FROM time_entry_activities WHERE is_default",
    )
    .await;
    let meeting = id(
        pool,
        "SELECT id FROM time_entry_activities WHERE name = '會議'",
    )
    .await;
    exec(
        pool,
        "UPDATE org_settings SET timelog_max_hours_per_day = 0, timelog_accept_0_hours = true,
                timelog_accept_future_dates = true, timelog_accept_closed_issues = true,
                timelog_require_issue = false, timelog_require_comments = false",
    )
    .await;
    exec(pool, "UPDATE time_entry_activities SET active = true").await;
    T {
        w,
        lead,
        own,
        bob_ctx,
        carol_ctx,
        dave,
        dave_ctx,
        dev_activity,
        meeting,
    }
}

fn doc(ops: Vec<Value>, pre: Vec<Value>) -> Value {
    json!({"command_schema_version": 1, "title": "time", "ops": ops, "preconditions": pre})
}

fn log(issue: Option<Uuid>, project: Option<Uuid>, fields: Value) -> Value {
    let mut o = json!({"op_id": op(), "kind": "log_time", "fields": fields});
    if let Some(i) = issue {
        o["issue"] = tgt(i);
    }
    if let Some(p) = project {
        o["project_id"] = json!(p);
    }
    o
}

fn upd(entry: Uuid, lv: i32, set: Value) -> Value {
    doc(
        vec![json!({"op_id": op(), "kind": "update_time_entry",
                    "target": {"type": "time_entry", "id": entry}, "set": set})],
        vec![json!({"target": {"type": "time_entry", "id": entry}, "lock_version": lv})],
    )
}

fn del(entry: Uuid, lv: i32) -> Value {
    doc(
        vec![json!({"op_id": op(), "kind": "delete_time_entry",
                    "target": {"type": "time_entry", "id": entry}})],
        vec![json!({"target": {"type": "time_entry", "id": entry}, "lock_version": lv})],
    )
}

async fn cmd(w: &World, who: AuthContext, d: &Value) -> anyhow::Result<CommandResult> {
    commands::submit(&w.app, who, Uuid::now_v7(), d).await
}

/// Logs `hours` for `who` on `issue` (or in WEB without one); the entry id.
async fn log_as(t: &T, who: AuthContext, issue: Option<Uuid>, fields: Value) -> Uuid {
    let project = issue.is_none().then_some(t.w.web);
    let r = cmd(&t.w, who, &doc(vec![log(issue, project, fields)], vec![]))
        .await
        .unwrap();
    r.time_entries[0].id
}

async fn issue_lv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// (hours in hundredths) of the daily total row, 0 when missing.
async fn total(w: &World, user: Uuid, day: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT (hours * 100)::bigint FROM time_daily_totals
                          WHERE user_id = $1 AND spent_on = $2::date), 0)",
    )
    .bind(user)
    .bind(day)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

/// The daily totals equal the sum of live entries, for every row.
async fn totals_consistent(w: &World) {
    let bad: i64 = count(
        &w.pool,
        "SELECT count(*) FROM (
            SELECT COALESCE(t.user_id, e.user_id) u, COALESCE(t.spent_on, e.spent_on) d,
                   COALESCE(t.hours, 0) th, COALESCE(e.h, 0) eh
            FROM time_daily_totals t
            FULL JOIN (SELECT user_id, spent_on, sum(hours) h FROM time_entries
                       WHERE deleted_at IS NULL GROUP BY 1, 2) e
              ON e.user_id = t.user_id AND e.spent_on = t.spent_on) x
         WHERE th <> eh",
    )
    .await;
    assert_eq!(bad, 0, "time_daily_totals drifted from the entries");
}

fn details(e: &anyhow::Error) -> Value {
    classify(e)
        .and_then(|a| a.details.clone())
        .unwrap_or(Value::Null)
}

fn reason(e: &anyhow::Error) -> String {
    details(e)["reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn lq() -> ListQuery {
    ListQuery::default()
}

async fn visible_ids(t: &T, who: AuthContext, q: ListQuery) -> Vec<Uuid> {
    time_reports::list(&t.w.app, who, &q)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|e| e.id)
        .collect()
}

async fn private_issue(w: &World, project: Uuid, author: Uuid) -> Uuid {
    let i = w.issue(project, "private").await;
    sqlx::query("UPDATE issues SET is_private = true, author_id = $2 WHERE id = $1")
        .bind(i)
        .bind(author)
        .execute(&w.pool)
        .await
        .unwrap();
    i
}

// ---------------------------------------------------------------- T-M1-31

#[tokio::test]
async fn t31_permission_matrix() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "perm").await;
    let h = |x: &str| json!({"hours": x, "spent_on": "2026-11-02"});

    // log_time for self: bob (Own) yes; dave (Dev only) no.
    let bob_e = log_as(&t, t.bob_ctx, Some(i), h("1.00")).await;
    let e = cmd(
        w,
        t.dave_ctx,
        &doc(vec![log(Some(i), None, h("1"))], vec![]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "permission");

    // For someone else: bob lacks log_time_for_other_users.
    let e = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![log(
                Some(i),
                None,
                json!({"hours": "1", "spent_on": "2026-11-02", "user_id": w.carol}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "permission", "{e:#}");
    // alice (Lead) may, for a member who may log time (carol) …
    let r = cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![log(
                Some(i),
                None,
                json!({"hours": "2", "spent_on": "2026-11-02", "user_id": w.carol}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap();
    let carol_e = r.time_entries[0].id;
    let author: Uuid = sqlx::query_scalar("SELECT author_id FROM time_entries WHERE id = $1")
        .bind(carol_e)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(author, w.alice);
    // … but not for dave (no log_time in WEB).
    let e = cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![log(
                Some(i),
                None,
                json!({"hours": "2", "spent_on": "2026-11-02", "user_id": t.dave}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "user_invalid");

    // Edit: bob his own (edit_own) yes, carol's no (he cannot even see it:
    // own visibility → not_found); alice (edit_time_entries) anyone's.
    cmd(w, t.bob_ctx, &upd(bob_e, 0, json!({"hours": "1.50"})))
        .await
        .unwrap();
    let e = cmd(w, t.bob_ctx, &upd(carol_e, 0, json!({"hours": "3"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    cmd(
        w,
        w.alice_ctx,
        &upd(carol_e, 0, json!({"comments": "fixed"})),
    )
    .await
    .unwrap();
    // An Own role that sees all time but may only edit its own.
    exec(
        &pool,
        &format!(
            "UPDATE roles SET time_entries_visibility = 'all' WHERE id = '{}'",
            t.own
        ),
    )
    .await;
    let e = cmd(w, t.bob_ctx, &upd(carol_e, 1, json!({"hours": "3"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(reason(&e), "permission");
    // Moving time to someone else needs log_time_for_other_users.
    let e = cmd(w, t.bob_ctx, &upd(bob_e, 1, json!({"user_id": w.carol})))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "permission");

    // Delete: same rule.
    let e = cmd(w, t.bob_ctx, &del(carol_e, 1)).await.unwrap_err();
    assert_eq!(reason(&e), "permission");
    cmd(w, t.bob_ctx, &del(bob_e, 1)).await.unwrap();
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM time_entries WHERE id = $1")
            .bind(bob_e)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted, "delete is a soft delete");
    // A deleted entry is gone for reads and writes.
    let e = cmd(w, w.alice_ctx, &upd(bob_e, 2, json!({"hours": "1"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    totals_consistent(w).await;

    // Module off: every time op is forbidden_op (module_disabled).
    exec(
        &pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'time_tracking'",
            w.web
        ),
    )
    .await;
    let e = cmd(w, t.bob_ctx, &doc(vec![log(Some(i), None, h("1"))], vec![]))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "module_disabled", "{e:#}");
    let e = cmd(w, w.alice_ctx, &upd(carol_e, 1, json!({"hours": "3"})))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "module_disabled", "{e:#}");
    let e = cmd(w, w.alice_ctx, &del(carol_e, 1)).await.unwrap_err();
    assert_eq!(reason(&e), "module_disabled", "{e:#}");
}

// ---------------------------------------------------------------- T-M1-32

#[tokio::test]
async fn t32_visibility() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "vis").await;
    let b = log_as(
        &t,
        t.bob_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    let c = log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "2", "spent_on": "2026-11-02"}),
    )
    .await;
    // own: each sees only their own; all: alice sees both.
    assert_eq!(visible_ids(&t, t.bob_ctx, lq()).await, vec![b]);
    assert_eq!(visible_ids(&t, t.carol_ctx, lq()).await, vec![c]);
    let mut all = visible_ids(&t, w.alice_ctx, lq()).await;
    all.sort();
    let mut want = vec![b, c];
    want.sort();
    assert_eq!(all, want);
    // dave has no view_time_entries: nothing, and the report says so.
    assert!(visible_ids(&t, t.dave_ctx, lq()).await.is_empty());
    let e = time_reports::summary(
        &w.app,
        t.dave_ctx,
        &SummaryQuery {
            project: Some("WEB".into()),
            from: Some("2026-11-01".into()),
            to: Some("2026-11-30".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "time_entries_not_visible");
    // Single read: not_found for an invisible entry.
    let e = time_reports::get(&w.app, t.bob_ctx, &c.to_string())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let v = time_reports::get(&w.app, w.alice_ctx, &b.to_string())
        .await
        .unwrap();
    assert_eq!(v.hours, "1.00");
    assert_eq!(v.can_edit, Some(true));

    // Time on a private issue carol cannot see: it does not exist for her,
    // even with all visibility, and her totals do not count it.
    exec(
        &pool,
        &format!(
            "UPDATE roles SET time_entries_visibility = 'all' WHERE id = '{}'",
            t.own
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE roles SET issues_visibility = 'default' WHERE id IN ('{}', '{}')",
            t.own, w.dev
        ),
    )
    .await;
    let p = private_issue(w, w.web, w.bob).await;
    let hidden = log_as(
        &t,
        t.bob_ctx,
        Some(p),
        json!({"hours": "4", "spent_on": "2026-11-02"}),
    )
    .await;
    let carol_sees = visible_ids(&t, t.carol_ctx, lq()).await;
    assert!(!carol_sees.contains(&hidden));
    assert!(carol_sees.contains(&b));
    let s = time_reports::summary(
        &w.app,
        t.carol_ctx,
        &SummaryQuery {
            from: Some("2026-11-01".into()),
            to: Some("2026-11-30".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        s.total, "3.00",
        "1 (bob) + 2 (carol); the private 4 is left out"
    );
    let e = time_reports::get(&w.app, t.carol_ctx, &hidden.to_string())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
}

// ---------------------------------------------------------------- T-M1-33, 34

#[tokio::test]
async fn t33_daily_limit_concurrent() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    exec(
        &pool,
        "UPDATE org_settings SET timelog_max_hours_per_day = 8",
    )
    .await;
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let app = w.app.clone();
        let who = t.bob_ctx;
        let web = w.web;
        tasks.push(tokio::spawn(async move {
            commands::submit(
                &app,
                who,
                Uuid::now_v7(),
                &doc(
                    vec![log(
                        None,
                        Some(web),
                        json!({"hours": "5", "spent_on": "2026-11-03"}),
                    )],
                    vec![],
                ),
            )
            .await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match common::join("a parallel time entry write", task).await {
            Ok(_) => ok += 1,
            Err(e) => {
                assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
                assert_eq!(details(&e)["kind"], "max_hours_per_day");
                assert_eq!(details(&e)["remaining"], "3.00");
            }
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(total(w, w.bob, "2026-11-03").await, 500);
    totals_consistent(w).await;
}

#[tokio::test]
async fn t34_remaining_only_when_visible() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    exec(
        &pool,
        "UPDATE org_settings SET timelog_max_hours_per_day = 8",
    )
    .await;
    // carol has 6 hours, 2 of them on a private issue alice cannot see.
    log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "4", "spent_on": "2026-11-04"}),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE roles SET issues_visibility = 'default' WHERE id IN ('{}', '{}', '{}')",
            t.own, w.dev, t.lead
        ),
    )
    .await;
    let p = private_issue(w, w.web, w.carol).await;
    log_as(
        &t,
        t.carol_ctx,
        Some(p),
        json!({"hours": "2", "spent_on": "2026-11-04"}),
    )
    .await;
    // carol herself: remaining 2.
    let e = cmd(
        w,
        t.carol_ctx,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "3", "spent_on": "2026-11-04"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "max_hours_per_day");
    assert_eq!(details(&e)["remaining"], "2.00");
    // alice for carol, without seeing all of carol's day: no remaining.
    let e = cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "3", "spent_on": "2026-11-04", "user_id": w.carol}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "max_hours_per_day");
    assert!(details(&e).get("remaining").is_none(), "{}", details(&e));
}

// ---------------------------------------------------------------- T-M1-35

#[tokio::test]
async fn t35_rules() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "rules").await;
    let try_log = |fields: Value, issue: Option<Uuid>| {
        let d = doc(
            vec![log(issue, issue.is_none().then_some(w.web), fields)],
            vec![],
        );
        async move { cmd(w, t.bob_ctx, &d).await }
    };
    // T1
    for bad in ["1.234", "-1", "abc", "1000.01", "1."] {
        let e = try_log(json!({"hours": bad, "spent_on": "2026-11-02"}), None)
            .await
            .unwrap_err();
        assert_eq!(details(&e)["kind"], "hours_invalid", "{bad}");
    }
    let e = try_log(json!({"hours": 1.5, "spent_on": "2026-11-02"}), None)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "hours_invalid");
    exec(
        &pool,
        "UPDATE org_settings SET timelog_accept_0_hours = false",
    )
    .await;
    let e = try_log(json!({"hours": "0", "spent_on": "2026-11-02"}), None)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "hours_invalid");
    // T2
    exec(
        &pool,
        "UPDATE org_settings SET timelog_accept_future_dates = false",
    )
    .await;
    let e = try_log(json!({"hours": "1", "spent_on": "2199-01-01"}), None)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "future_date");
    // T3: an inactive activity, and one turned off in the project.
    exec(
        &pool,
        &format!(
            "UPDATE time_entry_activities SET active = false WHERE id = '{}'",
            t.meeting
        ),
    )
    .await;
    let e = try_log(
        json!({"hours": "1", "spent_on": "2026-11-02", "activity_id": t.meeting}),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "activity_inactive");
    // Keeping an inactive activity on an existing entry is fine.
    exec(
        &pool,
        &format!(
            "UPDATE time_entry_activities SET active = true WHERE id = '{}'",
            t.meeting
        ),
    )
    .await;
    let kept = log_as(
        &t,
        t.bob_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02", "activity_id": t.meeting}),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO project_time_activities VALUES ('{}', '{}', false)",
            w.web, t.meeting
        ),
    )
    .await;
    cmd(w, t.bob_ctx, &upd(kept, 0, json!({"hours": "2"})))
        .await
        .unwrap();
    let e = cmd(
        w,
        t.bob_ctx,
        &upd(kept, 1, json!({"activity_id": t.dev_activity})),
    )
    .await;
    assert!(e.is_ok(), "an available activity may be chosen");
    let e = cmd(
        w,
        t.bob_ctx,
        &upd(kept, 2, json!({"activity_id": t.meeting})),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "activity_inactive");
    // T4
    exec(
        &pool,
        "UPDATE org_settings SET timelog_require_issue = true, timelog_require_comments = true",
    )
    .await;
    let e = try_log(json!({"hours": "1", "spent_on": "2026-11-02"}), None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::RequiredFieldMissing);
    assert_eq!(details(&e)["fields"], json!(["issue"]));
    let e = try_log(json!({"hours": "1", "spent_on": "2026-11-02"}), Some(i))
        .await
        .unwrap_err();
    assert_eq!(details(&e)["fields"], json!(["comments"]));
    exec(
        &pool,
        "UPDATE org_settings SET timelog_require_issue = false, timelog_require_comments = false",
    )
    .await;
    // T5: an issue closed before and after.
    exec(
        &pool,
        "UPDATE org_settings SET timelog_accept_closed_issues = false",
    )
    .await;
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(i)
        .bind(w.done)
        .execute(&pool)
        .await
        .unwrap();
    let e = try_log(json!({"hours": "1", "spent_on": "2026-11-02"}), Some(i))
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "closed_issue");
    // T6 in t33; T7:
    let e = try_log(
        json!({"hours": "1", "spent_on": "2026-11-02", "comments": "x".repeat(1025)}),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["field"], "comments");
    // Fields that cannot be set.
    for f in ["author_id", "project_id", "created_on_behalf_of_id"] {
        let mut fields = json!({"hours": "1", "spent_on": "2026-11-02"});
        fields[f] = json!(w.alice);
        let e = try_log(fields, None).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "{f}");
    }
    totals_consistent(w).await;
}

// ---------------------------------------------------------------- T-M1-36, 37

#[tokio::test]
async fn t36_t37_documents() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "mixed").await;
    let lv = issue_lv(w, i).await;
    // log_time with update_fields and add_note on the same issue.
    let r = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "mixed 2"}}),
                log(Some(i), None, json!({"hours": "1.25", "spent_on": "2026-11-02"})),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "logged"}),
            ],
            vec![json!({"target": tgt(i), "lock_version": lv})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.items.len(), 3);
    assert!(r.items[1].time_entry_id.is_some());
    assert_eq!(r.time_entries.len(), 1);
    assert_eq!(issue_lv(w, i).await, lv + 1);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM time_entries WHERE issue_id = '{i}'")
        )
        .await,
        1
    );
    // Another issue next to log_time: invalid_request.
    let j = w.issue(w.web, "other").await;
    let e = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![
                log(
                    Some(i),
                    None,
                    json!({"hours": "1", "spent_on": "2026-11-02"}),
                ),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(j), "body": "x"}),
            ],
            vec![json!({"target": tgt(j), "lock_version": issue_lv(w, j).await})],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
    // update_time_entry with an issue op: invalid_request.
    let entry = r.time_entries[0].id;
    let e = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![
                json!({"op_id": op(), "kind": "update_time_entry",
                       "target": {"type": "time_entry", "id": entry}, "set": {"hours": "2"}}),
                json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "x"}),
            ],
            vec![
                json!({"target": {"type": "time_entry", "id": entry}, "lock_version": 0}),
                json!({"target": tgt(i), "lock_version": issue_lv(w, i).await}),
            ],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
    // A time edit needs its precondition.
    let e = cmd(
        w,
        t.bob_ctx,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_time_entry",
                        "target": {"type": "time_entry", "id": entry}, "set": {"hours": "2"}})],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
    // A stale lock_version: precondition_failed.
    let e = cmd(w, t.bob_ctx, &upd(entry, 5, json!({"hours": "2"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PreconditionFailed);

    // T-M1-37: only time — the issue does not change, and a pending field
    // proposal on it still applies without a conflict.
    let lv = issue_lv(w, i).await;
    let events_before = count(
        &pool,
        &format!("SELECT count(*) FROM issue_events WHERE issue_id = '{i}'"),
    )
    .await;
    log_as(
        &t,
        t.bob_ctx,
        Some(i),
        json!({"hours": "0.5", "spent_on": "2026-11-02"}),
    )
    .await;
    assert_eq!(issue_lv(w, i).await, lv);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issue_events WHERE issue_id = '{i}'")
        )
        .await,
        events_before
    );
    // A person's field change written with the old lock_version still applies.
    cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![json!({"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": {"subject": "still fine"}})],
            vec![json!({"target": tgt(i), "lock_version": lv})],
        ),
    )
    .await
    .unwrap();
    totals_consistent(w).await;
}

// ---------------------------------------------------------------- agents (T-M1-38, 39, 39a, 39b, 40)

async fn delegate_time(
    t: &T,
    read_own_time: bool,
    policies: Option<PolicyInput>,
) -> agents::DelegationView {
    agents::create_delegation(
        &t.w.pool,
        &t.w.policy,
        &t.w.alice_ctx,
        DelegationInput {
            agent_client_id: t.w.client,
            project_id: t.w.web,
            proposal_ops: ["add_note", "log_time", "update_time_entry"]
                .map(String::from)
                .to_vec(),
            writable_fields: Vec::new(),
            max_ops: None,
            expires_in_days: Some(30),
            policies,
            read_own_time,
            read_own_time_ack: read_own_time,
        },
    )
    .await
    .unwrap()
}

async fn submit(w: &World, agent: AuthContext, d: &Value) -> anyhow::Result<SubmitResult> {
    exec(&w.pool, "DELETE FROM agent_write_log").await;
    changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN).await
}

#[tokio::test]
async fn t38_agent_writes() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let d = delegate_time(&t, false, None).await;
    assert!(!d.read_own_time);
    assert_eq!(
        d.read_own_time_warning.title,
        "這個 agent 可以讀你自己的工時紀錄，包括說明文字。"
    );
    // §4.3: time commands default to review.
    let modes: Vec<(String, String)> = sqlx::query_as(
        "SELECT command_kind, mode::text FROM delegation_command_policies WHERE delegation_id = $1 ORDER BY 1",
    )
    .bind(d.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        modes,
        vec![
            ("add_note".into(), "direct".into()),
            ("log_time".into(), "review".into()),
            ("update_time_entry".into(), "review".into()),
        ]
    );
    let (_, agent) = w.token(d.context_id).await;
    let i = w.issue(w.web, "agent time").await;

    // For the delegator: review_required, then approved → external_agent.
    let r = submit(
        w,
        agent,
        &doc(
            vec![log(
                Some(i),
                None,
                json!({"hours": "1", "spent_on": "2026-11-02"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap();
    let cs = match r {
        SubmitResult::ReviewRequired { proposal, .. } => proposal.changeset_id,
        other => panic!("expected review, got {other:?}"),
    };
    let view = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
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
    let (entry, user, author, obo): (Uuid, Uuid, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT id, user_id, author_id, created_on_behalf_of_id FROM time_entries WHERE issue_id = $1",
    )
    .bind(i)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(user, w.alice);
    assert_ne!(author, w.alice, "the agent is the author");
    assert_eq!(obo, Some(w.alice));
    let origin: String = sqlx::query_scalar(
        "SELECT origin FROM time_entry_events WHERE time_entry_id = $1 AND seq = 1",
    )
    .bind(entry)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(origin, "external_agent");

    // Direct when the delegator allows it; the receipt has ids only.
    agents::set_delegation_policies(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        PolicyInput {
            commands: BTreeMap::from([
                ("log_time".to_string(), PolicyMode::Direct),
                ("update_time_entry".to_string(), PolicyMode::Direct),
            ]),
            fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    let r = submit(
        w,
        agent,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "2", "spent_on": "2026-11-02"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap();
    let own_entry = match r {
        SubmitResult::Applied { time_entries, .. } => time_entries[0].id,
        other => panic!("expected applied, got {other:?}"),
    };
    // For someone else: agent_time_scope.
    let e = submit(
        w,
        agent,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "1", "spent_on": "2026-11-02", "user_id": w.carol}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "agent_time_scope", "{e:#}");
    // Changing someone else's time (alice sees carol's, all): agent_time_scope.
    let carol_e = log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    let e = submit(w, agent, &upd(carol_e, 0, json!({"hours": "3"})))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "agent_time_scope", "{e:#}");
    // Moving the delegator's time to someone else: agent_time_scope.
    let e = submit(w, agent, &upd(own_entry, 0, json!({"user_id": w.carol})))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "agent_time_scope", "{e:#}");
    // Its own (the delegator's) time: fine.
    let r = submit(w, agent, &upd(own_entry, 0, json!({"hours": "2.5"})))
        .await
        .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");
    // Delete is not an agent command, whatever the policy.
    let e = submit(w, agent, &del(own_entry, 1)).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest, "{e:#}");
    // log_time quota: 20 an hour.
    exec(&pool, "DELETE FROM agent_write_log").await;
    for _ in 0..20 {
        exec(
            &pool,
            &format!(
                "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
                 SELECT agent_principal_id, context_id, id, 'time_write' FROM delegations WHERE id = '{}'",
                d.id
            ),
        )
        .await;
    }
    let e = changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "1", "spent_on": "2026-11-05"}),
            )],
            vec![],
        ),
        false,
        ORIGIN,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded, "{e:#}");
    totals_consistent(w).await;
}

#[tokio::test]
async fn t39_agent_reads_own_time() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "read").await;
    exec(
        &pool,
        &format!(
            "UPDATE roles SET issues_visibility = 'default' WHERE id IN ('{}', '{}')",
            w.dev, t.lead
        ),
    )
    .await;
    let p = private_issue(w, w.web, w.alice).await;
    let mine = log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-02", "comments": "SSO 重現"}),
    )
    .await;
    let mine_private = log_as(
        &t,
        w.alice_ctx,
        Some(p),
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    let carols = log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    // alice's time in AUTH (not delegated).
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{}' FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            t.lead, w.auth_p, w.alice
        ),
    )
    .await;
    let other_project = cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![log(
                None,
                Some(w.auth_p),
                json!({"hours": "1", "spent_on": "2026-11-02"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap()
    .time_entries[0]
        .id;
    assert!(visible_ids(&t, w.alice_ctx, lq()).await.contains(&carols));

    // Off: forbidden_op time_read_not_delegated.
    let d = delegate_time(&t, false, None).await;
    let (_, agent) = w.token(d.context_id).await;
    let e = time_reports::list_for_tool(&w.app, agent, &lq())
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "time_read_not_delegated");
    // On: only the delegator's, in the delegated project, not on private issues.
    agents::set_read_own_time(&w.pool, &w.policy, &w.alice_ctx, d.id, true, true)
        .await
        .unwrap();
    let got: Vec<Uuid> = time_reports::list_for_tool(&w.app, agent, &lq())
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(got, vec![mine]);
    assert!(
        !got.contains(&mine_private) && !got.contains(&carols) && !got.contains(&other_project)
    );
    // No reports for agents.
    let e = time_reports::summary(
        &w.app,
        agent,
        &SummaryQuery {
            from: Some("2026-11-01".into()),
            to: Some("2026-11-30".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // T-M1-39a: the A1(c) exception — the agent copies its delegator's time
    // comment into a public note of the same project, and that succeeds.
    let comment = time_reports::list_for_tool(&w.app, agent, &lq())
        .await
        .unwrap()
        .items[0]
        .comments
        .clone();
    let lv = issue_lv(w, i).await;
    let r = submit(
        w,
        agent,
        &doc(
            vec![json!({"op_id": op(), "kind": "add_note", "target": tgt(i), "body": format!("週報：{comment}")})],
            vec![json!({"target": tgt(i), "lock_version": lv})],
        ),
    )
    .await
    .unwrap();
    assert!(matches!(r, SubmitResult::Applied { .. }), "{r:?}");

    // T-M1-39b: on without the acknowledgement: 422; the CHECK holds too.
    let e = agents::set_read_own_time(&w.pool, &w.policy, &w.alice_ctx, d.id, true, false)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    let e = sqlx::query("UPDATE delegations SET read_own_time_ack_at = NULL WHERE id = $1")
        .bind(d.id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("23514")
    );
}

#[tokio::test]
async fn t39b_create_without_ack() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let e = agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: vec!["log_time".into()],
            writable_fields: Vec::new(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: true,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "ack_required");
}

// ---------------------------------------------------------------- T-M1-43 and the stress test

#[tokio::test]
async fn t43_move_day_and_user() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let e = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "3", "spent_on": "2026-11-02", "user_id": w.bob}),
    )
    .await;
    assert_eq!(total(w, w.bob, "2026-11-02").await, 300);
    cmd(
        w,
        w.alice_ctx,
        &upd(e, 0, json!({"spent_on": "2026-11-03"})),
    )
    .await
    .unwrap();
    assert_eq!(total(w, w.bob, "2026-11-02").await, 0);
    assert_eq!(total(w, w.bob, "2026-11-03").await, 300);
    cmd(
        w,
        w.alice_ctx,
        &upd(
            e,
            1,
            json!({"user_id": w.carol, "spent_on": "2026-11-04", "hours": "2"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(total(w, w.bob, "2026-11-03").await, 0);
    assert_eq!(total(w, w.carol, "2026-11-04").await, 200);
    // Moving to someone who may not log time here: user_invalid.
    let err = cmd(w, w.alice_ctx, &upd(e, 2, json!({"user_id": t.dave})))
        .await
        .unwrap_err();
    assert_eq!(details(&err)["kind"], "user_invalid");
    totals_consistent(w).await;
}

async fn deadlocks(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// 100 concurrent documents log, move (across days and people), change and
/// delete entries on two issues; none may deadlock (40P01 is never seen,
/// even when the engine would retry it), and the daily totals stay exact.
#[tokio::test]
async fn t43_lock_order_stress() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i1 = w.issue(w.web, "stress 1").await;
    let i2 = w.issue(w.web, "stress 2").await;
    let users = [w.alice, w.bob, w.carol];
    let days = ["2026-11-02", "2026-11-03", "2026-11-04"];
    let mut entries = Vec::new();
    for k in 0..12 {
        entries.push(
            log_as(
                &t,
                w.alice_ctx,
                Some(if k % 2 == 0 { i1 } else { i2 }),
                json!({"hours": "1", "spent_on": days[k % 3], "user_id": users[k % 3]}),
            )
            .await,
        );
    }
    // pg_stat counters are updated asynchronously; force a fresh snapshot.
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    let before = deadlocks(&pool).await;
    let mut tasks = Vec::new();
    for n in 0..100usize {
        let app = w.app.clone();
        let who = w.alice_ctx;
        let entry = entries[n % entries.len()];
        let issue = if n % 2 == 0 { i1 } else { i2 };
        let user = users[(n * 7) % 3];
        let day = days[(n * 5) % 3];
        let web = w.web;
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let d = match n % 4 {
                0 => doc(
                    vec![log(
                        Some(issue),
                        None,
                        json!({"hours": "0.25", "spent_on": day, "user_id": user}),
                    )],
                    vec![],
                ),
                1 | 2 => {
                    let lv: i32 =
                        sqlx::query_scalar("SELECT lock_version FROM time_entries WHERE id = $1")
                            .bind(entry)
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                    upd(
                        entry,
                        lv,
                        json!({"spent_on": day, "user_id": user, "hours": format!("{}.50", n % 3)}),
                    )
                }
                _ => doc(
                    vec![log(
                        None,
                        Some(web),
                        json!({"hours": "0.5", "spent_on": day, "user_id": user}),
                    )],
                    vec![],
                ),
            };
            commands::submit(&app, who, Uuid::now_v7(), &d).await
        }));
    }
    for task in tasks {
        if let Err(e) = common::join("a parallel time entry write", task).await {
            let c = code(&e);
            assert!(
                matches!(c, ErrorCode::PreconditionFailed | ErrorCode::NotFound),
                "unexpected failure: {e:#}"
            );
            assert!(!format!("{e:#}").contains("40P01"), "deadlock: {e:#}");
        }
    }
    // Deletes too, in parallel.
    let mut tasks = Vec::new();
    for e in entries.iter().take(6).copied() {
        let app = w.app.clone();
        let who = w.alice_ctx;
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM time_entries WHERE id = $1")
                .bind(e)
                .fetch_one(&pool)
                .await
                .unwrap();
            commands::submit(&app, who, Uuid::now_v7(), &del(e, lv)).await
        }));
    }
    for task in tasks {
        common::join("a parallel time entry write", task)
            .await
            .unwrap();
    }
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    // Give the stats collector a moment (counters are flushed at commit).
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    assert_eq!(
        deadlocks(&pool).await,
        before,
        "a deadlock (40P01) happened"
    );
    totals_consistent(w).await;
}

// ---------------------------------------------------------------- T-M1-44 and the timesheet

#[tokio::test]
async fn t44_summary_and_timesheet() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "report").await;
    log_as(
        &t,
        t.bob_ctx,
        Some(i),
        json!({"hours": "1.5", "spent_on": "2026-11-09"}),
    )
    .await;
    log_as(
        &t,
        t.bob_ctx,
        None,
        json!({"hours": "2", "spent_on": "2026-11-10", "activity_id": t.meeting}),
    )
    .await;
    log_as(
        &t,
        t.carol_ctx,
        Some(i),
        json!({"hours": "8", "spent_on": "2026-11-10"}),
    )
    .await;
    let q = |g: &str| SummaryQuery {
        project: Some("WEB".into()),
        from: Some("2026-11-09".into()),
        to: Some("2026-11-15".into()),
        group_by: Some(g.into()),
        ..Default::default()
    };
    let s = time_reports::summary(&w.app, w.alice_ctx, &q("user"))
        .await
        .unwrap();
    assert_eq!(s.total, "11.50");
    assert_eq!(s.scope_note, "only_visible");
    assert_eq!(s.rows.len(), 2);
    let s = time_reports::summary(&w.app, t.bob_ctx, &q("user"))
        .await
        .unwrap();
    assert_eq!(s.total, "3.50");
    assert_eq!(s.rows.len(), 1);
    let s = time_reports::summary(&w.app, t.carol_ctx, &q("activity"))
        .await
        .unwrap();
    assert_eq!(s.total, "8.00");
    for g in ["issue", "tracker", "fixed_version", "day", "week", "month"] {
        let s = time_reports::summary(&w.app, w.alice_ctx, &q(g))
            .await
            .unwrap();
        assert_eq!(s.total, "11.50", "{g}");
    }
    let s = time_reports::summary(&w.app, w.alice_ctx, &q("week"))
        .await
        .unwrap();
    assert_eq!(s.rows[0].key.as_deref(), Some("2026-W46"));
    // More than 366 days.
    let e = time_reports::summary(
        &w.app,
        w.alice_ctx,
        &SummaryQuery {
            from: Some("2025-01-01".into()),
            to: Some("2026-11-15".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    // The timesheet: bob's own rows only, Monday first (week_start 1).
    let ts = time_reports::timesheet(&w.app, t.bob_ctx, Some("2026-W46"))
        .await
        .unwrap();
    assert_eq!(ts.days[0].to_string(), "2026-11-09");
    assert_eq!(ts.rows.len(), 2);
    assert_eq!(ts.week_total, "3.50");
    assert_eq!(ts.day_totals[0], "1.50");
    assert_eq!(ts.day_totals[1], "2.00");
    assert_eq!(ts.day_totals[2], "0");
    exec(&pool, "UPDATE org_settings SET week_start = 7").await;
    let ts = time_reports::timesheet(&w.app, t.bob_ctx, Some("2026-W46"))
        .await
        .unwrap();
    assert_eq!(ts.days[0].to_string(), "2026-11-15", "Sunday first");
    exec(&pool, "UPDATE org_settings SET week_start = 1").await;
}

// ---------------------------------------------------------------- CHECK lists (§2.1)

#[tokio::test]
async fn check_lists_match_constants() {
    let pool = test_pool().await;
    for (table, constraint) in [
        (
            "delegation_command_policies",
            "delegation_command_policies_command_kind_check",
        ),
        ("changeset_version_ops", "changeset_version_ops_kind_check"),
    ] {
        let def: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c
             JOIN pg_class t ON t.oid = c.conrelid WHERE t.relname = $1 AND c.conname = $2",
        )
        .bind(table)
        .bind(constraint)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("{table}.{constraint}: {e}"));
        for op in agents::PROPOSAL_OPS {
            assert!(
                def.contains(&format!("'{op}'")),
                "{constraint} lacks {op}: {def}"
            );
        }
    }
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'delegations_proposal_ops_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for op in agents::PROPOSAL_OPS {
        assert!(
            def.contains(&format!("'{op}'")),
            "delegations_proposal_ops lacks {op}: {def}"
        );
    }
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'agent_write_log_kind_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for k in [
        "direct_write",
        "issue_create",
        "issue_write",
        "propose",
        "audience_blocked",
        "version_create",
        "version_write",
        "validate",
        "time_write",
    ] {
        assert!(
            def.contains(&format!("'{k}'")),
            "agent_write_log lacks {k}: {def}"
        );
    }
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
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn m1_4_http() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    exec(
        &pool,
        &format!(
            "UPDATE principals SET is_admin = true WHERE id = '{}'",
            w.alice
        ),
    )
    .await;
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    );
    let alice = login(&r, "alice").await;
    let bob = login(&r, "bob").await;
    for path in [
        "/time-entries",
        "/time-entries/summary",
        "/me/timesheet",
        "/time-activities",
        "/org/timelog-settings",
    ] {
        let (st, v) = call(&r, None, "GET", path, None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{path}: {v}");
    }
    let i = w.issue(w.web, "http").await;
    // Log through /commands; read it back.
    let (st, v) = call(
        &r,
        Some(&bob),
        "POST",
        "/commands",
        Some(doc(
            vec![log(
                Some(i),
                None,
                json!({"hours": "1.50", "spent_on": "2026-11-09", "comments": "重現 Safari 問題"}),
            )],
            vec![],
        )),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let entry = v["data"]["time_entries"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(v["data"]["time_entries"][0]["kind"], "time_entry");
    assert_eq!(v["data"]["items"][0]["time_entry_id"], entry.as_str());
    let (st, v) = call(
        &r,
        Some(&bob),
        "GET",
        &format!("/time-entries/{entry}"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["hours"], "1.50");
    assert_eq!(v["data"]["activity"]["name"], "開發");
    assert_eq!(v["data"]["can_delete"], true);
    let (st, v) = call(
        &r,
        Some(&bob),
        "GET",
        "/time-entries?user=me&project=WEB",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 1);
    let (st, v) = call(
        &r,
        Some(&bob),
        "GET",
        &format!("/time-entries/{entry}/events"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"][0]["kind"], "created");
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/time-entries/summary?from=2026-11-09&to=2026-11-15&group_by=user",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["total"], "1.50");
    let (st, v) = call(
        &r,
        Some(&bob),
        "GET",
        "/me/timesheet?week=2026-W46",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["week_total"], "1.50");
    let (st, v) = call(
        &r,
        Some(&bob),
        "GET",
        "/time-activities?project=WEB",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 5);
    // Settings: bob may read but not change; alice (admin) may.
    let (st, v) = call(&r, Some(&bob), "GET", "/org/timelog-settings", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let lv = v["data"]["lock_version"].as_i64().unwrap();
    let body = json!({"lock_version": lv, "max_hours_per_day": "8", "accept_0_hours": true,
                      "accept_future_dates": true, "accept_closed_issues": true,
                      "require_issue": false, "require_comments": false});
    let (st, _) = call(
        &r,
        Some(&bob),
        "PUT",
        "/org/timelog-settings",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/org/timelog-settings",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["max_hours_per_day"], "8.00");
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/org/timelog-settings",
        Some(body),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    // Project settings: turn an activity off.
    let (st, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/projects/WEB/time-activities",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["module_enabled"], true);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/projects/WEB/time-activities",
        Some(
            json!({"lock_version": 0, "overrides": [{"activity_id": t.meeting, "active": false}]}),
        ),
        None,
    )
    .await;
    // alice is admin, but the project needs manage_project for non-admins.
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = call(
        &r,
        Some(&bob),
        "GET",
        "/time-activities?project=WEB",
        None,
        None,
    )
    .await;
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 4);
    // Admin activities: removing one in use fails.
    let (_, v) = call(&r, Some(&alice), "GET", "/time-activities", None, None).await;
    let items: Vec<Value> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["name"] != "開發")
        .map(|a| {
            json!({"id": a["id"], "name": a["name"], "position": a["position"],
                         "active": a["active"], "is_default": a["is_default"]})
        })
        .collect();
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        "/admin/time-activities",
        Some(json!({"items": items})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["details"]["kind"], "activity_in_use");
    // time-users: bob only himself; alice (log_time_for_other_users) the loggers.
    let (_, v) = call(
        &r,
        Some(&bob),
        "GET",
        "/projects/WEB/time-users",
        None,
        None,
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    let (_, v) = call(
        &r,
        Some(&alice),
        "GET",
        "/projects/WEB/time-users",
        None,
        None,
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 3);
    // Delete through /commands.
    let (st, v) = call(
        &r,
        Some(&bob),
        "POST",
        "/commands",
        Some(del(entry.parse().unwrap(), 0)),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, _) = call(
        &r,
        Some(&bob),
        "GET",
        &format!("/time-entries/{entry}"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // The session advertises log_time.
    let (_, v) = call(&r, Some(&bob), "GET", "/auth/session", None, None).await;
    assert!(
        v["data"]["capabilities"]["command_features"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "log_time")),
        "{v}"
    );
    // POST /time-entries/{id}/redact (the entry is deleted; D6 still allows it).
    let (st, v) = call(
        &r,
        Some(&alice),
        "POST",
        &format!("/time-entries/{entry}/redact"),
        Some(json!({"reason_code": "confidential", "reason": "pasted by mistake"})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["changed"], true, "{v}");
    assert_eq!(
        text_count(
            &pool,
            "SELECT count(*) FROM time_entries WHERE comments LIKE $1",
            "Safari"
        )
        .await,
        0
    );
    let (st, v) = call(
        &r,
        Some(&bob),
        "POST",
        &format!("/time-entries/{entry}/redact"),
        Some(json!({"reason_code": "confidential", "reason": "x"})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert!(st.is_client_error(), "bob may not redact: {st} {v}");
    // PUT /delegations/{id}/read-own-time: on and off both bump policy_version
    // (§2.5) and need a recent step-up.
    let d = delegate_time(&t, false, None).await;
    let pv = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT policy_version FROM delegations WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let v0 = pv(d.id).await;
    let path = format!("/delegations/{}/read-own-time", d.id);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        &path,
        Some(json!({"enabled": true, "ack": false})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(pv(d.id).await, v0);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        &path,
        Some(json!({"enabled": true, "ack": true})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["read_own_time"], true, "{v}");
    assert_eq!(pv(d.id).await, v0 + 1);
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        &path,
        Some(json!({"enabled": false, "ack": false})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["read_own_time"], false, "{v}");
    assert_eq!(pv(d.id).await, v0 + 2);
    // bob cannot touch alice's delegation.
    let (st, _) = call(
        &r,
        Some(&bob),
        "PUT",
        &path,
        Some(json!({"enabled": false, "ack": false})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // A stale step-up is refused.
    exec(
        &pool,
        &format!(
            "UPDATE browser_sessions SET reauthenticated_at = now() - interval '1 day' WHERE user_id = '{}'",
            w.alice
        ),
    )
    .await;
    let (st, v) = call(
        &r,
        Some(&alice),
        "PUT",
        &path,
        Some(json!({"enabled": true, "ack": true})),
        None,
    )
    .await;
    assert_eq!(v["error"]["code"], "reauthentication_required", "{st} {v}");
    assert_eq!(pv(d.id).await, v0 + 2);
}

// ---------------------------------------------------------------- T-M1-40

/// `spent_hours` everywhere a person reads it (issue detail, `get` fields,
/// the `where` field, view columns and totals) counts only the time they
/// can see; agents get `invalid_argument` (`unavailable_to_agent`).
#[tokio::test]
async fn t40_spent_hours() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "spent").await;
    let key: String =
        sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap();
    log_as(
        &t,
        t.bob_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    log_as(
        &t,
        t.carol_ctx,
        Some(i),
        json!({"hours": "2", "spent_on": "2026-11-02"}),
    )
    .await;
    log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "4", "spent_on": "2026-11-03"}),
    )
    .await;
    let gone = log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "8", "spent_on": "2026-11-04"}),
    )
    .await;
    cmd(w, w.alice_ctx, &del(gone, 0)).await.unwrap();

    // Issue detail: all (alice) 7, own (bob) 1, none (dave) absent.
    let detail = |who| oxledger::service::get(&w.app, who, &key);
    assert_eq!(
        detail(w.alice_ctx).await.unwrap().spent_hours.as_deref(),
        Some("7.00")
    );
    assert_eq!(
        detail(t.bob_ctx).await.unwrap().spent_hours.as_deref(),
        Some("1.00")
    );
    assert_eq!(detail(t.dave_ctx).await.unwrap().spent_hours, None);
    // get with fields.
    let get = |who, fields: &[&str]| {
        oxledger::issue_view::get(
            &w.app,
            who,
            oxledger::issue_view::GetParams {
                key: key.clone(),
                fields: Some(fields.iter().map(|s| s.to_string()).collect()),
                citations: None,
                include: None,
                note_limit: None,
                event_limit: None,
            },
        )
    };
    let v = get(t.bob_ctx, &["spent_hours"]).await.unwrap();
    assert_eq!(v.spent_hours, Some(Some("1.00".into())));
    let v = get(t.dave_ctx, &["spent_hours"]).await.unwrap();
    assert_eq!(v.spent_hours, Some(None));
    // T-M1-40: an agent asking for it.
    let d = delegate_time(&t, true, None).await;
    let (_, agent) = w.token(d.context_id).await;
    let e = get(agent, &["key", "spent_hours"]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    assert_eq!(reason(&e), "unavailable_to_agent");
    // Without fields the agent's detail has none.
    let v = oxledger::issue_view::get(
        &w.app,
        agent,
        oxledger::issue_view::GetParams {
            key: key.clone(),
            fields: None,
            citations: None,
            include: None,
            note_limit: None,
            event_limit: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(v.spent_hours, None);

    // MCP time_list: only alice's own (live) entry.
    let page = time_reports::agent_list(
        &w.app,
        agent,
        time_reports::TimeListParams {
            from: "2026-11-01".into(),
            to: "2026-11-30".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got: Vec<(Uuid, String)> = page
        .items
        .iter()
        .map(|e| (e.user.id, e.hours.clone()))
        .collect();
    assert_eq!(got, vec![(w.alice, "4.00".to_string())]);

    // where spent_hours: what the viewer sees decides.
    let other = w.issue(w.web, "nothing logged").await;
    let search = |who, v: Value| {
        oxledger::service::search(
            &w.app,
            who,
            oxledger::service::SearchParams {
                where_: Some(v),
                limit: Some(100),
                ..Default::default()
            },
        )
    };
    let ids = |r: oxledger::service::SearchResult| -> Vec<Uuid> {
        r.items.iter().map(|x| x.id).collect()
    };
    let gt = json!({"field": "spent_hours", "op": "gt", "value": "1.5"});
    assert_eq!(ids(search(w.alice_ctx, gt.clone()).await.unwrap()), vec![i]);
    assert!(
        ids(search(t.bob_ctx, gt.clone()).await.unwrap()).is_empty(),
        "bob sees 1 hour"
    );
    let eq0 = json!({"and": [{"field": "spent_hours", "op": "eq", "value": "0"},
                             {"field": "project", "op": "eq", "value": "WEB"}]});
    let zero = ids(search(t.bob_ctx, eq0).await.unwrap());
    assert!(zero.contains(&other) && !zero.contains(&i));
    let e = search(agent, gt.clone()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    assert_eq!(reason(&e), "unavailable_to_agent");
    // filter-fields: people only.
    let names = |f: oxledger::filter::v2::FilterFields| -> Vec<String> {
        f.fields.into_iter().map(|x| x.field).collect()
    };
    assert!(
        names(
            oxledger::filter::v2::filter_fields(&w.app, t.bob_ctx, None)
                .await
                .unwrap()
        )
        .contains(&"spent_hours".to_string())
    );
    assert!(
        !names(
            oxledger::filter::v2::filter_fields(&w.app, agent, None)
                .await
                .unwrap()
        )
        .contains(&"spent_hours".to_string())
    );

    // View column and total (R-VD7).
    let input = oxledger::views::PreviewInput {
        def: json!({"kind": "list", "columns": ["key", "spent_hours"], "totals": ["spent_hours", "estimated_hours"],
                            "query": {"where": {"field": "project", "op": "eq", "value": "WEB"}}}),
        scope: None,
        project_id: Some("WEB".into()),
        cursor: None,
        limit: None,
        lane: None,
        swimlane: None,
        override_: None,
    };
    for (who, cell, total) in [
        (w.alice_ctx, json!("7.00"), "7.00"),
        (t.bob_ctx, json!("1.00"), "1.00"),
        (t.dave_ctx, Value::Null, "0.00"),
    ] {
        let r = oxledger::views::preview(&w.app, who, &input).await.unwrap();
        let items = r.items.unwrap();
        let it = items.iter().find(|x| x.id == i).unwrap();
        assert_eq!(it.values.as_ref().unwrap()["spent_hours"], cell);
        let totals = r.totals.unwrap();
        assert_eq!(totals["spent_hours"].value.as_deref(), Some(total));
        assert!(totals["spent_hours"].complete);
    }
}

// ---------------------------------------------------------------- T-M1-41, 42

async fn agent_log(w: &World, agent: AuthContext, issue: Uuid, fields: Value) -> (Uuid, Uuid) {
    match submit(w, agent, &doc(vec![log(Some(issue), None, fields)], vec![]))
        .await
        .unwrap()
    {
        SubmitResult::Applied {
            execution_id,
            time_entries,
            ..
        } => (execution_id, time_entries[0].id),
        other => panic!("expected applied, got {other:?}"),
    }
}

async fn text_count(pool: &sqlx::PgPool, sql: &'static str, needle: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(format!("%{needle}%"))
        .fetch_one(pool)
        .await
        .unwrap()
}

fn why() -> RedactInput {
    RedactInput {
        reason_code: "confidential".into(),
        reason: "customer data pasted by mistake".into(),
    }
}

/// T-M1-41 (undo of the agent's time) and T-M1-42 (D12 on a time comment).
#[tokio::test]
async fn t41_t42_undo_and_redaction() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "undo time").await;
    let d = delegate_time(&t, false, None).await;
    let (_, agent) = w.token(d.context_id).await;
    agents::set_delegation_policies(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        d.id,
        PolicyInput {
            commands: BTreeMap::from([
                ("log_time".to_string(), PolicyMode::Direct),
                ("update_time_entry".to_string(), PolicyMode::Direct),
            ]),
            fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();

    // Nobody changed it: the undo deletes it (soft) and the day total drops.
    let (x1, e1) = agent_log(w, agent, i, json!({"hours": "2", "spent_on": "2026-11-02"})).await;
    assert_eq!(total(w, w.alice, "2026-11-02").await, 200);
    let pv = undo::preview(&w.app, w.alice_ctx, x1).await.unwrap();
    assert_eq!(pv.time_entries.len(), 1, "{pv:?}");
    assert_eq!(pv.time_entries[0].field, "created");
    assert!(pv.time_entries[0].restorable, "{pv:?}");
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x1,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.time_entries_restored, 1);
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM time_entries WHERE id = $1")
            .bind(e1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);
    assert_eq!(total(w, w.alice, "2026-11-02").await, 0);
    let (kind, exec): (String, Uuid) = sqlx::query_as(
        "SELECT kind, execution_id FROM time_entry_events WHERE time_entry_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(e1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((kind.as_str(), exec), ("deleted", u.execution_id));
    // Nothing left to undo there.
    let e = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x1,
        &UndoInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);

    // Someone changed the hours since: undo_not_applicable (changed_since).
    let (x2, e2) = agent_log(w, agent, i, json!({"hours": "3", "spent_on": "2026-11-03"})).await;
    cmd(w, w.alice_ctx, &upd(e2, 0, json!({"hours": "4"})))
        .await
        .unwrap();
    let e = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x2,
        &UndoInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    assert_eq!(
        details(&e)["reasons"][0]["reason"],
        json!("changed_since"),
        "{e:#}"
    );

    // An agent's field change goes back.
    let mine = log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-04"}),
    )
    .await;
    let x3 = match submit(
        w,
        agent,
        &upd(mine, 0, json!({"hours": "1.5", "spent_on": "2026-11-05"})),
    )
    .await
    .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("expected applied, got {other:?}"),
    };
    assert_eq!(total(w, w.alice, "2026-11-05").await, 150);
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x3,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.time_entries_restored, 1);
    let v = time_reports::get(&w.app, w.alice_ctx, &mine.to_string())
        .await
        .unwrap();
    assert_eq!(
        (v.hours.as_str(), v.spent_on.to_string().as_str()),
        ("1.00", "2026-11-04")
    );
    assert_eq!(total(w, w.alice, "2026-11-04").await, 100);
    assert_eq!(total(w, w.alice, "2026-11-05").await, 0);
    totals_consistent(w).await;

    // T-M1-42: redact a comment the agent wrote; the entry, its events and
    // the execution copies (this one and every one sourced from WEB) all
    // hold the marker afterwards, in one transaction.
    let (x4, e4) = agent_log(
        w,
        agent,
        i,
        json!({"hours": "1", "spent_on": "2026-11-06", "comments": "機密客戶名單"}),
    )
    .await;
    cmd(
        w,
        w.alice_ctx,
        &upd(e4, 0, json!({"comments": "機密客戶名單 v2"})),
    )
    .await
    .unwrap();
    grant(&pool, t.lead, &["manage_project"]).await;
    let snapshot_x2 = count(
        &pool,
        &format!(
            "SELECT count(*) FROM execution_time_entry_changes
             WHERE execution_id = '{x2}' AND after_value::text LIKE '%hours%'"
        ),
    )
    .await;
    assert_eq!(snapshot_x2, 1, "x2 still holds its snapshot");
    // Who may: not bob (no manage_project).
    let target = Target::TimeEntry { entry: e4 };
    let e = redaction::redact(
        &w.app,
        t.bob_ctx,
        &w.policy,
        Uuid::now_v7(),
        &target,
        &why(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(code(&e), ErrorCode::ForbiddenOp | ErrorCode::NotFound),
        "{e:#}"
    );
    let epoch_before = epoch(&pool, w.web).await;
    let r = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        Uuid::now_v7(),
        &target,
        &why(),
    )
    .await
    .unwrap();
    assert!(r.changed);
    assert_eq!(epoch(&pool, w.web).await, epoch_before + 1);
    let v = time_reports::get(&w.app, w.alice_ctx, &e4.to_string())
        .await
        .unwrap();
    assert_eq!(v.comments, "[已遮蔽]");
    assert!(v.redacted);
    assert_eq!(
        text_count(
            &pool,
            "SELECT count(*) FROM time_entry_events WHERE payload::text LIKE $1",
            "機密"
        )
        .await,
        0
    );
    assert_eq!(
        text_count(
            &pool,
            "SELECT count(*) FROM execution_time_entry_changes
             WHERE before_value::text LIKE $1 OR after_value::text LIKE $1",
            "機密"
        )
        .await,
        0
    );
    let redactions = count(
        &pool,
        &format!(
            "SELECT count(*) FROM time_entry_event_redactions r
             JOIN time_entry_events e ON e.id = r.event_id WHERE e.time_entry_id = '{e4}'"
        ),
    )
    .await;
    assert_eq!(redactions, 2, "the created and the updated event");
    // The other executions sourced from WEB lose their copies too.
    let marked = count(
        &pool,
        &format!(
            "SELECT count(*) FROM execution_time_entry_changes
             WHERE execution_id = '{x2}' AND after_value = to_jsonb('[已遮蔽]'::text)"
        ),
    )
    .await;
    assert_eq!(marked, 1);
    // The events still list the change, with the marker.
    let ev = time_reports::events(&w.app, w.alice_ctx, &e4.to_string())
        .await
        .unwrap();
    assert_eq!(ev[1].changes["comments"]["new"], json!("[已遮蔽]"));
    // Again: nothing new.
    let r = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        Uuid::now_v7(),
        &target,
        &why(),
    )
    .await
    .unwrap();
    assert!(!r.changed);
    // x4's entry was changed by alice since: changed_since.
    let pv = undo::preview(&w.app, w.alice_ctx, x4).await.unwrap();
    assert_eq!(
        pv.time_entries[0].reason.as_deref(),
        Some("changed_since"),
        "{pv:?}"
    );
    totals_consistent(w).await;
}

// ---------------------------------------------------------------- the review queue (§4.3)

/// A `log_time` proposal shows as a time row; short or no comments may be
/// ticked, longer ones and `update_time_entry` are opened one by one.
#[tokio::test]
async fn time_in_review_queue() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "queue time").await;
    let d = delegate_time(&t, false, None).await;
    let (_, agent) = w.token(d.context_id).await;
    let propose = |fields: Value| {
        let d = doc(vec![log(Some(i), None, fields)], vec![]);
        async move {
            match submit(w, agent, &d).await.unwrap() {
                SubmitResult::ReviewRequired { proposal, .. } => proposal.changeset_id,
                other => panic!("expected review, got {other:?}"),
            }
        }
    };
    let short =
        propose(json!({"hours": "1", "spent_on": "2026-11-02", "comments": "短說明"})).await;
    let long =
        propose(json!({"hours": "1", "spent_on": "2026-11-02", "comments": "長".repeat(81)})).await;
    let mine = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    let upd_cs = match submit(w, agent, &upd(mine, 0, json!({"hours": "2"})))
        .await
        .unwrap()
    {
        SubmitResult::ReviewRequired { proposal, .. } => proposal.changeset_id,
        other => panic!("expected review, got {other:?}"),
    };
    let l = changesets::list(&w.app, w.alice_ctx, None, None, None, None)
        .await
        .unwrap();
    let row = |id: Uuid| {
        l.items
            .iter()
            .find(|r| r.changeset_id == id)
            .unwrap()
            .clone()
    };
    let r = row(short);
    assert_eq!(r.review_modes, vec!["detail", "queue"]);
    assert_eq!(r.diff.len(), 1);
    assert_eq!(r.diff[0].field, "time");
    assert_eq!(r.diff[0].target.id, i);
    assert_eq!(r.diff[0].after["hours"], json!("1"));
    assert_eq!(r.diff[0].after["comments"], json!("短說明"));
    assert_eq!(row(long).review_modes, vec!["detail"]);
    assert_eq!(row(upd_cs).review_modes, vec!["detail"]);
}

// ================================================================ verifier repros (not part of the implementation)

async fn direct_time_policy(t: &T, d: Uuid) {
    agents::set_delegation_policies(
        &t.w.pool,
        &t.w.policy,
        &t.w.alice_ctx,
        d,
        PolicyInput {
            commands: BTreeMap::from([
                ("log_time".to_string(), PolicyMode::Direct),
                ("update_time_entry".to_string(), PolicyMode::Direct),
            ]),
            fields: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
}

/// V1: an agent without read_own_time learns its delegator's whole-day total
/// (across projects it is not delegated to) from the T6 error's `remaining`.
#[tokio::test]
async fn vfy_agent_remaining_leaks_delegator_total() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    exec(
        &pool,
        "UPDATE org_settings SET timelog_max_hours_per_day = 8",
    )
    .await;
    // alice logs 5 hours in AUTH (not delegated), with a secret-ish split.
    exec(
        &pool,
        &format!(
            "INSERT INTO member_roles SELECT id, '{}' FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            t.lead, w.auth_p, w.alice
        ),
    )
    .await;
    cmd(
        w,
        w.alice_ctx,
        &doc(
            vec![log(
                None,
                Some(w.auth_p),
                json!({"hours": "5", "spent_on": "2026-11-02"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap();
    let d = delegate_time(&t, false, None).await;
    direct_time_policy(&t, d.id).await;
    let (_, agent) = w.token(d.context_id).await;
    let e = submit(
        w,
        agent,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "8", "spent_on": "2026-11-02"}),
            )],
            vec![],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "max_hours_per_day");
    assert!(
        details(&e).get("remaining").is_none(),
        "agent (read_own_time off) is told the delegator's remaining hours: {}",
        details(&e)
    );
}

/// V2: after a redaction, the comments can be edited again, and a second
/// redaction of the new text is a silent no-op (changed = false).
#[tokio::test]
async fn vfy_redact_after_edit() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    grant(&pool, t.lead, &["manage_project"]).await;
    let e = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02", "comments": "first"}),
    )
    .await;
    let target = Target::TimeEntry { entry: e };
    let r = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        Uuid::now_v7(),
        &target,
        &why(),
    )
    .await
    .unwrap();
    assert!(r.changed);
    cmd(
        w,
        w.alice_ctx,
        &upd(e, 0, json!({"comments": "SECOND-SECRET"})),
    )
    .await
    .unwrap();
    let r = redaction::redact(
        &w.app,
        w.alice_ctx,
        &w.policy,
        Uuid::now_v7(),
        &target,
        &why(),
    )
    .await
    .unwrap();
    let left = text_count(
        &pool,
        "SELECT count(*) FROM time_entries WHERE comments LIKE $1",
        "SECOND-SECRET",
    )
    .await;
    assert!(
        r.changed && left == 0,
        "second redaction changed={} and the new text is still stored ({left} rows)",
        r.changed
    );
}

/// V3: undo with skip_time_entries marks the skipped field reverted too, so
/// it can never be undone later.
#[tokio::test]
async fn vfy_undo_skip_keeps_skipped_field() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "skip").await;
    let d = delegate_time(&t, false, None).await;
    direct_time_policy(&t, d.id).await;
    let (_, agent) = w.token(d.context_id).await;
    let mine = log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-04", "comments": "a"}),
    )
    .await;
    let x = match submit(
        w,
        agent,
        &upd(mine, 0, json!({"hours": "2", "comments": "b"})),
    )
    .await
    .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("{other:?}"),
    };
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x,
        &UndoInput {
            skip_time_entries: vec![format!("{mine}/hours")],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(u.time_entries_restored, 1);
    let pv = undo::preview(&w.app, w.alice_ctx, x).await.unwrap();
    assert!(
        pv.time_entries.iter().any(|r| r.field == "hours"),
        "the skipped hours change disappeared from the undo preview: {pv:?}"
    );
}

/// V4: an agent running a view whose stored query filters on spent_hours.
#[tokio::test]
async fn vfy_agent_check_spent_in_search_or() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let d = delegate_time(&t, true, None).await;
    let (_, agent) = w.token(d.context_id).await;
    // nested inside or / not
    let e = oxledger::service::search(
        &w.app,
        agent,
        oxledger::service::SearchParams {
            where_: Some(
                json!({"or": [{"field": "spent_hours", "op": "ne", "value": "1"},
                                       {"field": "project", "op": "eq", "value": "WEB"}]}),
            ),
            limit: Some(10),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "unavailable_to_agent", "{e:#}");
}

/// V5: in review mode (the default), an agent's log_time for someone else
/// or a change of someone else's entry must still be refused (agent_time_scope),
/// at proposal time or at apply time.
#[tokio::test]
async fn vfy_agent_scope_survives_review() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let d = delegate_time(&t, false, None).await; // log_time / update_time_entry: review
    let (_, agent) = w.token(d.context_id).await;
    let approve = |cs: Uuid| async move {
        let view = changesets::get(&w.app, w.alice_ctx, cs, None)
            .await
            .unwrap();
        changesets::approve_and_apply(
            &w.app,
            w.alice_ctx,
            Uuid::now_v7(),
            cs,
            &ApproveInput {
                version: 1,
                proposal_hash: view.proposal_hash.clone(),
                review_mode: None,
            },
        )
        .await
    };
    let mut leaks = Vec::new();
    // (a) log for carol.
    match submit(
        w,
        agent,
        &doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "1", "spent_on": "2026-11-02", "user_id": w.carol}),
            )],
            vec![],
        ),
    )
    .await
    {
        Err(e) => assert_eq!(reason(&e), "agent_time_scope", "{e:#}"),
        Ok(SubmitResult::ReviewRequired { proposal, .. }) => {
            let o = approve(proposal.changeset_id).await;
            let n = count(
                &pool,
                &format!(
                    "SELECT count(*) FROM time_entries WHERE user_id = '{}' AND deleted_at IS NULL",
                    w.carol
                ),
            )
            .await;
            leaks.push(format!(
                "log for carol: proposal created; apply = {:?}; carol entries = {n}",
                o.as_ref()
                    .map(|o| o.execution.as_ref().map(|x| x.error.clone()))
            ));
        }
        Ok(other) => panic!("{other:?}"),
    }
    // (b) change carol's entry.
    let carol_e = log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-03"}),
    )
    .await;
    match submit(w, agent, &upd(carol_e, 0, json!({"hours": "7"}))).await {
        Err(e) => assert_eq!(reason(&e), "agent_time_scope", "{e:#}"),
        Ok(SubmitResult::ReviewRequired { proposal, .. }) => {
            let o = approve(proposal.changeset_id).await;
            let h: String =
                sqlx::query_scalar("SELECT hours::text FROM time_entries WHERE id = $1")
                    .bind(carol_e)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            leaks.push(format!(
                "update carol's: proposal created; apply ok = {}; hours now {h}",
                o.is_ok()
            ));
        }
        Ok(other) => panic!("{other:?}"),
    }
    // (c) move alice's entry to carol.
    let mine = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-04"}),
    )
    .await;
    match submit(w, agent, &upd(mine, 0, json!({"user_id": w.carol}))).await {
        Err(e) => assert_eq!(reason(&e), "agent_time_scope", "{e:#}"),
        Ok(SubmitResult::ReviewRequired { proposal, .. }) => {
            let o = approve(proposal.changeset_id).await;
            let u: Uuid = sqlx::query_scalar("SELECT user_id FROM time_entries WHERE id = $1")
                .bind(mine)
                .fetch_one(&pool)
                .await
                .unwrap();
            leaks.push(format!(
                "move to carol: proposal created; apply ok = {}; user now carol = {}",
                o.is_ok(),
                u == w.carol
            ));
        }
        Ok(other) => panic!("{other:?}"),
    }
    assert!(leaks.is_empty(), "{leaks:#?}");
}

// ---------------------------------------------------------------- verification gaps (M1.4 review)

/// T-M1-43, deterministic: two documents move entries between the same two
/// days in opposite directions. The first stops (fault point) right after it
/// locked its first daily total; the second then waits for that row. With
/// one (user, day) order for every document nothing deadlocks; a document
/// that locked in its own order (old day first) would.
#[tokio::test]
async fn t43_lock_order_two_documents() {
    let pool = test_pool().await;
    faults::clear();
    let t = setup(&pool).await;
    let w = &t.w;
    let a = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    let b = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "2", "spent_on": "2026-11-03"}),
    )
    .await;
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    let before = deadlocks(&pool).await;
    let gate = Arc::new(Gate::default());
    faults::arm("time.after_first_total", Action::Block(gate.clone()));
    let who = w.alice_ctx;
    let first = {
        let app = w.app.clone();
        let d = upd(a, 0, json!({"spent_on": "2026-11-03"}));
        tokio::spawn(async move { commands::submit(&app, who, Uuid::now_v7(), &d).await })
    };
    gate.reached.notified().await;
    let second = {
        let app = w.app.clone();
        let d = upd(b, 0, json!({"spent_on": "2026-11-02"}));
        tokio::spawn(async move { commands::submit(&app, who, Uuid::now_v7(), &d).await })
    };
    // The second document is waiting for a row the first holds.
    common::poll_until(
        "the second document to wait for a lock",
        common::WAIT_TIMEOUT,
        async || {
            let n = count(
                &pool,
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
    common::join("the first document", first).await.unwrap();
    common::join("the second document", second).await.unwrap();
    faults::clear();
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    exec(&pool, "SELECT pg_stat_clear_snapshot()").await;
    assert_eq!(
        deadlocks(&pool).await,
        before,
        "a deadlock (40P01) happened"
    );
    assert_eq!(total(w, w.alice, "2026-11-02").await, 200);
    assert_eq!(total(w, w.alice, "2026-11-03").await, 100);
    totals_consistent(w).await;
}

/// E4w: every direct log_time counts (`time_write`), and the 21st in an
/// hour is `quota_exceeded`.
#[tokio::test]
async fn agent_log_time_quota() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let d = delegate_time(&t, false, None).await;
    direct_time_policy(&t, d.id).await;
    let (_, agent) = w.token(d.context_id).await;
    let one = || {
        doc(
            vec![log(
                None,
                Some(w.web),
                json!({"hours": "0.1", "spent_on": "2026-11-02"}),
            )],
            vec![],
        )
    };
    let writes = || async {
        count(
            &pool,
            "SELECT count(*) FROM agent_write_log WHERE kind = 'time_write'",
        )
        .await
    };
    exec(&pool, "DELETE FROM agent_write_log").await;
    // Not `submit`: it clears agent_write_log first.
    let go = || async {
        changesets::command_submit(&w.app, agent, Uuid::now_v7(), &one(), false, ORIGIN).await
    };
    go().await.unwrap();
    assert_eq!(writes().await, 1, "a direct log_time is counted");
    // 18 more from earlier this hour: 19 in all, so one more fits.
    exec(
        &pool,
        &format!(
            "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind)
             SELECT agent_principal_id, context_id, delegation_id, 'time_write'
             FROM agent_write_log, generate_series(1, 18)
             WHERE kind = 'time_write' AND delegation_id = '{}'",
            d.id
        ),
    )
    .await;
    go().await.unwrap();
    assert_eq!(writes().await, 20);
    let e = go().await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded, "{e:#}");
    assert_eq!(writes().await, 20);
}

/// T-M1-41: the `permission` reason, the created entry that needs only
/// log_time (M1.4-D15, pending the user's call), and a partial undo with
/// `skip_time_entries`.
#[tokio::test]
async fn t41_undo_permission_and_skip() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let i = w.issue(w.web, "undo perms").await;
    let d = delegate_time(&t, false, None).await;
    direct_time_policy(&t, d.id).await;
    let (_, agent) = w.token(d.context_id).await;
    let (x_created, created) =
        agent_log(w, agent, i, json!({"hours": "1", "spent_on": "2026-11-02"})).await;
    let mine = log_as(
        &t,
        w.alice_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-03", "comments": "a"}),
    )
    .await;
    let x_changed = match submit(
        w,
        agent,
        &upd(mine, 0, json!({"hours": "2", "comments": "b"})),
    )
    .await
    .unwrap()
    {
        SubmitResult::Applied { execution_id, .. } => execution_id,
        other => panic!("{other:?}"),
    };
    // alice keeps log_time but loses edit_time_entries.
    exec(
        &pool,
        &format!(
            "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'edit_time_entries'",
            t.lead
        ),
    )
    .await;
    let pv = undo::preview(&w.app, w.alice_ctx, x_changed).await.unwrap();
    assert_eq!(pv.time_entries.len(), 2, "{pv:?}");
    for r in &pv.time_entries {
        assert!(!r.restorable, "{r:?}");
        assert_eq!(r.reason.as_deref(), Some("permission"), "{r:?}");
    }
    let pv = undo::preview(&w.app, w.alice_ctx, x_created).await.unwrap();
    assert_eq!(pv.time_entries.len(), 1, "{pv:?}");
    assert_eq!(pv.time_entries[0].field, "created");
    assert!(
        pv.time_entries[0].restorable,
        "the agent's new entry for alice can be taken back with log_time: {pv:?}"
    );
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x_created,
        &UndoInput::default(),
    )
    .await
    .unwrap();
    assert_eq!(u.time_entries_restored, 1);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM time_entries WHERE id = '{created}' AND deleted_at IS NOT NULL")
        )
        .await,
        1
    );
    // Back with edit_time_entries: undo hours only, comments stay.
    grant(&pool, t.lead, &["edit_time_entries"]).await;
    let u = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        x_changed,
        &UndoInput {
            skip_time_entries: vec![format!("{mine}/comments")],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(u.time_entries_restored, 1);
    let (h, c): (String, String) =
        sqlx::query_as("SELECT hours::text, comments FROM time_entries WHERE id = $1")
            .bind(mine)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((h.as_str(), c.as_str()), ("1.00", "b"));
    let pv = undo::preview(&w.app, w.alice_ctx, x_changed).await.unwrap();
    let left: Vec<(&str, bool)> = pv
        .time_entries
        .iter()
        .map(|r| (r.field.as_str(), r.restorable))
        .collect();
    assert_eq!(left, vec![("comments", true)], "{pv:?}");
    totals_consistent(w).await;
}

/// Update and delete of an entry the caller cannot read are `not_found`
/// before anything else; a change on a closed issue is `closed_issue`.
#[tokio::test]
async fn update_delete_invisible_and_closed() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    exec(
        &pool,
        &format!(
            "UPDATE roles SET issues_visibility = 'default' WHERE id IN ('{}', '{}', '{}')",
            t.own, w.dev, t.lead
        ),
    )
    .await;
    let p = private_issue(w, w.web, w.carol).await;
    let hidden = log_as(
        &t,
        t.carol_ctx,
        Some(p),
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    // alice sees everyone's time, but not carol's private issue.
    for d in [upd(hidden, 0, json!({"hours": "2"})), del(hidden, 0)] {
        let e = cmd(w, w.alice_ctx, &d).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    }
    // bob reads only his own time: carol's plain entry is not_found too
    // (not `permission`), also with the module off (not `module_disabled`).
    let plain = log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    for d in [upd(plain, 0, json!({"hours": "2"})), del(plain, 0)] {
        let e = cmd(w, t.bob_ctx, &d).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    }
    exec(
        &pool,
        "DELETE FROM enabled_modules WHERE module = 'time_tracking'",
    )
    .await;
    let e = cmd(w, t.bob_ctx, &upd(plain, 0, json!({"hours": "2"})))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    // ... while carol, who can read it, is told the module is off.
    let e = cmd(w, t.carol_ctx, &upd(plain, 0, json!({"hours": "2"})))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), "module_disabled", "{e:#}");
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    // closed_issue on update.
    let i = w.issue(w.web, "closing").await;
    let e1 = log_as(
        &t,
        t.bob_ctx,
        Some(i),
        json!({"hours": "1", "spent_on": "2026-11-02"}),
    )
    .await;
    exec(
        &pool,
        "UPDATE org_settings SET timelog_accept_closed_issues = false",
    )
    .await;
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(i)
        .bind(w.done)
        .execute(&pool)
        .await
        .unwrap();
    let e = cmd(w, t.bob_ctx, &upd(e1, 0, json!({"hours": "2"})))
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "closed_issue", "{e:#}");
    totals_consistent(w).await;
}

/// An agent running a project view whose stored `where` uses spent_hours.
#[tokio::test]
async fn agent_view_run_with_spent_hours() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    grant(&pool, t.lead, &["manage_public_views"]).await;
    w.issue(w.web, "viewed").await;
    let v = views::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &NewView {
            scope: "project".into(),
            project_id: Some("WEB".into()),
            kind: "list".into(),
            name: "Spent".into(),
            def: json!({"kind": "list", "query": {"where":
                {"field": "spent_hours", "op": "lt", "value": "5"}}}),
        },
    )
    .await
    .unwrap();
    let d = delegate_time(&t, true, None).await;
    let (_, agent) = w.token(d.context_id).await;
    let e = views::agent_run(
        &w.app,
        agent,
        &ViewRunParams {
            view: v.id.clone(),
            limit: None,
            cursor: None,
            lane: None,
            fields: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "unavailable_to_agent", "{e:#}");
}

/// MCP `time_list` over the protocol.
#[tokio::test]
async fn t39c_mcp_time_list() {
    let pool = test_pool().await;
    let t = setup(&pool).await;
    let w = &t.w;
    let mine = log_as(
        &t,
        w.alice_ctx,
        None,
        json!({"hours": "1.25", "spent_on": "2026-11-02"}),
    )
    .await;
    log_as(
        &t,
        t.carol_ctx,
        None,
        json!({"hours": "3", "spent_on": "2026-11-02"}),
    )
    .await;
    let d = delegate_time(&t, true, None).await;
    let (token, _) = w.token(d.context_id).await;
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN_SPA.into(),
            session: SessionPolicy::default(),
        },
    );
    let mut m = world::mcp_client::Mcp::connect(router, token).await;
    let listed = m.rpc("tools/list", json!({})).await;
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "time_list"),
        "{listed}"
    );
    let v = m
        .call(
            "time_list",
            json!({"from": "2026-11-01", "to": "2026-11-30"}),
        )
        .await
        .unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["id"], json!(mine));
    assert_eq!(items[0]["hours"], "1.25");
    // A missing `to`: refused (a protocol error or an error result).
    let r = m
        .rpc(
            "tools/call",
            json!({"name": "time_list", "arguments": {"from": "2026-11-01"}}),
        )
        .await;
    assert!(
        !r["error"].is_null() || r["result"]["isError"] == json!(true),
        "{r}"
    );
    agents::set_read_own_time(&w.pool, &w.policy, &w.alice_ctx, d.id, false, false)
        .await
        .unwrap();
    let e = m
        .call(
            "time_list",
            json!({"from": "2026-11-01", "to": "2026-11-30"}),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("time_read_not_delegated"), "{e}");
}
