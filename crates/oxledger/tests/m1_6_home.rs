// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.6: preferences, the home page and interface roles (§2.8,
//! §3.8, §6.12). T-M1-60 (protected widget, templates replace / merge),
//! T-M1-61 (nobody else's preferences or home can be read), every
//! reference re-checked on read (deleted view, view no longer readable,
//! project left, module off, permission revoked: no name, no count, one
//! answer), concurrency (lock_version and Idempotency-Key), the CHECK
//! lists of 0026 against the code's constants, and the HTTP routes. Needs
//! PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::{AuthContext, SessionPolicy};
use oxledger::error::{ErrorCode, classify};
use oxledger::home::{self, ApplyTemplate, HomePut, HomeView, PreferencesPut, WidgetInput};
use oxledger::views::{self, NewView, RunInput};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

fn details(e: &anyhow::Error) -> Value {
    classify(e)
        .unwrap_or_else(|| panic!("not an API error: {e:#}"))
        .details
        .clone()
        .unwrap_or(Value::Null)
}

fn wi(kind: &str) -> WidgetInput {
    WidgetInput {
        id: None,
        widget_type: kind.into(),
        width: None,
        view_id: None,
        limit: None,
        project_id: None,
        protected: None,
        available: None,
        view_name: None,
        view_kind: None,
        project_key: None,
        project_name: None,
    }
}

fn wid(id: &str, kind: &str) -> WidgetInput {
    WidgetInput {
        id: Some(id.into()),
        ..wi(kind)
    }
}

fn wview(id: &str, view: &str) -> WidgetInput {
    WidgetInput {
        view_id: Some(view.into()),
        ..wid(id, "view")
    }
}

fn wms(id: &str, project: Uuid) -> WidgetInput {
    WidgetInput {
        project_id: Some(project.to_string()),
        ..wid(id, "milestones")
    }
}

/// The GET body sent back as a PUT body.
fn round_trip(h: &HomeView) -> Vec<WidgetInput> {
    serde_json::from_value(serde_json::to_value(&h.widgets).unwrap()).unwrap()
}

async fn put_home(
    w: &World,
    who: AuthContext,
    lock_version: i32,
    widgets: Vec<WidgetInput>,
) -> anyhow::Result<HomeView> {
    home::put_home(
        &w.app,
        who,
        Uuid::now_v7(),
        &HomePut {
            lock_version,
            widgets,
        },
    )
    .await
}

fn prefs_put(lock_version: i32) -> PreferencesPut {
    PreferencesPut {
        lock_version,
        interface_roles: None,
        onboarded: None,
        theme: None,
        density: None,
        nav_shortcuts: None,
        default_views: None,
    }
}

async fn apply(w: &World, who: AuthContext, roles: &[&str], mode: &str) -> home::Applied {
    home::apply_template(
        &w.app,
        who,
        Uuid::now_v7(),
        &ApplyTemplate {
            roles: roles.iter().map(|s| s.to_string()).collect(),
            mode: mode.into(),
        },
    )
    .await
    .unwrap()
}

fn kinds(h: &HomeView) -> Vec<(String, Option<String>)> {
    h.widgets
        .iter()
        .map(|w| (w.widget_type.clone(), w.view_id.clone()))
        .collect()
}

/// alice's agent (WEB delegated) and its token.
async fn agent(w: &World) -> (String, AuthContext) {
    let ctx = w.new_context().await;
    w.delegate(w.web).await.unwrap();
    w.token(ctx).await
}

async fn view(
    w: &World,
    who: AuthContext,
    scope: &str,
    project: Option<&str>,
    name: &str,
) -> String {
    views::create(
        &w.app,
        who,
        Uuid::now_v7(),
        &NewView {
            scope: scope.into(),
            project_id: project.map(String::from),
            kind: "list".into(),
            name: name.into(),
            def: json!({"kind": "list"}),
        },
    )
    .await
    .unwrap()
    .id
}

// ---------------------------------------------------------------- T-M1-60

