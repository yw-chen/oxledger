// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6b.3: what leaves through the agent's exits (§8.8 response
//! filter, §9.10): T-M9…T-M13, T-M15…T-M20 over the real `/mcp` route, and
//! the §8.9 quotas S6 left (validate per hour, two proposal writes at a
//! time, payload sizes). Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use http_body_util::BodyExt;
use oxledger::agents;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::auth::AuthContext;
use oxledger::auth::SessionPolicy;
use oxledger::changesets::faults::{self, Action, Gate};
use oxledger::changesets::{self, ApproveInput};
use oxledger::commands;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
use world::mcp_client::Mcp;
use world::*;

fn router(w: &World) -> Router {
    api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: "https://spa.test".into(),
            session: SessionPolicy::default(),
        },
    )
}

async fn mk(w: &World, project: Uuid, subject: &str, desc: &str, private: bool) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: project,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description: desc,
            author_id: w.bob,
            assigned_to_id: None,
            is_private: private,
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

/// alice (a person) adds a note: an event and a lock_version bump.
async fn human_note(w: &World, i: Uuid, body: &str) {
    let l = lv(w, i).await;
    commands::submit(&w.app, w.alice_ctx, Uuid::now_v7(), &note_doc(i, l, body))
        .await
        .unwrap();
}

