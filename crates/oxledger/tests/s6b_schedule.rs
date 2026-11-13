// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.5: the schedule view, the health thresholds and the undo
//! of milestone writes (§17.8, §17.12, §17.13): T-V9, T-V9b, T-V10 (the
//! integration layer), T-V11, T-V12, T-V28, T-V29, T-V33, T-V34, T-V35.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use chrono::NaiveDate;
use common::test_pool;
use oxledger::agents::{self, DelegationInput};
use oxledger::auth::{self, AuthContext};
use oxledger::changesets::{self, SubmitResult, faults};
use oxledger::commands;
use oxledger::error::ErrorCode;
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::undo::{self, UndoInput};
use oxledger::versions::{
    self, NewVersion, OrgScheduleSettingsInput, ScheduleSettingsInput, Timeline, VersionPatch,
};
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

async fn today(w: &World) -> NaiveDate {
    sqlx::query_scalar("SELECT (now() AT TIME ZONE timezone)::date FROM org_settings")
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn days(d: NaiveDate, n: i64) -> NaiveDate {
    d + chrono::Duration::days(n)
}

async fn mk(w: &World, subject: &'static str, private: bool) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: w.web,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description: "",
            author_id: w.bob,
            assigned_to_id: None,
            is_private: private,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

/// Dates and version straight in the table (the timeline only reads).
async fn place(
    w: &World,
    i: Uuid,
    start: Option<NaiveDate>,
    due: Option<NaiveDate>,
    v: Option<Uuid>,
) {
    sqlx::query(
        "UPDATE issues SET start_date = $2, due_date = $3, fixed_version_id = $4 WHERE id = $1",
    )
    .bind(i)
    .bind(start)
    .bind(due)
    .bind(v)
    .execute(&w.pool)
    .await
    .unwrap();
}

async fn close(w: &World, i: Uuid) {
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(i)
        .bind(w.done)
        .execute(&w.pool)
        .await
        .unwrap();
}

async fn vnew(w: &World, who: AuthContext, name: &str, date: Option<NaiveDate>) -> Uuid {
    versions::create(
        &w.app,
        who,
        Uuid::now_v7(),
        "WEB",
        &NewVersion {
            name: name.into(),
            effective_date: date.map(|d| d.to_string()),
            description: None,
            status: None,
        },
    )
    .await
    .unwrap()
    .version
    .id
}

async fn vlv(w: &World, v: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM versions WHERE id = $1")
        .bind(v)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

async fn vset(w: &World, who: AuthContext, v: Uuid, set: Value) {
    let patch: VersionPatch =
        serde_json::from_value(json!({"lock_version": vlv(w, v).await, "set": set})).unwrap();
    versions::update(&w.app, who, Uuid::now_v7(), v, &patch)
        .await
        .unwrap();
}

async fn tl(
    w: &World,
    who: AuthContext,
    from: Option<&str>,
    to: Option<&str>,
    vs: Option<&str>,
    closed: bool,
) -> anyhow::Result<Timeline> {
    versions::timeline(&w.app, who, "WEB", from, to, vs, closed, None).await
}

async fn ilv(w: &World, i: Uuid) -> i32 {
    sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

fn fields_doc(i: Uuid, lv: i32, set: Value) -> Value {
    json!({"command_schema_version": 1, "title": "write",
           "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i), "set": set}],
           "preconditions": [{"target": tgt(i), "lock_version": lv}]})
}

fn vdoc(op_json: Value, pre: Option<(Uuid, i32)>) -> Value {
    let mut d = json!({"command_schema_version": 1, "title": "milestone", "ops": [op_json]});
    if let Some((v, lv)) = pre {
        d["preconditions"] = json!([{"target": {"type": "version", "id": v}, "lock_version": lv}]);
    }
    d
}

async fn direct(w: &World, agent: AuthContext, d: &Value) -> (Uuid, SubmitResult) {
    let r = changesets::command_submit(&w.app, agent, Uuid::now_v7(), d, false, ORIGIN)
        .await
        .unwrap();
    match &r {
        SubmitResult::Applied { execution_id, .. } => (*execution_id, r.clone()),
        other => panic!("not applied: {other:?}"),
    }
}

async fn agent_set_version(w: &World, agent: AuthContext, v: Uuid, set: Value) -> Uuid {
    let d = vdoc(
        json!({"op_id": op(), "kind": "update_version", "target": {"type": "version", "id": v}, "set": set}),
        Some((v, vlv(w, v).await)),
    );
    direct(w, agent, &d).await.0
}

async fn agent_create_version(w: &World, agent: AuthContext, name: &str) -> (Uuid, Uuid) {
    let d = vdoc(
        json!({"op_id": op(), "kind": "create_version", "project_id": w.web, "fields": {"name": name}}),
        None,
    );
    let (e, r) = direct(w, agent, &d).await;
    let SubmitResult::Applied { versions, .. } = r else {
        unreachable!()
    };
    (e, versions[0].version_id)
}

async fn human(w: &World, who: AuthContext, d: &Value) {
    commands::submit(&w.app, who, Uuid::now_v7(), d)
        .await
        .unwrap();
}

async fn undo_err(w: &World, e: Uuid) -> Value {
    let err = undo::apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        e,
        &UndoInput::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&err), ErrorCode::UndoNotApplicable, "{err:?}");
    oxledger::error::classify(&err)
        .and_then(|a| a.details)
        .unwrap_or(Value::Null)
}

/// alice delegates WEB with dates, version and milestone commands.
async fn delegate(w: &World) {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: ["update_fields", "create_version", "update_version"]
                .map(String::from)
                .to_vec(),
            writable_fields: ["subject", "fixed_version_id", "start_date", "due_date"]
                .map(String::from)
                .to_vec(),
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
            read_own_time: false,
            read_own_time_ack: false,
        },
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------- timeline

#[tokio::test]
async fn timeline_view() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let dave: Uuid = id(&pool, "SELECT id FROM principals WHERE login = 'dave'").await;
    member(&pool, w.web, dave, viewer).await;
    let dave_ctx = sign_in(&pool, &w.policy, "dave").await;
    let a = w.alice_ctx;
    let t = today(&w).await;

    // ------------------------------------------------ T-V9b: default ranges
    let r = tl(&w, a, None, None, None, false).await.unwrap();
    assert_eq!(r.today, t.to_string());
    assert_eq!(r.range.from, days(t, -14).to_string());
    assert_eq!(r.range.to, days(t, 90).to_string(), "no version");
    assert_eq!(r.lanes.len(), 1, "only the lane without a version");
    assert!(r.lanes[0].version.is_none());
    let past = vnew(&w, a, "past", Some(days(t, -30))).await;
    let r = tl(&w, a, None, None, None, false).await.unwrap();
    assert_eq!(
        r.range.to,
        days(t, 90).to_string(),
        "open versions only in the past"
    );
    let near = vnew(&w, a, "near", Some(days(t, 20))).await;
    let r = tl(&w, a, None, None, None, false).await.unwrap();
    assert_eq!(r.range.to, days(t, 34).to_string());
    let far = vnew(&w, a, "far", NaiveDate::from_ymd_opt(2199, 1, 1)).await;
    let r = tl(&w, a, None, None, None, false).await.unwrap();
    let (f, to) = (
        NaiveDate::parse_from_str(&r.range.from, "%Y-%m-%d").unwrap(),
        NaiveDate::parse_from_str(&r.range.to, "%Y-%m-%d").unwrap(),
    );
    assert_eq!((to - f).num_days(), 399);
    assert_eq!(r.range.truncated_by_version, Some(far));
    // One end only; the 400-day limit; from > to.
    let r = tl(&w, a, Some("2030-01-01"), None, None, false)
        .await
        .unwrap();
    assert_eq!(r.range.from, "2030-01-01");
    assert!(r.range.to.as_str() >= "2030-01-01");
    let r = tl(&w, a, None, Some("2020-01-01"), None, false)
        .await
        .unwrap();
    assert!(r.range.from.as_str() <= "2020-01-01" && r.range.to == "2020-01-01");
    for (from, to) in [("2027-01-02", "2027-01-01"), ("2026-01-01", "2027-02-05")] {
        let e = tl(&w, a, Some(from), Some(to), None, false)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidArgument, "{from}..{to}");
    }
    tl(&w, a, Some("2026-01-01"), Some("2027-02-04"), None, false)
        .await
        .unwrap();
    let e = tl(&w, a, Some("2026-13-01"), None, None, false)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    // ------------------------------------------------ lanes, intersections, flags
    let (i1, k1) = mk(&w, "one", false).await;
    let (i2, k2) = mk(&w, "two", false).await;
    let (i3, _) = mk(&w, "undated", false).await;
    let (i4, k4) = mk(&w, "due only, overdue", false).await;
    let (i5, _) = mk(&w, "outside", false).await;
    let (i6, k6) = mk(&w, "late for version", false).await;
    place(&w, i1, Some(days(t, 1)), Some(days(t, 3)), Some(near)).await;
    place(&w, i2, Some(days(t, 5)), None, Some(near)).await;
    place(&w, i3, None, None, Some(near)).await;
    place(&w, i4, None, Some(days(t, -2)), None).await;
    place(&w, i5, Some(days(t, 200)), Some(days(t, 210)), None).await;
    place(&w, i6, Some(days(t, 2)), Some(days(t, 25)), Some(near)).await;
    let from = days(t, -14).to_string();
    let to = days(t, 60).to_string();
    let r = tl(&w, a, Some(&from), Some(&to), None, false)
        .await
        .unwrap();
    let names: Vec<_> = r
        .lanes
        .iter()
        .map(|l| l.version.as_ref().map(|v| v.name.clone()))
        .collect();
    assert_eq!(
        names,
        [
            Some("past".into()),
            Some("near".into()),
            Some("far".into()),
            None
        ],
        "versions by date, then the lane without one"
    );
    let near_lane = &r.lanes[1];
    let keys: Vec<_> = near_lane.items.iter().map(|i| i.key.clone()).collect();
    assert_eq!(
        keys,
        [k1.clone(), k6.clone(), k2.clone()],
        "by coalesce(start, due), key"
    );
    assert_eq!(near_lane.undated_count, 1);
    assert!(
        near_lane.items[1]
            .flags
            .contains(&"late_for_version".to_string())
    );
    let none_lane = r.lanes.last().unwrap();
    assert_eq!(
        none_lane
            .items
            .iter()
            .map(|i| i.key.clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&k4),
        "a one-day item inside, the far one outside"
    );
    assert_eq!(none_lane.items[0].flags, ["overdue"]);
    assert_eq!(none_lane.items[0].status.name, "New");
    // versions=: ids or none.
    let r2 = tl(&w, a, Some(&from), Some(&to), Some("none"), false)
        .await
        .unwrap();
    assert_eq!(r2.lanes.len(), 1);
    assert!(r2.lanes[0].version.is_none());
    let only = near.to_string();
    let r2 = tl(&w, a, Some(&from), Some(&to), Some(&only), false)
        .await
        .unwrap();
    assert_eq!(r2.lanes.len(), 1);
    assert_eq!(r2.lanes[0].version.as_ref().unwrap().id, near);
    let e = tl(
        &w,
        a,
        Some(&from),
        Some(&to),
        Some(&Uuid::now_v7().to_string()),
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    // include_closed: closed issues and closed versions.
    close(&w, i1).await;
    vset(&w, a, past, json!({"status": "closed"})).await;
    let r = tl(&w, a, Some(&from), Some(&to), None, false)
        .await
        .unwrap();
    assert!(
        r.lanes
            .iter()
            .all(|l| l.version.as_ref().is_none_or(|v| v.id != past))
    );
    assert!(!r.lanes[0].items.iter().any(|i| i.key == k1));
    let r = tl(&w, a, Some(&from), Some(&to), None, true).await.unwrap();
    let pl = r
        .lanes
        .iter()
        .find(|l| l.version.as_ref().is_some_and(|v| v.id == past));
    assert_eq!(pl.unwrap().version.as_ref().unwrap().health, "closed");
    let nl = r
        .lanes
        .iter()
        .find(|l| l.version.as_ref().is_some_and(|v| v.id == near))
        .unwrap();
    let it = nl.items.iter().find(|i| i.key == k1).unwrap();
    assert!(it.status.is_closed);

    // ------------------------------------------------ T-V9: a private issue leaves no trace
    let before = tl(&w, dave_ctx, Some(&from), Some(&to), None, true)
        .await
        .unwrap();
    let before_list = versions::list(&w.app, dave_ctx, "WEB", None, Some("stats"))
        .await
        .unwrap();
    // A viewer without manage_versions: the SPA hides create/change.
    assert!(!before_list.can_manage);
    assert!(
        versions::list(&w.app, a, "WEB", None, None)
            .await
            .unwrap()
            .can_manage
    );
    assert!(
        !versions::schedule_settings(&w.app, dave_ctx, "WEB")
            .await
            .unwrap()
            .can_manage
    );
    let (p1, _) = mk(&w, "private between", true).await;
    place(&w, p1, Some(days(t, 2)), Some(days(t, 4)), Some(near)).await;
    let (p2, _) = mk(&w, "private undated", true).await;
    place(&w, p2, None, None, Some(near)).await;
    let (p3, _) = mk(&w, "private overdue", true).await;
    place(&w, p3, None, Some(days(t, -1)), Some(near)).await;
    let after = tl(&w, dave_ctx, Some(&from), Some(&to), None, true)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "T-V9: lanes, undated_count and health unchanged"
    );
    let after_list = versions::list(&w.app, dave_ctx, "WEB", None, Some("stats"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&before_list).unwrap(),
        serde_json::to_value(&after_list).unwrap(),
        "T-V9: stats unchanged"
    );
    // alice sees them (control).
    let r = tl(&w, a, Some(&from), Some(&to), None, true).await.unwrap();
    let nl = r
        .lanes
        .iter()
        .find(|l| l.version.as_ref().is_some_and(|v| v.id == near))
        .unwrap();
    assert_eq!(nl.undated_count, 2);
    assert!(
        nl.version
            .as_ref()
            .unwrap()
            .health_reasons
            .contains(&"issue_overdue".to_string())
    );
    let dl = after
        .lanes
        .iter()
        .find(|l| l.version.as_ref().is_some_and(|v| v.id == near))
        .unwrap();
    assert!(
        !dl.version
            .as_ref()
            .unwrap()
            .health_reasons
            .contains(&"issue_overdue".to_string())
    );
    // A deleted version shows only while the caller sees an issue in it.
    let gone = vnew(&w, a, "gone", Some(days(t, 10))).await;
    let (g1, _) = mk(&w, "private in gone", true).await;
    place(&w, g1, Some(days(t, 9)), None, Some(gone)).await;
    sqlx::query("UPDATE versions SET deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();
    let r = tl(&w, a, Some(&from), Some(&to), None, true).await.unwrap();
    let gl = r
        .lanes
        .iter()
        .find(|l| l.version.as_ref().is_some_and(|v| v.id == gone));
    assert!(gl.unwrap().version.as_ref().unwrap().deleted);
    let r = tl(&w, dave_ctx, Some(&from), Some(&to), None, true)
        .await
        .unwrap();
    assert!(
        r.lanes
            .iter()
            .all(|l| l.version.as_ref().is_none_or(|v| v.id != gone))
    );

    // ------------------------------------------------ T-V10 (integration): health per lane
    let nodate = vnew(&w, a, "nodate", None).await;
    let overdue = vnew(&w, a, "overdue", Some(days(t, -3))).await;
    let calm = vnew(&w, a, "calm", Some(days(t, 50))).await;
    let (o1, _) = mk(&w, "still open", false).await;
    place(&w, o1, None, None, Some(overdue)).await;
    let (c1, _) = mk(&w, "calm work", false).await;
    place(&w, c1, Some(days(t, 1)), Some(days(t, 30)), Some(calm)).await;
    let r = tl(&w, a, Some(&from), Some(&to), None, true).await.unwrap();
    let h = |v: Uuid| {
        let l = r
            .lanes
            .iter()
            .find(|l| l.version.as_ref().is_some_and(|x| x.id == v))
            .unwrap();
        l.version.as_ref().unwrap().health.clone()
    };
    assert_eq!(h(past), "closed");
    assert_eq!(h(nodate), "no_date");
    assert_eq!(h(overdue), "overdue");
    assert_eq!(h(near), "at_risk");
    assert_eq!(h(calm), "on_track");

    // ------------------------------------------------ lane paging (200)
    let keys_before = tl(&w, a, Some(&from), Some(&to), Some("none"), false)
        .await
        .unwrap();
    let base = keys_before.lanes[0].items.len();
    for _ in 0..(201 - base) {
        let (i, _) = mk(&w, "bulk", false).await;
        place(&w, i, Some(days(t, 7)), None, None).await;
    }
    let r = tl(&w, a, Some(&from), Some(&to), Some("none"), false)
        .await
        .unwrap();
    assert_eq!(r.lanes[0].items.len(), 200);
    let cur = r.lanes[0].next_cursor.clone().expect("a next page");
    let r2 = versions::timeline(
        &w.app,
        a,
        "WEB",
        Some(&from),
        Some(&to),
        Some("none"),
        false,
        Some(&cur),
    )
    .await
    .unwrap();
    assert_eq!(r2.lanes.len(), 1);
    assert_eq!(r2.lanes[0].items.len(), 1);
    assert!(r2.lanes[0].next_cursor.is_none());
    let e = versions::timeline(&w.app, a, "WEB", None, None, None, false, Some("junk"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidCursor);
    let _ = k2;
}

// ---------------------------------------------------------------- thresholds

#[tokio::test]
async fn schedule_settings() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let t = today(&w).await;
    // The organization row survives the test reset.
    exec(
        &pool,
        "UPDATE org_settings SET health_at_risk_days = 7, health_at_risk_open_pct = 50",
    )
    .await;

    let put = |lv: i32, inherit: bool, d: Option<Option<i32>>, p: Option<Option<i32>>| {
        let mut j = json!({"lock_version": lv, "inherit_org": inherit});
        if let Some(d) = d {
            j["health_at_risk_days"] = json!(d);
        }
        if let Some(p) = p {
            j["health_at_risk_open_pct"] = json!(p);
        }
        serde_json::from_value::<ScheduleSettingsInput>(j).unwrap()
    };

    let s = versions::schedule_settings(&w.app, a, "WEB").await.unwrap();
    assert_eq!(s.source, "org");
    assert!(s.can_manage);
    assert_eq!(s.effective.health_at_risk_days, Some(7));
    assert_eq!(s.effective.health_at_risk_open_pct, Some(50));
    assert_eq!(s.lock_version, 0);
    // inherit_org true with values; false without both.
    for bad in [
        put(0, true, Some(Some(3)), None),
        put(0, false, Some(Some(3)), None),
    ] {
        let e = versions::put_schedule_settings(&w.app, a, Uuid::now_v7(), "WEB", &bad)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest);
    }
    let e = versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(0, false, Some(Some(91)), Some(Some(5))),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);

    // ------------------------------------------------ T-V33
    // A version 5 days out: 4 of 5 open (80%).
    let v = vnew(&w, a, "v", Some(days(t, 5))).await;
    let mut is = Vec::new();
    for _ in 0..5 {
        let (i, _) = mk(&w, "work", false).await;
        place(&w, i, Some(days(t, 1)), Some(days(t, 4)), Some(v)).await;
        is.push(i);
    }
    close(&w, is[0]).await;
    let reasons = |w: &World| {
        let w = w.app.clone();
        async move {
            let r = versions::timeline(&w, a, "WEB", None, None, None, false, None)
                .await
                .unwrap();
            let l = r
                .lanes
                .into_iter()
                .find(|l| l.version.as_ref().is_some_and(|x| x.id == v))
                .unwrap();
            l.version.unwrap().health_reasons
        }
    };
    let near = "deadline_near_incomplete".to_string();
    assert!(
        reasons(&w).await.contains(&near),
        "org 7 days / 50%: 80% > 50"
    );
    let key = Uuid::now_v7();
    let s = versions::put_schedule_settings(
        &w.app,
        a,
        key,
        "WEB",
        &put(0, false, Some(Some(3)), Some(Some(80))),
    )
    .await
    .unwrap();
    assert_eq!((s.source.as_str(), s.lock_version), ("project", 1));
    assert!(
        !reasons(&w).await.contains(&near),
        "project 3 days: 5 days out"
    );
    // Replay of the same key: no second bump.
    let s = versions::put_schedule_settings(
        &w.app,
        a,
        key,
        "WEB",
        &put(0, false, Some(Some(3)), Some(Some(80))),
    )
    .await
    .unwrap();
    assert_eq!(s.lock_version, 1);
    let s = versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(1, false, Some(Some(7)), Some(Some(80))),
    )
    .await
    .unwrap();
    assert!(
        !reasons(&w).await.contains(&near),
        "exactly 80% is not over 80"
    );
    reopen_or_close(&w, is[0], false).await;
    assert!(reasons(&w).await.contains(&near), "100% > 80");
    versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(s.lock_version, false, Some(None), Some(Some(80))),
    )
    .await
    .unwrap();
    assert!(!reasons(&w).await.contains(&near), "turned off");
    // Total 0: no issue in the version.
    for i in &is {
        place(&w, *i, None, None, None).await;
    }
    let s = versions::schedule_settings(&w.app, a, "WEB").await.unwrap();
    versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(s.lock_version, false, Some(Some(7)), Some(Some(0))),
    )
    .await
    .unwrap();
    assert!(!reasons(&w).await.contains(&near), "total 0 never");
    // Back to the org values.
    let s = versions::schedule_settings(&w.app, a, "WEB").await.unwrap();
    let s = versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(s.lock_version, true, None, None),
    )
    .await
    .unwrap();
    assert_eq!(s.source, "org");
    assert_eq!(s.project.health_at_risk_days, None);

    // ------------------------------------------------ T-V34: the later one is a conflict
    // (a) a project changed before.
    let lv = s.lock_version;
    versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "WEB",
        &put(lv, false, Some(Some(1)), Some(Some(1))),
    )
    .await
    .unwrap();
    let e = versions::put_schedule_settings(
        &w.app,
        bob,
        Uuid::now_v7(),
        "WEB",
        &put(lv, false, Some(Some(2)), Some(Some(2))),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // (b) a new project, never changed: the row exists.
    let np = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('NEWP','New') RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects WHERE key = 'NEWP'",
    )
    .await;
    member(&pool, np, w.alice, w.dev).await;
    member(&pool, np, w.bob, w.dev).await;
    let s = versions::schedule_settings(&w.app, a, "NEWP")
        .await
        .unwrap();
    assert_eq!(s.lock_version, 0);
    versions::put_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        "NEWP",
        &put(0, false, Some(Some(1)), Some(Some(1))),
    )
    .await
    .unwrap();
    let e = versions::put_schedule_settings(
        &w.app,
        bob,
        Uuid::now_v7(),
        "NEWP",
        &put(0, false, Some(Some(2)), Some(Some(2))),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // (c) the organization; only an administrator.
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    auth::create_user(&pool, "root2", "root2", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let root2 = sign_in(&pool, &w.policy, "root2").await;
    let o = versions::org_schedule_settings(&w.app, a).await.unwrap();
    let oi = |lv: i32, d: Option<i32>| OrgScheduleSettingsInput {
        lock_version: lv,
        health_at_risk_days: d,
        health_at_risk_open_pct: Some(50),
    };
    let e = versions::put_org_schedule_settings(
        &w.app,
        a,
        Uuid::now_v7(),
        &oi(o.lock_version, Some(5)),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let o2 = versions::put_org_schedule_settings(
        &w.app,
        root,
        Uuid::now_v7(),
        &oi(o.lock_version, Some(5)),
    )
    .await
    .unwrap();
    assert_eq!(
        (o2.health_at_risk_days, o2.lock_version),
        (Some(5), o.lock_version + 1)
    );
    let e = versions::put_org_schedule_settings(
        &w.app,
        root2,
        Uuid::now_v7(),
        &oi(o.lock_version, Some(6)),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // Bob without manage_versions in a project: forbidden.
    let lead = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reader','all') RETURNING id",
    )
    .await;
    grant(&pool, lead, &["view_issues"]).await;
    let np2 = id(
        &pool,
        "INSERT INTO projects (key, name) VALUES ('RD','Read') RETURNING id",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'issue_tracking' FROM projects WHERE key = 'RD'",
    )
    .await;
    member(&pool, np2, w.bob, lead).await;
    let e = versions::put_schedule_settings(
        &w.app,
        bob,
        Uuid::now_v7(),
        "RD",
        &put(0, true, None, None),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // Leave the organization defaults as other suites expect them.
    exec(
        &pool,
        "UPDATE org_settings SET health_at_risk_days = 7, health_at_risk_open_pct = 50",
    )
    .await;
}

async fn reopen_or_close(w: &World, i: Uuid, done: bool) {
    sqlx::query("UPDATE issues SET status_id = $2 WHERE id = $1")
        .bind(i)
        .bind(if done { w.done } else { w.new })
        .execute(&w.pool)
        .await
        .unwrap();
}

// ---------------------------------------------------------------- milestone undo

#[tokio::test]
async fn milestone_undo() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let t = today(&w).await;
    let v19 = vnew(&w, a, "v1.9", Some(days(t, 10))).await;
    let v20 = vnew(&w, a, "v2.0", Some(days(t, 40))).await;
    let ctx = w.new_context().await;
    delegate(&w).await;
    let (_t, agent) = w.token(ctx).await;

    // ------------------------------------------------ T-V11: the old version is closed
    let (i1, _) = mk(&w, "moves", false).await;
    human(
        &w,
        a,
        &fields_doc(i1, ilv(&w, i1).await, json!({"fixed_version_id": v19})),
    )
    .await;
    let (e11, _) = direct(
        &w,
        agent,
        &fields_doc(i1, ilv(&w, i1).await, json!({"fixed_version_id": v20})),
    )
    .await;
    vset(&w, a, v19, json!({"status": "closed"})).await;
    let p = undo::preview(&w.app, a, e11).await.unwrap();
    let f = p
        .fields
        .iter()
        .find(|f| f.field == "fixed_version_id")
        .unwrap();
    assert!(!f.restorable);
    assert_eq!(f.reason.as_deref(), Some("value_no_longer_valid"));
    assert_eq!(f.subreason.as_deref(), Some("version_not_open"));
    let d = undo_err(&w, e11).await;
    assert!(d.to_string().contains("version_not_open"), "{d}");
    let fv: Option<Uuid> = sqlx::query_scalar("SELECT fixed_version_id FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fv, Some(v20));

    // ------------------------------------------------ T-V12: only start would come back
    let (i2, _) = mk(&w, "dates", false).await;
    human(
        &w,
        a,
        &fields_doc(
            i2,
            ilv(&w, i2).await,
            json!({"start_date": days(t, 8).to_string(), "due_date": days(t, 10).to_string()}),
        ),
    )
    .await;
    let (e12, _) = direct(
        &w,
        agent,
        &fields_doc(
            i2,
            ilv(&w, i2).await,
            json!({"start_date": days(t, 2).to_string(), "due_date": days(t, 4).to_string()}),
        ),
    )
    .await;
    human(
        &w,
        bob,
        &fields_doc(
            i2,
            ilv(&w, i2).await,
            json!({"due_date": days(t, 5).to_string()}),
        ),
    )
    .await;
    let lv_before = ilv(&w, i2).await;
    undo_err(&w, e12).await;
    assert_eq!(ilv(&w, i2).await, lv_before, "T-V12: nothing written");

    // ------------------------------------------------ T-V28: A → B → A is not restored
    let e28 = agent_set_version(
        &w,
        agent,
        v20,
        json!({"effective_date": days(t, 45).to_string()}),
    )
    .await;
    vset(
        &w,
        bob,
        v20,
        json!({"effective_date": days(t, 46).to_string()}),
    )
    .await;
    vset(
        &w,
        carol,
        v20,
        json!({"effective_date": days(t, 45).to_string()}),
    )
    .await;
    let p = undo::preview(&w.app, a, e28).await.unwrap();
    assert_eq!(p.version_fields.len(), 1);
    assert_eq!(p.version_fields[0].reason.as_deref(), Some("changed_since"));
    undo_err(&w, e28).await;

    // ------------------------------------------------ T-V29: undo create_version
    let (ca, va) = agent_create_version(&w, agent, "untouched").await;
    let (cb, vb) = agent_create_version(&w, agent, "renamed").await;
    let (cc, vc) = agent_create_version(&w, agent, "in use").await;
    let (cd, vd) = agent_create_version(&w, agent, "hidden use").await;
    vset(&w, bob, vb, json!({"name": "renamed by bob"})).await;
    let (iu, _) = mk(&w, "uses c", false).await;
    place(&w, iu, None, None, Some(vc)).await;
    // The only issue in vd is one alice cannot see.
    exec(&pool, "UPDATE roles SET issues_visibility = 'default'").await;
    let (ih, _) = mk(&w, "private in d", true).await;
    place(&w, ih, None, None, Some(vd)).await;
    let r = undo::apply(&w.app, a, Uuid::now_v7(), ca, &UndoInput::default())
        .await
        .unwrap();
    assert!(r.version_deleted);
    let p = undo::preview(&w.app, a, cb).await.unwrap();
    assert_eq!(
        p.version_created.unwrap().reason.as_deref(),
        Some("changed_since")
    );
    undo_err(&w, cb).await;
    let p = undo::preview(&w.app, a, cc).await.unwrap();
    assert_eq!(p.version_created.unwrap().reason.as_deref(), Some("in_use"));
    undo_err(&w, cc).await;
    let r = undo::apply(&w.app, a, Uuid::now_v7(), cd, &UndoInput::default())
        .await
        .unwrap();
    assert!(
        r.version_deleted,
        "only invisible uses: soft delete (same as R5)"
    );
    exec(&pool, "UPDATE roles SET issues_visibility = 'all'").await;
    let deleted: Vec<bool> = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM versions WHERE id = ANY($1) ORDER BY name_key",
    )
    .bind(vec![va, vb, vc, vd])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(deleted.iter().filter(|d| **d).count(), 2);

    // ------------------------------------------------ T-V35: suspended, revoked: undo still works
    // The role change above bumped the epoch: a new context.
    let ctx = w.new_context().await;
    delegate(&w).await;
    let (_t, agent) = w.token(ctx).await;
    let e35 = agent_set_version(
        &w,
        agent,
        v20,
        json!({"effective_date": days(t, 60).to_string()}),
    )
    .await;
    agents::suspend_context(&pool, &a, ctx).await.unwrap();
    let dels: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM delegations WHERE revoked_at IS NULL")
        .fetch_all(&pool)
        .await
        .unwrap();
    for d in dels {
        agents::revoke_delegation(&pool, &a, d).await.unwrap();
    }
    let r = undo::apply(&w.app, a, Uuid::now_v7(), e35, &UndoInput::default())
        .await
        .unwrap();
    assert_eq!(r.version_fields_restored, 1);
    let date: Option<NaiveDate> =
        sqlx::query_scalar("SELECT effective_date FROM versions WHERE id = $1")
            .bind(v20)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(date, Some(days(t, 45)));
}