#[tokio::test]
async fn t60_protected_widget_and_templates() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // First read: the general template, need_decision protected.
    let h = home::home(&w.app, a).await.unwrap();
    assert_eq!(h.lock_version, 0);
    assert_eq!(
        kinds(&h),
        vec![
            ("need_decision".into(), None),
            ("view".into(), Some("builtin:assigned_to_me".into())),
            ("view".into(), Some("builtin:involving_me".into())),
        ]
    );
    assert!(h.widgets[0].protected && h.widgets.iter().all(|x| x.available));
    assert!(h.widgets[1..].iter().all(|x| !x.protected));
    assert!(h.widgets[1].view_name.is_some());
    let p = home::preferences(&w.app, a).await.unwrap();
    assert!(!p.onboarded && p.interface_roles.is_empty());
    assert_eq!(
        (p.theme.as_str(), p.density.as_str()),
        ("dark", "comfortable")
    );
    assert_eq!(
        p.default_views.get("*").map(|d| d.view_id.as_str()),
        Some("builtin:list")
    );

    // Missing need_decision: protected_widget_missing, nothing written.
    let e = put_home(&w, a, 0, vec![wview("w_x", "builtin:assigned_to_me")])
        .await
        .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "protected_widget_missing");
    let e = put_home(&w, a, 0, vec![]).await.unwrap_err();
    assert_eq!(details(&e)["kind"], "protected_widget_missing");
    assert_eq!(home::home(&w.app, a).await.unwrap().lock_version, 0);
    // The other checks.
    let bad: Vec<(Vec<WidgetInput>, &str)> = vec![
        (
            vec![wi("need_decision"), wi("need_decision")],
            "duplicate_protected_widget",
        ),
        (
            (0..13).map(|_| wi("agent_summary")).collect(),
            "too_many_widgets",
        ),
        (
            vec![wi("need_decision"), wi("weather")],
            "unknown_widget_type",
        ),
        (
            vec![WidgetInput {
                width: Some(4),
                ..wi("need_decision")
            }],
            "width_out_of_range",
        ),
        (
            vec![
                wi("need_decision"),
                WidgetInput {
                    limit: Some(2),
                    ..wview("v", "builtin:assigned_to_me")
                },
            ],
            "limit_out_of_range",
        ),
        (vec![wi("need_decision"), wi("view")], "view_required"),
        (
            vec![WidgetInput {
                view_id: Some("builtin:assigned_to_me".into()),
                ..wi("need_decision")
            }],
            "unexpected_property",
        ),
        (
            vec![wid("a", "need_decision"), wid("a", "triage_count")],
            "duplicate_widget_id",
        ),
        (vec![wid("a b", "need_decision")], "invalid_widget_id"),
    ];
    for (widgets, kind) in bad {
        let e = put_home(&w, a, 0, widgets).await.unwrap_err();
        assert_eq!(details(&e)["kind"], kind);
    }
    // Move and resize need_decision; 12 is fine.
    let mut twelve: Vec<WidgetInput> = (0..11).map(|_| wi("agent_summary")).collect();
    twelve.push(WidgetInput {
        width: Some(3),
        ..wid("mine", "need_decision")
    });
    let h = put_home(&w, a, 0, twelve).await.unwrap();
    assert_eq!(h.lock_version, 1);
    assert_eq!(h.widgets.len(), 12);
    assert_eq!(h.widgets[11].id, "mine");
    assert_eq!(h.widgets[11].width, 3);

    // replace: the template, keeping alice's own need_decision.
    let r = apply(&w, a, &["pm"], "replace").await;
    assert_eq!(
        kinds(&r.home),
        vec![
            ("need_decision".into(), None),
            ("triage_count".into(), None),
            ("milestones".into(), None),
            ("view".into(), Some("builtin:involving_me".into())),
        ]
    );
    assert_eq!(
        (r.home.widgets[0].id.as_str(), r.home.widgets[0].width),
        ("mine", 3)
    );
    assert_eq!(r.home.lock_version, 2);
    let nav: Vec<_> = r
        .preferences
        .nav_shortcuts
        .iter()
        .map(|s| s.kind.as_str())
        .collect();
    assert_eq!(nav, vec!["triage", "timeline"]);
    assert_eq!(r.preferences.default_views["*"].view_id, "builtin:timeline");
    // merge: only what is missing; nothing removed.
    let r = apply(&w, a, &["engineer"], "merge").await;
    assert_eq!(
        kinds(&r.home),
        vec![
            ("need_decision".into(), None),
            ("triage_count".into(), None),
            ("milestones".into(), None),
            ("view".into(), Some("builtin:involving_me".into())),
            ("view".into(), Some("builtin:assigned_to_me".into())),
            ("my_time_week".into(), None),
            ("agent_summary".into(), None),
        ]
    );
    assert_eq!(r.home.widgets.iter().filter(|x| x.protected).count(), 1);
    let nav: Vec<_> = r
        .preferences
        .nav_shortcuts
        .iter()
        .map(|s| s.kind.as_str())
        .collect();
    assert_eq!(nav, vec!["triage", "timeline", "board", "view"]);
    assert_eq!(r.preferences.default_views["*"].view_id, "builtin:timeline");
    // merge again changes nothing (no lock bump).
    let again = apply(&w, a, &["engineer"], "merge").await;
    assert_eq!(again.home.lock_version, r.home.lock_version);
    assert_eq!(again.preferences.lock_version, r.preferences.lock_version);
    // A home without need_decision cannot exist even after replace with
    // a home that was 12 wide.
    let r = apply(&w, a, &["pm", "designer", "engineer"], "replace").await;
    assert!(r.home.widgets.len() <= home::TEMPLATE_MAX_WIDGETS);
    assert_eq!(r.home.widgets[0].widget_type, "need_decision");
    // merge into a full home stops at 12.
    let full: Vec<WidgetInput> = std::iter::once(wi("need_decision"))
        .chain((0..11).map(|_| wi("agent_summary")))
        .collect();
    let h = put_home(&w, a, r.home.lock_version, full).await.unwrap();
    let r = apply(&w, a, &["pm"], "merge").await;
    assert_eq!(r.home.widgets.len(), home::MAX_WIDGETS);
    assert_eq!(r.home.lock_version, h.lock_version);
    // Bad modes and roles.
    for (roles, mode, kind) in [
        (vec!["pm"], "swap", "invalid_mode"),
        (vec!["boss"], "replace", "unknown_role"),
    ] {
        let e = home::apply_template(
            &w.app,
            a,
            Uuid::now_v7(),
            &ApplyTemplate {
                roles: roles.into_iter().map(String::from).collect(),
                mode: mode.into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(details(&e)["kind"], kind);
    }
    // Templates as data: changes nothing.
    let before = home::home(&w.app, a).await.unwrap().lock_version;
    let t = home::templates(a, Some("designer,pm")).unwrap();
    assert_eq!(t.roles, vec!["designer", "pm"]);
    assert_eq!(t.widgets[0].widget_type, "need_decision");
    assert_eq!(t.default_views["*"], "builtin:list");
    assert!(t.widgets.len() <= home::TEMPLATE_MAX_WIDGETS);
    let e = home::templates(a, Some("pm,boss")).unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidArgument);
    assert_eq!(home::home(&w.app, a).await.unwrap().lock_version, before);
}