async fn approve(w: &World, cs: Uuid, version: i32) -> String {
    let full = changesets::get(&w.app, w.alice_ctx, cs, None)
        .await
        .unwrap();
    let full = serde_json::to_value(&full).unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &ApproveInput {
            version,
            proposal_hash: full["proposal_hash"].as_str().unwrap().to_string(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    o.execution.unwrap().status
}

async fn owner_revise(w: &World, cs: Uuid, expected: i32, proposal: Value) {
    changesets::owner_revise(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        cs,
        &json!({"expected_current_version": expected, "proposal": proposal}),
    )
    .await
    .unwrap();
}

fn uid(v: &Value) -> Uuid {
    serde_json::from_value(v.clone()).unwrap_or_else(|_| panic!("not a uuid: {v}"))
}

/// A world with WEB and AUTH delegated in one context, and the agent's MCP.
/// With `both`, AUTH too; carol joins AUTH first so the two projects have
/// the same readers (otherwise WEB writes are cross_project, §3.8).
async fn setup_with(pool: &sqlx::PgPool, both: bool) -> (World, Mcp, Uuid, Uuid, AuthContext) {
    faults::clear();
    let w = World::build(pool).await;
    if both {
        member(pool, w.auth_p, w.carol, w.dev).await;
    }
    let ctx = w.new_context().await;
    let d_web = w.delegate(w.web).await.unwrap().id;
    let d_auth = if both {
        w.delegate(w.auth_p).await.unwrap().id
    } else {
        Uuid::nil()
    };
    let (token, agent) = w.token(ctx).await;
    let m = Mcp::connect(router(&w), token).await;
    (w, m, d_web, d_auth, agent)
}

async fn setup(pool: &sqlx::PgPool) -> (World, Mcp, Uuid, Uuid, AuthContext) {
    setup_with(pool, false).await
}

fn private_create(project: Uuid, tracker: Uuid, subject: &str) -> Value {
    json!({"command_schema_version": 1, "title": "agent proposal",
           "ops": [{"op_id": op(), "kind": "create_issue", "project_id": project, "tracker_id": tracker,
                    "fields": {"subject": subject, "is_private": true}}]})
}

// ---------------------------------------------------------------- T-M9, T-M10

#[tokio::test]
async fn reads_events_and_actions() {
    let pool = test_pool().await;
    let (w, mut m, _, _, _) = setup(&pool).await;

    // T-M9: 10 issues with 20 events each; event_limit 3.
    let mut keys = Vec::new();
    for n in 0..10 {
        let (i, k) = mk(&w, w.web, &format!("events {n}"), "", false).await;
        for e in 0..19 {
            human_note(&w, i, &format!("note {e}")).await;
        }
        keys.push(k);
    }
    let r = m
        .call(
            "get_many",
            json!({"keys": keys, "fields": ["subject"], "include": ["recent_events"], "event_limit": 3}),
        )
        .await
        .unwrap();
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 10);
    for it in items {
        assert_eq!(it["outcome"], "ok", "{it}");
        let ev = &it["issue"]["recent_events"];
        assert_eq!(ev["items"].as_array().unwrap().len(), 3, "{ev}");
        assert_eq!(ev["truncated"], true, "{ev}");
    }

    // T-M10: a delegation of add_note only; alice herself may edit.
    let ctx2 = w.new_context().await;
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        agents::DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: vec!["add_note".into()],
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
    let (t2, _) = w.token(ctx2).await;
    let mut m2 = Mcp::connect(router(&w), t2).await;
    let g = m2
        .call(
            "get",
            json!({"key": keys[0], "fields": ["subject"], "include": ["actions"]}),
        )
        .await
        .unwrap();
    let a = &g["actions"];
    assert_eq!(a["fields"], json!([]), "{a}");
    assert_eq!(a["transitions"], json!([]), "{a}");
    assert_eq!(a["notes"]["can_add"], true, "{a}");
    assert!(a.get("duplicate_of").is_none(), "{a}");
    for bad in ["can_apply", "can_approve", "applicable"] {
        assert!(!g.to_string().contains(bad), "{g}");
    }
}

// ---------------------------------------------------------------- T-M11, T-M12, T-M19

#[tokio::test]
async fn revoked_and_private_results() {
    let pool = test_pool().await;
    let (w, mut m, d_web, _d_auth, _) = setup_with(&pool, true).await;
    let (i, _) = mk(&w, w.web, "revoke me", "", false).await;
    let (ia, _) = mk(&w, w.auth_p, "auth one", "", false).await;

    // T-M19: both projects listed.
    let p_web = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note_doc(i, 0, "web note"),
                   "group_label": "web only"}),
        )
        .await
        .unwrap();
    let g_web = p_web["group_id"].clone();
    m.call(
        "changeset_propose",
        json!({"operation_id": Uuid::now_v7(), "proposal": note_doc(ia, 0, "auth note")}),
    )
    .await
    .unwrap();
    let l = m.call("changeset_list", json!({})).await.unwrap();
    assert_eq!(l["items"].as_array().unwrap().len(), 2, "{l}");

    // T-M12: a private issue, approved; the agent sees only {op_id, status}.
    let op12 = Uuid::now_v7();
    let doc12 = private_create(w.web, w.bug, "PRIVATE-TITLE-12");
    let p12 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op12, "proposal": doc12.clone(),
                                          "group_label": "private"}),
        )
        .await
        .unwrap();
    let cs12 = uid(&p12["changeset_id"]);
    assert_eq!(approve(&w, cs12, 1).await, "succeeded");
    let (new_id, new_key): (Uuid, String) = sqlx::query_as(
        "SELECT i.id, k.key FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
         WHERE i.subject = 'PRIVATE-TITLE-12'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let outs = vec![
        m.call("changeset_get", json!({"changeset_id": cs12})).await,
        m.call("changeset_list", json!({})).await,
        m.call("changeset_list", json!({"group_id": p12["group_id"]}))
            .await,
        m.call("operation_get", json!({"operation_id": op12})).await,
        m.call(
            "changeset_propose",
            json!({"operation_id": op12, "proposal": doc12,
                                           "group_label": "private"}),
        )
        .await,
    ];
    for o in &outs {
        let t = match o {
            Ok(v) | Err(v) => v.to_string(),
        };
        assert!(o.is_ok(), "{t}");
        for leak in [new_key.as_str(), "PRIVATE-TITLE-12", &new_id.to_string()] {
            assert!(!t.contains(leak), "{leak} in {t}");
        }
    }
    let got = outs[0].as_ref().unwrap();
    let items = &got["last_execution"]["items"];
    for it in items.as_array().unwrap() {
        let mut ks: Vec<&String> = it.as_object().unwrap().keys().collect();
        ks.sort();
        assert_eq!(ks, vec!["op_id", "status"], "{it}");
    }

    // T-M11: applied, then the WEB delegation is revoked (AUTH keeps the context).
    let op11 = Uuid::now_v7();
    let p11 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op11, "proposal": note_doc(i, 0, "n11")}),
        )
        .await
        .unwrap();
    assert_eq!(approve(&w, uid(&p11["changeset_id"]), 1).await, "succeeded");
    agents::revoke_delegation(&pool, &w.alice_ctx, d_web)
        .await
        .unwrap();
    let e = m
        .call(
            "changeset_get",
            json!({"changeset_id": p11["changeset_id"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "delegation_unavailable", "{e}");
    let e = m
        .call("operation_get", json!({"operation_id": op11}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], "delegation_unavailable", "{e}");

    // T-M19: after revoking WEB only AUTH is listed; a WEB-only group is empty.
    let l = m.call("changeset_list", json!({})).await.unwrap();
    let rows = l["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{l}");
    let l = m
        .call("changeset_list", json!({"group_id": g_web}))
        .await
        .unwrap();
    assert_eq!(l["items"], json!([]), "{l}");
}

// ---------------------------------------------------------------- T-M15…T-M18

#[tokio::test]
async fn owner_revisions_do_not_leak() {
    let pool = test_pool().await;
    let (w, mut m, _, _, _) = setup(&pool).await;

    // T-M15: a private note revised by the owner with new text.
    let (i, _) = mk(&w, w.web, "private note", "", false).await;
    let op15 = Uuid::now_v7();
    let note15 = json!({"command_schema_version": 1, "title": "agent proposal",
        "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i), "body": "agent text", "is_private": true}],
        "preconditions": [{"target": tgt(i), "lock_version": 0}]});
    let p15 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op15, "proposal": note15}),
        )
        .await
        .unwrap();
    let cs15 = uid(&p15["changeset_id"]);
    owner_revise(
        &w,
        cs15,
        1,
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i),
                        "body": "agent text OWNER-ADDED-15", "is_private": true}],
               "preconditions": [{"target": tgt(i), "lock_version": 0}]}),
    )
    .await;
    let outs = [
        m.call("changeset_get", json!({"changeset_id": cs15}))
            .await
            .unwrap(),
        m.call("changeset_get", json!({"changeset_id": cs15, "version": 2}))
            .await
            .unwrap(),
        m.call("changeset_list", json!({})).await.unwrap(),
        m.call("operation_get", json!({"operation_id": op15}))
            .await
            .unwrap(),
    ];
    for o in &outs {
        assert!(!o.to_string().contains("OWNER-ADDED-15"), "{o}");
    }
    assert_eq!(
        outs[0]["content_unavailable"], "owner_revised",
        "{}",
        outs[0]
    );
    assert_eq!(
        outs[1]["content_unavailable"], "owner_revised",
        "{}",
        outs[1]
    );

    // T-M16: the owner changes the description to Z; a conflict follows.
    let (i16, _) = mk(&w, w.web, "public desc", "old", false).await;
    let op16 = Uuid::now_v7();
    let set = |d: &str| {
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i16), "set": {"description": d}}],
               "preconditions": [{"target": tgt(i16), "lock_version": 0}]})
    };
    let p16 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op16, "proposal": set("agent desc")}),
        )
        .await
        .unwrap();
    let cs16 = uid(&p16["changeset_id"]);
    owner_revise(&w, cs16, 1, set("ZED-SECRET-16")).await;
    human_note(&w, i16, "someone else").await;
    assert_eq!(approve(&w, cs16, 2).await, "failed");
    let outs = [
        m.call(
            "changeset_validate",
            json!({"changeset_id": cs16, "version": 2}),
        )
        .await,
        m.call("changeset_get", json!({"changeset_id": cs16})).await,
        m.call("operation_get", json!({"operation_id": op16})).await,
    ];
    for o in &outs {
        let t = match o {
            Ok(v) | Err(v) => v.to_string(),
        };
        assert!(!t.contains("ZED-SECRET-16"), "{t}");
        assert!(!t.contains("\"proposed\""), "{t}");
    }

    // T-M17: a hidden precondition value Z that does not match (conflict).
    let (i17, _) = mk(&w, w.web, "hidden pre", "real", false).await;
    let op17 = Uuid::now_v7();
    let p17 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op17, "proposal": subject_doc(i17, 0, "agent subject")}),
        )
        .await
        .unwrap();
    let cs17 = uid(&p17["changeset_id"]);
    owner_revise(
        &w,
        cs17,
        1,
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": op(), "kind": "update_fields", "target": tgt(i17), "set": {"subject": "x"}}],
               "preconditions": [{"target": tgt(i17), "lock_version": 0,
                                  "field_values": {"description": "ZED-PRE-17"}}]}),
    )
    .await;
    let outs = [
        m.call(
            "changeset_validate",
            json!({"changeset_id": cs17, "version": 2}),
        )
        .await,
        m.call("changeset_get", json!({"changeset_id": cs17})).await,
        m.call("changeset_list", json!({})).await,
        m.call("operation_get", json!({"operation_id": op17})).await,
    ];
    for o in &outs {
        let t = match o {
            Ok(v) | Err(v) => v.to_string(),
        };
        assert!(!t.contains("ZED-PRE-17"), "{t}");
    }

    // T-M18: the owner picks one of two candidates; the agent replays both
    // under the same op_id. No owner hash (or anything derived) comes back.
    let (i18, _) = mk(&w, w.web, "low entropy", "", false).await;
    let fixed = op();
    let cand = |b: &str| {
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": fixed, "kind": "add_note", "target": tgt(i18), "body": b, "is_private": true}],
               "preconditions": [{"target": tgt(i18), "lock_version": 0}]})
    };
    let op18 = Uuid::now_v7();
    let p18 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op18, "proposal": cand("yes")}),
        )
        .await
        .unwrap();
    let cs18 = uid(&p18["changeset_id"]);
    owner_revise(&w, cs18, 1, cand("no")).await;
    let owner_hash: Vec<u8> = sqlx::query_scalar(
        "SELECT proposal_hash FROM changeset_versions WHERE changeset_id = $1 AND version = 2",
    )
    .bind(cs18)
    .fetch_one(&pool)
    .await
    .unwrap();
    let hex: String = owner_hash.iter().map(|b| format!("{b:02x}")).collect();
    let mut seen = String::new();
    for c in ["yes", "no"] {
        for o in [
            m.call(
                "changeset_propose",
                json!({"operation_id": op18, "proposal": cand(c)}),
            )
            .await,
            m.call(
                "changeset_propose",
                json!({"operation_id": Uuid::now_v7(), "proposal": cand(c)}),
            )
            .await,
            m.call("changeset_get", json!({"changeset_id": cs18})).await,
            m.call("changeset_get", json!({"changeset_id": cs18, "version": 2}))
                .await,
            m.call(
                "changeset_validate",
                json!({"changeset_id": cs18, "version": 2}),
            )
            .await,
            m.call("changeset_list", json!({})).await,
            m.call("operation_get", json!({"operation_id": op18})).await,
        ] {
            seen.push_str(&match o {
                Ok(v) | Err(v) => v.to_string(),
            });
        }
    }
    assert!(!seen.contains(&hex), "owner hash leaked");
    assert!(!seen.contains(&hex[..16]), "owner hash prefix leaked");
    let b64 = oxledger::auth::encode(&owner_hash);
    assert!(!seen.contains(&b64), "owner hash (base64) leaked");
}