// ---------------------------------------------------------------- preferences

#[tokio::test]
async fn preferences_put_and_concurrency() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let p = home::preferences(&w.app, a).await.unwrap();
    // First-login flow: roles + onboarded, then apply-template replace.
    let mut put = prefs_put(p.lock_version);
    put.interface_roles = Some(vec!["engineer".into(), "pm".into(), "engineer".into()]);
    put.onboarded = Some(true);
    put.theme = Some("high_contrast".into());
    let p = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap();
    assert_eq!(p.interface_roles, vec!["engineer", "pm"]);
    assert!(p.onboarded);
    assert_eq!(p.theme, "high_contrast");
    assert_eq!(p.lock_version, 1);
    // Validation.
    for (f, kind) in [
        (
            Box::new(|x: &mut PreferencesPut| x.theme = Some("neon".into()))
                as Box<dyn Fn(&mut PreferencesPut)>,
            "invalid_theme",
        ),
        (
            Box::new(|x: &mut PreferencesPut| x.density = Some("tiny".into())),
            "invalid_density",
        ),
        (
            Box::new(|x: &mut PreferencesPut| x.interface_roles = Some(vec!["ceo".into()])),
            "unknown_role",
        ),
        (
            Box::new(|x: &mut PreferencesPut| {
                x.nav_shortcuts = Some(vec![
                    serde_json::from_value(json!({"kind": "view"})).unwrap(),
                ])
            }),
            "view_required",
        ),
        (
            Box::new(|x: &mut PreferencesPut| {
                x.nav_shortcuts = Some(vec![
                    serde_json::from_value(json!({"kind": "triage"})).unwrap(),
                    serde_json::from_value(json!({"kind": "triage"})).unwrap(),
                ])
            }),
            "duplicate_shortcut",
        ),
        (
            Box::new(|x: &mut PreferencesPut| {
                x.default_views = Some(BTreeMap::from([(
                    "*".into(),
                    "builtin:project_board:WEB".into(),
                )]))
            }),
            "invalid_default_view",
        ),
    ] {
        let mut x = prefs_put(1);
        f(&mut x);
        let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &x)
            .await
            .unwrap_err();
        assert_eq!(details(&e)["kind"], kind);
    }
    assert_eq!(home::preferences(&w.app, a).await.unwrap().lock_version, 1);

    // Two tabs with the same lock_version at the same time: one wins, the
    // other gets conflict with the current version.
    let mut one = prefs_put(1);
    one.density = Some("compact".into());
    let mut two = prefs_put(1);
    two.theme = Some("light".into());
    let (k1, k2) = (Uuid::now_v7(), Uuid::now_v7());
    let (r1, r2) = common::wait_for(
        "two racing preference writes",
        common::WAIT_TIMEOUT,
        async {
            tokio::join!(
                home::put_preferences(&w.app, a, k1, &one),
                home::put_preferences(&w.app, a, k2, &two)
            )
        },
    )
    .await;
    let (ok, err) = match (r1, r2) {
        (Ok(v), Err(e)) | (Err(e), Ok(v)) => (v, e),
        (a, b) => panic!("expected one conflict: {:?} / {:?}", a.is_ok(), b.is_ok()),
    };
    assert_eq!(ok.lock_version, 2);
    let c = classify(&err).unwrap();
    assert_eq!(c.code, ErrorCode::Conflict);
    let d = c.details.clone().unwrap();
    assert_eq!(d["current_lock_version"], 2);
    assert_eq!(d["current"]["lock_version"], 2);
    // The winner's key replays without a bump; a different body under it
    // is idempotency_key_reused.
    let winner = if ok.density == "compact" {
        (k1, &one)
    } else {
        (k2, &two)
    };
    let again = home::put_preferences(&w.app, a, winner.0, winner.1)
        .await
        .unwrap();
    assert_eq!(again.lock_version, 2);
    let mut other = winner.1.clone();
    other.theme = Some("system".into());
    let e = home::put_preferences(&w.app, a, winner.0, &other)
        .await
        .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::IdempotencyKeyReused);
    assert_eq!(home::preferences(&w.app, a).await.unwrap().lock_version, 2);
    // onboarded false asks again.
    let mut x = prefs_put(2);
    x.onboarded = Some(false);
    let p = home::put_preferences(&w.app, a, Uuid::now_v7(), &x)
        .await
        .unwrap();
    assert!(!p.onboarded);
    // Same for the home: two PUTs on lock 0, one conflict.
    let h = home::home(&w.app, a).await.unwrap();
    let (r1, r2) = common::wait_for("two racing home writes", common::WAIT_TIMEOUT, async {
        tokio::join!(
            put_home(&w, a, h.lock_version, vec![wi("need_decision")]),
            put_home(
                &w,
                a,
                h.lock_version,
                vec![wi("need_decision"), wi("agent_summary")]
            )
        )
    })
    .await;
    assert_eq!(
        [r1.is_ok(), r2.is_ok()].iter().filter(|x| **x).count(),
        1,
        "exactly one wins"
    );
    let e = r1.err().or(r2.err()).unwrap();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::Conflict);
    assert_eq!(home::home(&w.app, a).await.unwrap().lock_version, 1);
}

// ---------------------------------------------------------------- T-M1-61

#[tokio::test]
async fn t61_nobody_reads_anyone_elses() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    // alice changes hers.
    let mut put = prefs_put(0);
    home::preferences(&w.app, w.alice_ctx).await.unwrap();
    put.theme = Some("light".into());
    home::put_preferences(&w.app, w.alice_ctx, Uuid::now_v7(), &put)
        .await
        .unwrap();
    // bob reads only his.
    assert_eq!(home::preferences(&w.app, bob).await.unwrap().theme, "dark");
    // An agent (alice's delegate) has no /me at all.
    let (token, agent) = agent(&w).await;
    for e in [
        home::preferences(&w.app, agent).await.unwrap_err(),
        home::home(&w.app, agent).await.unwrap_err(),
        home::templates(agent, None).unwrap_err(),
        home::put_home(
            &w.app,
            agent,
            Uuid::now_v7(),
            &HomePut {
                lock_version: 0,
                widgets: vec![wi("need_decision")],
            },
        )
        .await
        .unwrap_err(),
    ] {
        assert_eq!(classify(&e).unwrap().code, ErrorCode::HumanSessionRequired);
    }
    // No MCP tool about preferences or the home page.
    for t in oxledger::registry::tools() {
        assert!(
            !t.name.contains("preference") && !t.name.contains("home"),
            "{}",
            t.name
        );
    }
    // No route takes another person: every documented route about them is
    // under /me (or the templates, which are not anyone's).
    for r in oxledger::openapi::session_routes() {
        if r.path.contains("preferences") || r.path.contains("home") {
            assert!(
                r.path.starts_with("/me/") || r.path.starts_with("/home/"),
                "{}",
                r.path
            );
            assert!(!r.path.contains('{'), "{}", r.path);
        }
    }
    // Over HTTP.
    let r = router(&w);
    let b = login(&r, "bob").await;
    let alice = w.alice.to_string();
    for path in [
        format!("/users/{alice}/preferences"),
        format!("/users/{alice}/home"),
        format!("/principals/{alice}/preferences"),
        format!("/me/{alice}/preferences"),
        format!("/me/preferences/{alice}"),
        format!("/me/home/{alice}"),
    ] {
        let (st, v) = call(&r, Some(&b), "GET", &path, None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{path}: {v}");
    }
    // Query parameters naming someone else are ignored: still bob's own.
    let (st, v) = call(
        &r,
        Some(&b),
        "GET",
        &format!("/me/preferences?user={alice}&user_id={alice}"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["data"]["theme"], "dark");
    // A bearer token is not accepted on /api/v1.
    let resp = r
        .clone()
        .oneshot(
            Request::get("/api/v1/me/home")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- re-checked references

/// What an unavailable widget says: nothing but its own stored ids.
fn assert_dark(v: &Value) {
    assert_eq!(v["available"], false, "{v}");
    for k in ["view_name", "view_kind", "project_key", "project_name"] {
        assert!(v.get(k).is_none(), "{k} in {v}");
    }
}

#[tokio::test]
async fn references_rechecked_on_every_read() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    grant(&pool, w.dev, &["manage_public_views", "triage_issues"]).await;
    // Views: a WEB project view, alice's personal view bound to AUTH, one
    // to be deleted, one cross-project; bob's personal view.
    let web_view = view(&w, a, "project", Some("WEB"), "Web open").await;
    let auth_mine = view(&w, a, "personal", Some("AUTH"), "Auth mine").await;
    let doomed = view(&w, a, "personal", None, "Doomed").await;
    let bobs = view(&w, bob, "personal", None, "Bob's").await;
    home::home(&w.app, a).await.unwrap();
    let h = put_home(
        &w,
        a,
        0,
        vec![
            wid("d", "need_decision"),
            wview("web", &web_view),
            wview("auth", &auth_mine),
            wview("doomed", &doomed),
            wview("board", "builtin:project_board:AUTH"),
            wms("ms", w.auth_p),
            wid("tri", "triage_count"),
        ],
    )
    .await
    .unwrap();
    assert!(h.widgets.iter().all(|x| x.available), "{:?}", h.widgets);
    assert_eq!(h.widgets[1].view_name.as_deref(), Some("Web open"));
    assert_eq!(h.widgets[5].project_key.as_deref(), Some("AUTH"));
    let mut put = prefs_put(0);
    put.nav_shortcuts = Some(vec![
        serde_json::from_value(json!({"kind": "view", "id": web_view})).unwrap(),
        serde_json::from_value(json!({"kind": "view", "id": auth_mine})).unwrap(),
        serde_json::from_value(json!({"kind": "triage"})).unwrap(),
    ]);
    put.default_views = Some(BTreeMap::from([
        (w.auth_p.to_string(), "builtin:list".into()),
        (w.web.to_string(), web_view.clone().into()),
        ("*".into(), "builtin:board".into()),
    ]));
    let p = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap();
    assert_eq!(p.default_views.len(), 3);
    assert!(p.nav_shortcuts.iter().all(|s| s.available));

    // New references must be readable now; "missing" and "not visible"
    // read the same.
    let ghost = Uuid::now_v7().to_string();
    let mut shapes = Vec::new();
    for v in [&bobs, &ghost] {
        let e = put_home(&w, a, h.lock_version, {
            let mut x = round_trip(&h);
            x.push(wview("new", v));
            x
        })
        .await
        .unwrap_err();
        let c = classify(&e).unwrap();
        shapes.push((c.code, c.message.clone(), c.details.clone()));
    }
    assert_eq!(shapes[0], shapes[1]);
    assert_eq!(shapes[0].2.as_ref().unwrap()["kind"], "unknown_view");
    // carol (WEB only): AUTH as milestones, or a project that does not
    // exist, are the same unknown_project; a default view for AUTH too.
    home::home(&w.app, carol).await.unwrap();
    let mut shapes = Vec::new();
    for p in [w.auth_p, Uuid::now_v7()] {
        let e = put_home(&w, carol, 0, vec![wi("need_decision"), wms("m", p)])
            .await
            .unwrap_err();
        let c = classify(&e).unwrap();
        shapes.push((c.code, c.message.clone(), c.details.clone()));
        let mut x = prefs_put(0);
        x.default_views = Some(BTreeMap::from([(p.to_string(), "builtin:list".into())]));
        let e = home::put_preferences(&w.app, carol, Uuid::now_v7(), &x)
            .await
            .unwrap_err();
        assert_eq!(details(&e)["kind"], "unknown_project");
    }
    assert_eq!(shapes[0], shapes[1]);
    assert_eq!(shapes[0].2.as_ref().unwrap()["kind"], "unknown_project");
    // carol cannot name alice's personal view or a WEB view in a shortcut
    // as someone else's: alice's is unknown_view.
    let mut x = prefs_put(0);
    x.nav_shortcuts = Some(vec![
        serde_json::from_value(json!({"kind": "view", "id": doomed})).unwrap(),
    ]);
    let e = home::put_preferences(&w.app, carol, Uuid::now_v7(), &x)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "unknown_view");

    // Triage counts list AUTH while alice is a member.
    exec(
        &pool,
        &format!(
            "UPDATE project_triage_settings SET enabled = true, decline_status_id = '{}'",
            w.done
        ),
    )
    .await;
    let keys = |c: oxledger::triage::TriageCounts| {
        c.projects
            .into_iter()
            .map(|p| p.project.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(oxledger::triage::counts(&w.app, a).await.unwrap()),
        vec!["AUTH", "WEB"]
    );

    // 1. alice leaves AUTH: the AUTH view, the AUTH board, the AUTH
    //    milestones, the AUTH default view all go dark the same way.
    exec(
        &pool,
        &format!(
            "DELETE FROM members WHERE project_id = '{}' AND principal_id = '{}'",
            w.auth_p, w.alice
        ),
    )
    .await;
    // 2. the doomed view is deleted.
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM view_defs WHERE id = $1::uuid")
        .bind(&doomed)
        .fetch_one(&pool)
        .await
        .unwrap();
    views::delete(&w.app, a, Uuid::now_v7(), &doomed, lv)
        .await
        .unwrap();
    let h2 = home::home(&w.app, a).await.unwrap();
    let v = serde_json::to_value(&h2.widgets).unwrap();
    assert_eq!(v[1]["available"], true);
    for i in [2, 3, 4, 5] {
        assert_dark(&v[i]);
    }
    // Deleted and invisible look alike, key for key.
    let shape = |x: &Value| {
        let mut k: Vec<String> = x.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(shape(&v[2]), shape(&v[3]));
    let p = home::preferences(&w.app, a).await.unwrap();
    // A default view whose project went away stays, dark, with only the
    // stored id (no silent loss when the GET body is sent back).
    let d = &p.default_views[&w.auth_p.to_string()];
    assert!(!d.available && d.name.is_none() && d.view_id == "builtin:list");
    let d = &p.default_views[&w.web.to_string()];
    assert!(d.available && d.name.as_deref() == Some("Web open"));
    // Sent back as read, it is kept.
    let mut x = prefs_put(p.lock_version);
    x.default_views =
        Some(serde_json::from_value(serde_json::to_value(&p.default_views).unwrap()).unwrap());
    let p2 = home::put_preferences(&w.app, a, Uuid::now_v7(), &x)
        .await
        .unwrap();
    assert_eq!(p2.default_views.len(), 3);
    assert_eq!(p2.lock_version, p.lock_version);
    assert!(p.nav_shortcuts[0].available && !p.nav_shortcuts[1].available);
    assert!(p.nav_shortcuts[1].name.is_none());
    // The data endpoints the widgets use say the same: the AUTH view and
    // versions are gone, triage counts no longer list AUTH (no count).
    assert!(
        views::run(&w.app, a, &auth_mine, &RunInput::default())
            .await
            .is_err()
    );
    assert!(
        oxledger::versions::list(&w.app, a, &w.auth_p.to_string(), None, Some("stats"))
            .await
            .is_err()
    );
    assert_eq!(
        keys(oxledger::triage::counts(&w.app, a).await.unwrap()),
        vec!["WEB"]
    );
    // The home can still be rearranged with the dark widgets kept as they
    // were (unchanged references are not re-validated)...
    let mut x = round_trip(&h2);
    x.reverse();
    let h3 = put_home(&w, a, h2.lock_version, x).await.unwrap();
    assert_eq!(h3.widgets.len(), 7);
    // ...but a dark reference cannot be moved to another widget id.
    let mut x = round_trip(&h3);
    x.push(wview("again", &auth_mine));
    let e = put_home(&w, a, h3.lock_version, x).await.unwrap_err();
    assert_eq!(details(&e)["kind"], "unknown_view");
    // Coming back makes them readable again.
    member(&pool, w.auth_p, w.alice, w.dev).await;
    let h4 = home::home(&w.app, a).await.unwrap();
    assert!(
        h4.widgets
            .iter()
            .filter(|x| x.id != "doomed")
            .all(|x| x.available)
    );

    // 3. The issue tracking module off in WEB: the WEB view goes dark.
    exec(
        &pool,
        &format!(
            "DELETE FROM enabled_modules WHERE project_id = '{}' AND module = 'issue_tracking'",
            w.web
        ),
    )
    .await;
    let h5 = home::home(&w.app, a).await.unwrap();
    let web = h5.widgets.iter().find(|x| x.id == "web").unwrap();
    assert_dark(&serde_json::to_value(web).unwrap());
    let p = home::preferences(&w.app, a).await.unwrap();
    assert!(!p.nav_shortcuts[0].available);
    let d = &p.default_views[&w.web.to_string()];
    assert!(!d.available && d.name.is_none());
    exec(
        &pool,
        &format!(
            "INSERT INTO enabled_modules VALUES ('{}', 'issue_tracking')",
            w.web
        ),
    )
    .await;
    assert!(
        home::home(&w.app, a)
            .await
            .unwrap()
            .widgets
            .iter()
            .find(|x| x.id == "web")
            .unwrap()
            .available
    );

    // 4. view_issues revoked: everything project-bound goes dark.
    exec(
        &pool,
        &format!(
            "DELETE FROM role_permissions WHERE role_id = '{}' AND permission = 'view_issues'",
            w.dev
        ),
    )
    .await;
    let h6 = home::home(&w.app, a).await.unwrap();
    for id in ["web", "auth", "board", "ms"] {
        let x = h6.widgets.iter().find(|x| x.id == id).unwrap();
        assert_dark(&serde_json::to_value(x).unwrap());
    }
    // Built-in cross-project views stay (they run on what alice can see).
    let p = home::preferences(&w.app, a).await.unwrap();
    let live: Vec<_> = p
        .default_views
        .iter()
        .filter(|(_, d)| d.available)
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(live, vec!["*"]);
}

// ---------------------------------------------------------------- CHECK lists

async fn check_values(pool: &sqlx::PgPool, column: &str) -> Vec<String> {
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c
         JOIN pg_class t ON t.oid = c.conrelid
         WHERE t.relname = 'user_preferences' AND c.contype = 'c'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let def = defs
        .iter()
        .find(|d| {
            d.starts_with(&format!("CHECK (({column} "))
                || d.starts_with(&format!("CHECK ({column} "))
        })
        .unwrap_or_else(|| panic!("{column}: no CHECK in {defs:?}"));
    let mut out = Vec::new();
    let mut rest = def.as_str();
    while let Some(a) = rest.find('\'') {
        let tail = &rest[a + 1..];
        let b = tail.find('\'').unwrap();
        out.push(tail[..b].to_string());
        rest = &tail[b + 1..];
    }
    out.sort();
    out
}

#[tokio::test]
async fn check_lists_match_constants() {
    let pool = test_pool().await;
    let sorted = |xs: &[&str]| {
        let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };
    assert_eq!(
        check_values(&pool, "interface_roles").await,
        sorted(&home::ROLES)
    );
    assert_eq!(check_values(&pool, "theme").await, sorted(&home::THEMES));
    assert_eq!(
        check_values(&pool, "density").await,
        sorted(&home::DENSITIES)
    );
    // Only users have preferences (trigger).
    let w = World::build(&pool).await;
    let (_, a) = agent(&w).await;
    let e = sqlx::query("INSERT INTO user_preferences (user_id, home) VALUES ($1, '{}')")
        .bind(a.principal_id().unwrap())
        .execute(&pool)
        .await;
    assert!(e.is_err());
}

// ---------------------------------------------------------------- HTTP

const ORIGIN_SPA: &str = "https://spa.test";

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
async fn m1_6_http() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let r = router(&w);
    for (m, path) in [
        ("GET", "/me/preferences"),
        ("GET", "/me/home"),
        ("GET", "/home/templates"),
    ] {
        let (st, v) = call(&r, None, m, path, None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{path}: {v}");
    }
    let b = login(&r, "alice").await;
    let (st, v) = call(&r, Some(&b), "GET", "/me/preferences", None, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let p: Value = v["data"].clone();
    assert_eq!(p["onboarded"], false);
    assert_eq!(p["lock_version"], 0);
    // Writes need an Idempotency-Key.
    let body = json!({"lock_version": 0, "interface_roles": ["pm"], "onboarded": true});
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/preferences",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["details"]["kind"], "idempotency_key");
    let key = Uuid::now_v7();
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/preferences",
        Some(body.clone()),
        Some(key),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["onboarded"], true);
    assert_eq!(v["data"]["lock_version"], 1);
    // Replay: same answer, no second bump.
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/preferences",
        Some(body),
        Some(key),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["lock_version"], 1);
    // Unknown fields are refused.
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/preferences",
        Some(json!({"lock_version": 1, "user_id": w.bob})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // Apply the pm template.
    let (st, v) = call(
        &r,
        Some(&b),
        "POST",
        "/me/home/apply-template",
        Some(json!({"roles": ["pm"], "mode": "replace"})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["home"]["widgets"][1]["type"], "triage_count");
    assert_eq!(
        v["data"]["preferences"]["default_views"]["*"]["view_id"],
        "builtin:timeline"
    );
    let lv = v["data"]["home"]["lock_version"].clone();
    // The GET body goes back as a PUT body (read-only fields ignored).
    let (st, v) = call(&r, Some(&b), "GET", "/me/home", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["data"]["widgets"][0]["protected"], true);
    let mut widgets = v["data"]["widgets"].clone();
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/home",
        Some(json!({"lock_version": lv, "widgets": widgets.clone()})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Removing need_decision: 422 invalid_value protected_widget_missing.
    widgets.as_array_mut().unwrap().remove(0);
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/home",
        Some(json!({"lock_version": lv, "widgets": widgets})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(v["error"]["code"], "invalid_value", "{v}");
    assert_eq!(v["error"]["details"]["kind"], "protected_widget_missing");
    assert!(st.is_client_error());
    // A stale lock_version: conflict with the current home.
    let (st, v) = call(
        &r,
        Some(&b),
        "PUT",
        "/me/home",
        Some(json!({"lock_version": 0, "widgets": [{"type": "need_decision"}]})),
        Some(Uuid::now_v7()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"]["details"]["current"]["lock_version"], lv);
    // Templates.
    let (st, v) = call(
        &r,
        Some(&b),
        "GET",
        "/home/templates?roles=engineer,pm",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["roles"], json!(["engineer", "pm"]));
    assert_eq!(v["data"]["default_views"]["*"], "builtin:board");
    let (st, v) = call(
        &r,
        Some(&b),
        "GET",
        "/home/templates?roles=boss",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // Preferences as read.
    let (_, v) = call(&r, Some(&b), "GET", "/me/preferences", None, None).await;
    assert_eq!(v["data"]["interface_roles"], json!(["pm"]));
}

// ---------------------------------------------------------------- independent verification (M1.6)

/// V1: `builtin:project_board:` / `builtin:project_issues:` take the
/// project part through `describe::visible_project`, which trims it, while
/// the stored `view_id` is the raw input (only the outer whitespace is
/// trimmed). So a ~1 MB value passes `usable` and is stored as-is in the
/// home (and the same in a shortcut id and a default view value): the
/// widget/shortcut/default-view counts are capped, their size is not.
/// Expected (fixed): over 80 characters is `invalid_value`
/// (`invalid_view_id`); a short padded spelling is stored in canonical form.
#[tokio::test]
async fn verify_m16_padded_builtin_view_id_is_stored_verbatim() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let h = home::home(&w.app, a).await.unwrap();
    let padded = format!("builtin:project_board:{}AUTH", " ".repeat(500_000));
    let r = put_home(
        &w,
        a,
        h.lock_version,
        vec![wi("need_decision"), wview("big", &padded)],
    )
    .await;
    let stored: i32 = sqlx::query_scalar(
        "SELECT octet_length(home::text) FROM user_preferences WHERE user_id = $1",
    )
    .bind(w.alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    let e = r.unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "invalid_view_id");
    assert!(stored < 10_000, "home is {stored} bytes");
    // The same through a shortcut and a default view (AUTH's own board).
    let p = home::preferences(&w.app, a).await.unwrap();
    let mut put = prefs_put(p.lock_version);
    put.nav_shortcuts = Some(vec![
        serde_json::from_value(json!({"kind": "view", "id": padded})).unwrap(),
    ]);
    put.default_views = Some(BTreeMap::from([(
        w.auth_p.to_string(),
        padded.clone().into(),
    )]));
    let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "invalid_view_id");
    put.nav_shortcuts = None;
    let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "invalid_view_id");
    // A short padded spelling is stored canonically (project by id) in the
    // widget, the shortcut and the default view.
    let short = "builtin:project_board:  auth ";
    let canon = format!("builtin:project_board:{}", w.auth_p);
    let h = home::home(&w.app, a).await.unwrap();
    let h = put_home(
        &w,
        a,
        h.lock_version,
        vec![wi("need_decision"), wview("b", short)],
    )
    .await
    .unwrap();
    assert_eq!(h.widgets[1].view_id.as_deref(), Some(canon.as_str()));
    let p = home::preferences(&w.app, a).await.unwrap();
    let mut put = prefs_put(p.lock_version);
    put.nav_shortcuts = Some(vec![
        serde_json::from_value(json!({"kind": "view", "id": short})).unwrap(),
    ]);
    put.default_views = Some(BTreeMap::from([(w.auth_p.to_string(), short.into())]));
    let p = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap();
    assert_eq!(p.nav_shortcuts[0].id.as_deref(), Some(canon.as_str()));
    assert_eq!(p.default_views[&w.auth_p.to_string()].view_id, canon);
}

/// V2: two spellings of one project id in `default_views` (upper and lower
/// case) are both accepted and silently collapse into one entry; which one
/// wins depends on the map order of the raw keys. Expected: a
/// `duplicate`-style `invalid_value` (or at least a deterministic rule).
#[tokio::test]
async fn verify_m16_default_views_duplicate_project_spellings() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let p = home::preferences(&w.app, a).await.unwrap();
    let mut put = prefs_put(p.lock_version);
    put.default_views = Some(BTreeMap::from([
        (w.web.to_string().to_uppercase(), "builtin:board".into()),
        (w.web.to_string(), "builtin:timeline".into()),
    ]));
    let r = home::put_preferences(&w.app, a, Uuid::now_v7(), &put).await;
    match r {
        Err(e) => assert_eq!(classify(&e).unwrap().code, ErrorCode::InvalidValue),
        Ok(p) => panic!(
            "two entries for one project accepted, kept {:?}",
            p.default_views.get(&w.web.to_string())
        ),
    }
}

/// V3 (coverage; expected to pass): the agent is refused on the two writes
/// the existing test does not call (preferences PUT, apply-template), keys
/// are per person (bob may reuse alice's key), a key used on one /me write
/// cannot be replayed on another, and the 20 / 100 caps.
#[tokio::test]
async fn verify_m16_agent_keys_and_caps() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let (_, agent) = agent(&w).await;
    let e = home::put_preferences(&w.app, agent, Uuid::now_v7(), &prefs_put(0))
        .await
        .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::HumanSessionRequired);
    let e = home::apply_template(
        &w.app,
        agent,
        Uuid::now_v7(),
        &ApplyTemplate {
            roles: vec!["pm".into()],
            mode: "replace".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::HumanSessionRequired);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_preferences WHERE user_id = $1")
        .bind(agent.principal_id().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // One key, two people: independent.
    home::preferences(&w.app, a).await.unwrap();
    home::preferences(&w.app, bob).await.unwrap();
    let key = Uuid::now_v7();
    let mut x = prefs_put(0);
    x.theme = Some("light".into());
    let pa = home::put_preferences(&w.app, a, key, &x).await.unwrap();
    let mut y = prefs_put(0);
    y.theme = Some("system".into());
    let pb = home::put_preferences(&w.app, bob, key, &y).await.unwrap();
    assert_eq!((pa.theme.as_str(), pb.theme.as_str()), ("light", "system"));
    // The same key on another /me write: reused.
    let e = home::put_home(
        &w.app,
        a,
        key,
        &HomePut {
            lock_version: 0,
            widgets: vec![wi("need_decision")],
        },
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::IdempotencyKeyReused);
    let e = home::apply_template(
        &w.app,
        a,
        key,
        &ApplyTemplate {
            roles: vec![],
            mode: "merge".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&e).unwrap().code, ErrorCode::IdempotencyKeyReused);

    // Caps: 21 shortcuts, 101 default views.
    let mut x = prefs_put(1);
    x.nav_shortcuts = Some(
        (0..21)
            .map(|i| {
                serde_json::from_value(json!({"kind": "view", "id": format!("builtin:x{i}")}))
                    .unwrap()
            })
            .collect(),
    );
    let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &x)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "too_many_shortcuts");
    let mut x = prefs_put(1);
    x.default_views = Some(
        (0..101)
            .map(|_| (Uuid::now_v7().to_string(), "builtin:list".into()))
            .collect(),
    );
    let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &x)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "too_many_default_views");
    // Nothing moved.
    assert_eq!(home::preferences(&w.app, a).await.unwrap().lock_version, 1);
}

/// V4 (coverage; expected to pass): a project view of a project that
/// becomes private (alice not a member) and of an archived project; a
/// milestones widget of an archived project; deleted and hidden widgets
/// carry exactly the same keys and the same values except their own ids.
#[tokio::test]
async fn verify_m16_archived_and_conflict_details_do_not_leak() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let carol = sign_in(&pool, &w.policy, "carol").await;
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let auth_view = view(&w, a, "project", Some("AUTH"), "Auth secret plan").await;
    home::home(&w.app, a).await.unwrap();
    let h = put_home(
        &w,
        a,
        0,
        vec![
            wid("d", "need_decision"),
            wview("v", &auth_view),
            wms("ms", w.auth_p),
        ],
    )
    .await
    .unwrap();
    assert!(h.widgets.iter().all(|x| x.available));
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'archived' WHERE id = '{}'",
            w.auth_p
        ),
    )
    .await;
    let h2 = home::home(&w.app, a).await.unwrap();
    let ms = h2.widgets.iter().find(|x| x.id == "ms").unwrap();
    assert!(!ms.available && ms.project_key.is_none() && ms.project_name.is_none());
    // The conflict's `current` is rendered with the same rules.
    let e = put_home(&w, a, 0, round_trip(&h2)).await.unwrap_err();
    assert_eq!(details(&e)["current"], serde_json::to_value(&h2).unwrap());
    // carol never saw AUTH: nothing about it in any of her answers.
    home::home(&w.app, carol).await.unwrap();
    let e = put_home(
        &w,
        carol,
        0,
        vec![wi("need_decision"), wview("x", &auth_view)],
    )
    .await
    .unwrap_err();
    let s = format!("{e:#} {}", details(&e));
    assert!(
        !s.contains("Auth secret plan") && !s.contains("AUTH"),
        "{s}"
    );
}