// ---------------------------------------------------------------- T-M20

/// Replaces UUIDs (in order of first appearance) and timestamps.
fn normalize(v: &Value, map: &mut HashMap<String, usize>, hashes: &[String]) -> Value {
    match v {
        Value::String(s) => {
            if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                return json!("<t>");
            }
            let mut s = s.clone();
            for (n, h) in hashes.iter().enumerate() {
                s = s.replace(h, &format!("<h{n}>"));
            }
            let mut out = String::new();
            let mut rest = s.as_str();
            while !rest.is_empty() {
                if rest.len() >= 36
                    && rest.is_char_boundary(36)
                    && Uuid::parse_str(&rest[..36]).is_ok()
                    && rest.as_bytes()[8] == b'-'
                {
                    let n = map.len();
                    let k = *map.entry(rest[..36].to_string()).or_insert(n);
                    out.push_str(&format!("<u{k}>"));
                    rest = &rest[36..];
                } else {
                    let c = rest.chars().next().unwrap();
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize(x, map, hashes)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "request_id")
                .map(|(k, x)| (k.clone(), normalize(x, map, hashes)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// One world of T-M20: the owner's draft carries a hidden precondition value
/// that matches (`"base"`) or not. Returns the agent's view before and after
/// the approval.
async fn tm20_world(pool: &sqlx::PgPool, hidden: &str) -> (Vec<Value>, Vec<Value>) {
    let (w, mut m, _, _, _) = setup(pool).await;
    let (i, _) = mk(&w, w.web, "twin", "base", false).await;
    let fixed_op = "0190f000-0000-7000-8000-000000000001";
    let agent_doc = json!({"command_schema_version": 1, "title": "agent proposal",
        "ops": [{"op_id": fixed_op, "kind": "update_fields", "target": tgt(i), "set": {"subject": "agent"}}],
        "preconditions": [{"target": tgt(i), "lock_version": 0}]});
    let opk: Uuid = "0190f000-0000-7000-8000-0000000000aa".parse().unwrap();
    let p = m
        .call(
            "changeset_propose",
            json!({"operation_id": opk, "proposal": agent_doc}),
        )
        .await
        .unwrap();
    let cs = uid(&p["changeset_id"]);
    let h1 = p["proposal_hash"].as_str().unwrap_or("-").to_string();
    owner_revise(
        &w,
        cs,
        1,
        json!({"command_schema_version": 1, "title": "agent proposal",
               "ops": [{"op_id": fixed_op, "kind": "update_fields", "target": tgt(i), "set": {"subject": "owner"}}],
               "preconditions": [{"target": tgt(i), "lock_version": 0,
                                  "field_values": {"description": hidden}}]}),
    )
    .await;
    let mut map = HashMap::new();
    let hs = [h1];
    let look = async |m: &mut Mcp, map: &mut HashMap<String, usize>| {
        let mut v = Vec::new();
        for (t, a) in [
            (
                "changeset_validate",
                json!({"changeset_id": cs, "version": 2}),
            ),
            ("changeset_get", json!({"changeset_id": cs})),
            ("changeset_list", json!({})),
            ("operation_get", json!({"operation_id": opk})),
        ] {
            let r = match m.call(t, a).await {
                Ok(x) => json!({"ok": x}),
                Err(x) => json!({"err": x}),
            };
            v.push(normalize(&r, map, &hs));
        }
        v
    };
    let before = look(&mut m, &mut map).await;
    approve(&w, cs, 2).await;
    let after = look(&mut m, &mut map).await;
    (before, after)
}

#[tokio::test]
async fn twin_worlds_differ_only_in_outcome() {
    let pool = test_pool().await;
    let (ba, aa) = tm20_world(&pool, "base").await;
    let (bb, ab) = tm20_world(&pool, "other").await;
    assert_eq!(ba, bb, "before approval the worlds must look identical");
    // `consistent_after` (the search token of the agent's own write) exists
    // exactly when the write happened: it says nothing beyond the outcome.
    fn drop_key(v: &Value, key: &str) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .filter(|(k, _)| k.as_str() != key)
                    .map(|(k, x)| (k.clone(), drop_key(x, key)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| drop_key(x, key)).collect()),
            _ => v.clone(),
        }
    }
    let strip = |v: &[Value], present: bool| {
        let s = serde_json::to_string(v).unwrap();
        assert_eq!(s.contains("consistent_after"), present, "{s}");
        serde_json::to_string(&drop_key(&json!(v), "consistent_after")).unwrap()
    };
    let a = strip(&aa, true);
    let b = strip(&ab, false);
    assert!(a.contains("succeeded") && b.contains("failed"), "{a}\n{b}");
    assert_eq!(
        a.replace("succeeded", "<o>"),
        b.replace("failed", "<o>"),
        "after approval only succeeded/failed may differ"
    );
}

// ---------------------------------------------------------------- T-M13

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
                .header(header::ORIGIN, "https://spa.test")
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

fn strip_request_id(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "request_id")
                .map(|(k, x)| (k.clone(), strip_request_id(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_request_id).collect()),
        _ => v.clone(),
    }
}

#[tokio::test]
async fn rest_and_mcp_errors_match() {
    let pool = test_pool().await;
    let (w, mut m, _, _, _) = setup(&pool).await;
    let r = router(&w);
    let b = login(&r, "alice").await;
    // B13: the same not_found from REST (a person) and MCP (the agent).
    let resp = r
        .clone()
        .oneshot(
            Request::get("/api/v1/issues/WEB-999")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={}", b.session))
                .header("x-csrf-token", &b.csrf)
                .header(header::ORIGIN, "https://spa.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let rest: Value = serde_json::from_slice(&bytes).unwrap();
    let mcp = m.call("get", json!({"key": "WEB-999"})).await.unwrap_err();
    let mcp_err = if mcp.get("error").is_some() {
        mcp["error"].clone()
    } else {
        mcp
    };
    assert_eq!(
        strip_request_id(&rest["error"]),
        strip_request_id(&mcp_err),
        "REST and MCP error objects differ"
    );
}

// ---------------------------------------------------------------- §8.9 quotas

#[tokio::test]
async fn proposal_quotas() {
    let pool = test_pool().await;
    let (w, mut m, _d_web, _, agent) = setup(&pool).await;
    let (i, _) = mk(&w, w.web, "quota", "", false).await;

    // Payload: over 768 KiB as sent, and over 512 KiB normalized.
    let big = "x".repeat(800 * 1024);
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(),
                                          "proposal": note_doc(i, 0, &big)}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "invalid_request", "{}", &e.to_string()[..200]);
    assert_eq!(e["details"]["reason"], "payload_too_large");
    assert_eq!(e["details"]["limit_bytes"], 768 * 1024);
    let mid = "y".repeat(600 * 1024);
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(),
                                          "proposal": note_doc(i, 0, &mid)}),
        )
        .await
        .unwrap_err();
    // A note that big is refused by one of the size rules, never stored.
    assert!(e["code"].as_str().is_some(), "{e}");
    // The normalized limit on its own (a note cannot reach it under 768 KiB).
    assert!(oxledger::quota::check_normalized(512 * 1024).is_ok());
    let e = oxledger::quota::check_normalized(512 * 1024 + 1).unwrap_err();
    assert_eq!(code(&e).as_str(), "invalid_request");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changesets").await,
        0,
        "nothing stored"
    );

    // Two proposal writes at a time; the third is rate_limited with retry_after_ms.
    let g1 = Arc::new(Gate::default());
    let g2 = Arc::new(Gate::default());
    faults::arm("agent_write.after_slot", Action::Block(g1.clone()));
    faults::arm("agent_write.after_slot", Action::Block(g2.clone()));
    let spawn = |n: &'static str| {
        let app = w.app.clone();
        let doc = note_doc(i, 0, n);
        tokio::spawn(
            async move { changesets::propose(&app, agent, Uuid::now_v7(), &doc, ORIGIN).await },
        )
    };
    let h1 = spawn("one");
    g1.reached.notified().await;
    let h2 = spawn("two");
    g2.reached.notified().await;
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(),
                                          "proposal": note_doc(i, 0, "three")}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "rate_limited", "{e}");
    assert!(
        e["retry_after_ms"].as_u64().is_some() || e["details"]["retry_after_ms"].as_u64().is_some(),
        "{e}"
    );
    g1.release.notify_one();
    g2.release.notify_one();
    common::join("the first request", h1).await.unwrap();
    common::join("the second request", h2).await.unwrap();
    // A slot is free again.
    m.call(
        "changeset_propose",
        json!({"operation_id": Uuid::now_v7(),
                                       "proposal": note_doc(i, 0, "four")}),
    )
    .await
    .unwrap();
    faults::clear();
}