/// V5 (test gap; expected to pass): keeping a widget id while changing its
/// view re-checks the new view (a mutant that keeps by id alone survives
/// the existing tests).
#[tokio::test]
async fn verify_m16_same_widget_id_new_view_is_checked() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let bobs = view(&w, bob, "personal", None, "Bob's").await;
    let h = home::home(&w.app, a).await.unwrap();
    let mut x = round_trip(&h);
    let i = x.iter().position(|w| w.widget_type == "view").unwrap();
    x[i].view_id = Some(bobs);
    let e = put_home(&w, a, h.lock_version, x).await.unwrap_err();
    assert_eq!(details(&e)["kind"], "unknown_view");
}

/// V6 (test gap; expected to pass): a default view for WEB that is bound
/// to AUTH (both readable) is `unknown_view`.
#[tokio::test]
async fn verify_m16_default_view_bound_to_other_project() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let auth_view = view(&w, a, "project", Some("AUTH"), "Auth list").await;
    let p = home::preferences(&w.app, a).await.unwrap();
    let mut put = prefs_put(p.lock_version);
    put.default_views = Some(BTreeMap::from([(
        w.web.to_string(),
        auth_view.clone().into(),
    )]));
    let e = home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "unknown_view");
    put.default_views = Some(BTreeMap::from([(w.auth_p.to_string(), auth_view.into())]));
    home::put_preferences(&w.app, a, Uuid::now_v7(), &put)
        .await
        .unwrap();
}

/// V7 (coverage; expected to pass): a milestones widget on an archived
/// project is `unknown_project` on PUT. Note: removing the
/// `versions::readable` check in `check_widgets` still passes this (an
/// archived project is already outside `visible_project`), so that check
/// looks equivalent to the scope check today.
#[tokio::test]
async fn verify_m16_milestones_put_needs_readable_versions() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    home::home(&w.app, a).await.unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'archived' WHERE id = '{}'",
            w.auth_p
        ),
    )
    .await;
    let e = put_home(&w, a, 0, vec![wi("need_decision"), wms("m", w.auth_p)])
        .await
        .unwrap_err();
    assert_eq!(details(&e)["kind"], "unknown_project");
}