#[tokio::test]
async fn validate_quota() {
    let pool = test_pool().await;
    let (w, mut m, d_web, _, _) = setup(&pool).await;
    let (i, _) = mk(&w, w.web, "validate", "", false).await;
    let p = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(),
                                          "proposal": note_doc(i, 0, "v")}),
        )
        .await
        .unwrap();
    let cs = p["changeset_id"].clone();
    let (agent, ctx): (Uuid, Uuid) = sqlx::query_as(
        "SELECT agent_principal_id, context_id FROM agent_write_log WHERE delegation_id = $1 LIMIT 1",
    )
    .bind(d_web)
    .fetch_one(&pool)
    .await
    .unwrap();
    let log = |n: i32, ago_secs: f64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO agent_write_log (agent_principal_id, context_id, delegation_id, kind, at)
                 SELECT $1, $2, $3, 'validate', clock_timestamp() - make_interval(secs => $5)
                 FROM generate_series(1, $4)",
            )
            .bind(agent)
            .bind(ctx)
            .bind(d_web)
            .bind(n)
            .bind(ago_secs)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    // Older than an hour does not count.
    log(200, 3700.0).await;
    m.call(
        "changeset_validate",
        json!({"changeset_id": cs, "version": 1}),
    )
    .await
    .unwrap();
    // 1 + 118 this hour; the 120th passes, the 121st does not.
    log(118, 10.0).await;
    m.call(
        "changeset_validate",
        json!({"changeset_id": cs, "version": 1}),
    )
    .await
    .unwrap();
    let e = m
        .call(
            "changeset_validate",
            json!({"changeset_id": cs, "version": 1}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "quota_exceeded", "{e}");
    assert_eq!(e["details"]["kind"], "validates_per_hour", "{e}");
    // A replayed proposal key does not count against the proposal quota.
    let before = count(
        &pool,
        "SELECT count(*) FROM agent_write_log WHERE kind = 'propose'",
    )
    .await;
    let k = Uuid::now_v7();
    let doc = note_doc(i, 0, "replay");
    for _ in 0..3 {
        m.call(
            "changeset_propose",
            json!({"operation_id": k, "proposal": doc.clone()}),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM agent_write_log WHERE kind = 'propose'"
        )
        .await,
        before + 1
    );
}
