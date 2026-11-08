// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 S6.4/S6.5: the agent's MCP tools over the real `/mcp` route
//! (Streamable HTTP): the tool list is the registry's allowlist (I-5), the
//! credential method is enforced per tool, describe scope context (§5.3),
//! proposals with groups, changeset_get / list / validate / operation_get
//! through the response filter (owner revisions hide their content and hash,
//! §8.8), cancel, command_submit and version_list; unknown authority
//! parameters are refused. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::api::{self, ApiConfig};
use oxledger::auth::{AuthMethod, SessionPolicy};
use oxledger::changesets::{self, faults};
use oxledger::mcp;
use oxledger::registry;
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

use world::mcp_client::Mcp;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

#[tokio::test]
async fn s6_mcp() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    grant(&pool, w.dev, &["manage_versions"]).await;
    let i1 = w.issue(w.web, "login loops").await;
    let ctx = w.new_context().await;
    let d = agents_delegate(&w).await;
    let (token, _agent) = w.token(ctx).await;
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: "https://spa.test".into(),
            session: SessionPolicy::default(),
        },
    );
    let mut m = Mcp::connect(router, token).await;

    // I-5: the tool list is exactly the registry's MCP allowlist; nothing approves or applies.
    let listed = m.rpc("tools/list", json!({})).await;
    let mut names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| s(&t["name"]).to_string())
        .collect();
    names.sort();
    let mut want: Vec<String> = registry::mcp_tools()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    want.sort();
    assert_eq!(names, want);
    for n in &names {
        assert!(
            !n.contains("approve") && !n.contains("apply") && !n.contains("reject"),
            "{n}"
        );
    }
    for n in [
        "command_submit",
        "changeset_propose",
        "changeset_get",
        "version_list",
    ] {
        assert!(names.iter().any(|x| x == n), "{n} missing");
    }
    // A4: a legacy key keeps the four read tools only.
    for t in ["search", "get", "resolve", "describe"] {
        assert!(
            mcp::check_method(t, AuthMethod::LegacyApiKey).is_ok(),
            "{t}"
        );
    }
    for t in registry::mcp_tools().iter().map(|t| t.name) {
        if ["search", "get", "resolve", "describe"].contains(&t) {
            continue;
        }
        assert!(
            mcp::check_method(t, AuthMethod::LegacyApiKey).is_err(),
            "{t}"
        );
    }

    // §5.3 describe context.
    let c = m
        .call("describe", json!({"scope": "context"}))
        .await
        .unwrap();
    assert_eq!(c["context"]["context_id"], json!(ctx));
    assert_eq!(c["context"]["projects"][0]["project"], "WEB");
    assert_eq!(c["context"]["projects"][0]["delegation"], "active");
    assert_eq!(
        c["context"]["projects"][0]["publish"]["public_content"],
        "same_or_narrower"
    );
    let e = m
        .call("describe", json!({"scope": "context", "project": "WEB"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], "invalid_argument", "{e}");

    // Proposals with a group label: the same label joins the same group.
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let note = |body: &str| {
        json!({"command_schema_version": 1, "title": "n",
               "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i1), "body": body}],
               "preconditions": [{"target": tgt(i1), "lock_version": lv}]})
    };
    let op1 = Uuid::now_v7();
    let p1 = m
        .call(
            "changeset_propose",
            json!({"operation_id": op1, "proposal": note("a"), "group_label": "Safari 登入",
                   "include_handoff_text": true, "response": "full"}),
        )
        .await
        .unwrap();
    assert!(s(&p1["review_url"]).starts_with("https://spa.test/"));
    assert!(s(&p1["handoff_text"]).contains(s(&p1["review_url"])));
    assert_eq!(p1["full"]["proposal"]["title"], "n");
    let g = p1["group_id"].clone();
    assert!(g.is_string());
    let p2 = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("b"), "group_label": "Safari 登入"}),
        )
        .await
        .unwrap();
    assert_eq!(p2["group_id"], g);
    let p3 = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("c"), "group_id": g}),
        )
        .await
        .unwrap();
    assert_eq!(p3["group_id"], g);
    // A group of another project, or one older than 24 hours, is refused;
    // the label then starts a new group.
    let gid: Uuid = serde_json::from_value(g.clone()).unwrap();
    let other = id(
        &pool,
        &format!(
            "INSERT INTO changeset_groups (delegation_id, project_id, security_epoch, label)
             SELECT delegation_id, '{}', security_epoch, 'x' FROM changeset_groups WHERE id = '{gid}'
             RETURNING id",
            w.auth_p
        ),
    )
    .await;
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("o"), "group_id": other}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "invalid_argument", "{e}");
    exec(
        &pool,
        &format!("UPDATE changeset_groups SET created_at = now() - interval '25 hours' WHERE id = '{gid}'"),
    )
    .await;
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("old"), "group_id": g}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "invalid_argument", "{e}");
    let fresh = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("new"), "group_label": "Safari 登入"}),
        )
        .await
        .unwrap();
    assert_ne!(fresh["group_id"], g);
    exec(
        &pool,
        &format!("UPDATE changeset_groups SET created_at = now() WHERE id = '{gid}'"),
    )
    .await;
    for bad in [json!("a\nb"), json!("x".repeat(81)), json!("a\u{202E}b")] {
        let e = m
            .call(
                "changeset_propose",
                json!({"operation_id": Uuid::now_v7(), "proposal": note("d"), "group_label": bad}),
            )
            .await
            .unwrap_err();
        assert_eq!(e["code"], "invalid_argument", "{e}");
    }
    let e = m
        .call(
            "changeset_propose",
            json!({"operation_id": Uuid::now_v7(), "proposal": note("d"), "group_id": Uuid::now_v7()}),
        )
        .await
        .unwrap_err();
    assert_eq!(e["code"], "invalid_argument");
    // Authority parameters do not exist (§8.8).
    for extra in ["approved", "actor", "origin", "on_behalf_of", "author_id"] {
        let mut a = json!({"operation_id": Uuid::now_v7(), "proposal": note("e")});
        a[extra] = json!(true);
        assert!(m.call("changeset_propose", a).await.is_err(), "{extra}");
    }

    // changeset_list by group, changeset_get, validate, operation_get.
    let l = m
        .call("changeset_list", json!({"group_id": g}))
        .await
        .unwrap();
    assert_eq!(l["items"].as_array().unwrap().len(), 3);
    let cs1 = p1["changeset_id"].clone();
    let got = m
        .call("changeset_get", json!({"changeset_id": cs1}))
        .await
        .unwrap();
    assert_eq!(got["state"], "proposed");
    assert_eq!(got["proposal"]["ops"][0]["body"], "a");
    assert!(got["proposal_hash"].is_string());
    assert_eq!(got["requires_human_approval"], true);
    assert_eq!(got["group_label"], "Safari 登入");
    let v = m
        .call(
            "changeset_validate",
            json!({"changeset_id": cs1, "version": 1}),
        )
        .await
        .unwrap();
    assert_eq!(v["validation"]["status"], "passed", "{v}");
    let o = m
        .call("operation_get", json!({"operation_id": op1}))
        .await
        .unwrap();
    assert_eq!(o["changeset"]["changeset_id"], cs1);

    // The owner revises: the agent sees neither the content nor the hash.
    let csid: Uuid = serde_json::from_value(cs1.clone()).unwrap();
    changesets::owner_revise(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        csid,
        &json!({"expected_current_version": 1, "proposal": note("secret owner text")}),
    )
    .await
    .unwrap();
    let got = m
        .call("changeset_get", json!({"changeset_id": cs1}))
        .await
        .unwrap();
    assert_eq!(got["version"], 2);
    assert_eq!(got["content_unavailable"], "owner_revised");
    assert!(got.get("proposal").is_none() && got.get("proposal_hash").is_none());
    assert!(got.get("validation").is_none());
    assert!(!got.to_string().contains("secret owner text"));
    // The targets come from the hidden body too (§8.8).
    assert_eq!(got["targets"], json!([]), "{got}");
    let l = m
        .call("changeset_list", json!({"group_id": g}))
        .await
        .unwrap();
    let row = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["changeset_id"] == cs1)
        .unwrap()
        .clone();
    assert_eq!(row["targets"], json!([]), "{l}");
    let v = m
        .call(
            "changeset_validate",
            json!({"changeset_id": cs1, "version": 2}),
        )
        .await
        .unwrap();
    assert_eq!(v["content_unavailable"], "owner_revised");
    assert!(v.get("validation").is_none());
    // The human applies the owner's version: the agent sees only succeeded/failed.
    let ov = serde_json::to_value(
        changesets::get(&w.app, w.alice_ctx, csid, None)
            .await
            .unwrap(),
    )
    .unwrap();
    let o = changesets::approve_and_apply(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        csid,
        &changesets::ApproveInput {
            version: 2,
            proposal_hash: s(&ov["proposal_hash"]).to_string(),
            review_mode: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(o.execution.unwrap().status, "succeeded");
    let got = m
        .call("changeset_get", json!({"changeset_id": cs1}))
        .await
        .unwrap();
    assert_eq!(got["last_execution"]["status"], "succeeded", "{got}");
    assert!(got["last_execution"].get("items").is_none(), "{got}");
    assert!(!got.to_string().contains("error_code"));

    // Cancel one of them; it counts toward the per-delegation quota (§8.9).
    let before = count(
        &pool,
        "SELECT count(*) FROM agent_write_log WHERE kind = 'propose'",
    )
    .await;
    let cs2 = p2["changeset_id"].clone();
    let st = m
        .call(
            "changeset_cancel",
            json!({"operation_id": Uuid::now_v7(), "changeset_id": cs2}),
        )
        .await
        .unwrap();
    assert_eq!(st["state"], "canceled", "{st}");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM agent_write_log WHERE kind = 'propose'"
        )
        .await,
        before + 1
    );
    let l = m
        .call("changeset_list", json!({"state": "canceled"}))
        .await
        .unwrap();
    assert_eq!(l["items"].as_array().unwrap().len(), 1);

    // command_submit directly, then operation_get of it.
    let op2 = Uuid::now_v7();
    let r = m
        .call(
            "command_submit",
            // The owner's applied version moved lock_version to 1.
            json!({"operation_id": op2, "command": world::note_doc(i1, 1, "direct")}),
        )
        .await
        .unwrap();
    assert_eq!(r["outcome"], "applied", "{r}");
    let o = m
        .call("operation_get", json!({"operation_id": op2}))
        .await
        .unwrap();
    assert_eq!(o["execution_id"], r["execution_id"]);
    assert_eq!(o["effects"][0]["issue_id"], json!(i1));
    // A version through command_submit, then version_list by name.
    let r = m
        .call(
            "command_submit",
            json!({"operation_id": Uuid::now_v7(), "command": {"command_schema_version": 1, "title": "v",
                   "ops": [{"op_id": op(), "kind": "create_version", "project_id": w.web,
                            "fields": {"name": "v2.1", "effective_date": "2099-01-01"}}]}}),
        )
        .await
        .unwrap();
    let vid = r["versions"][0]["version_id"].clone();
    let vl = m
        .call(
            "version_list",
            json!({"project": "WEB", "name": "V2.1", "next": true}),
        )
        .await
        .unwrap();
    assert_eq!(vl["versions"][0]["id"], vid);
    assert_eq!(vl["next"]["outcome"], "resolved", "{vl}");
    let vl = m
        .call("version_list", json!({"project": "WEB", "name": "nope"}))
        .await
        .unwrap();
    assert_eq!(vl["versions"], json!([]));

    // An idempotent replay re-authenticates first: a target no longer
    // readable answers not_found, nothing of the stored proposal (§8.8; the
    // replay view filters as well, defence in depth).
    let i2 = w.issue(w.web, "soon archived").await;
    let rop = Uuid::now_v7();
    let args = json!({"operation_id": rop, "proposal": {"command_schema_version": 1, "title": "n",
        "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i2), "body": "x"}],
        "preconditions": [{"target": tgt(i2), "lock_version": 0}]}});
    let first = m.call("changeset_propose", args.clone()).await.unwrap();
    assert_eq!(first["validation"]["status"], "passed", "{first}");
    let again = m.call("changeset_propose", args.clone()).await.unwrap();
    assert_eq!(again["changeset_id"], first["changeset_id"]);
    assert_eq!(again["validation"]["status"], "passed", "{again}");
    exec(
        &pool,
        &format!("UPDATE issues SET archived_at = now() WHERE id = '{i2}'"),
    )
    .await;
    let e = m.call("changeset_propose", args).await.unwrap_err();
    assert_eq!(e["code"], "not_found", "{e}");
    assert!(!e.to_string().contains(s(&first["changeset_id"])), "{e}");

    // The ceiling and the policy rows each gate a command (§16.3 rule 1).
    let ver_doc = |name: &str| {
        json!({"operation_id": Uuid::now_v7(), "command": {"command_schema_version": 1, "title": "v",
               "ops": [{"op_id": op(), "kind": "create_version", "project_id": w.web,
                        "fields": {"name": name}}]}})
    };
    exec(
        &pool,
        &format!("UPDATE delegations SET proposal_ops = array_remove(proposal_ops, 'create_version') WHERE id = '{d}'"),
    )
    .await;
    let e = m.call("command_submit", ver_doc("v9")).await.unwrap_err();
    assert_eq!(e["code"], "forbidden_op", "{e}");
    exec(
        &pool,
        &format!("UPDATE delegations SET proposal_ops = proposal_ops || ARRAY['create_version'] WHERE id = '{d}'"),
    )
    .await;
    exec(
        &pool,
        &format!("DELETE FROM delegation_command_policies WHERE delegation_id = '{d}' AND command_kind = 'create_version'"),
    )
    .await;
    let e = m.call("command_submit", ver_doc("v9")).await.unwrap_err();
    assert_eq!(e["code"], "forbidden_op", "{e}");

    // A revoked delegation: the agent's proposals are no longer described.
    agents::revoke_delegation(&pool, &w.alice_ctx, d)
        .await
        .unwrap();
    let e = m
        .call("changeset_get", json!({"changeset_id": p3["changeset_id"]}))
        .await
        .unwrap_err();
    assert!(
        e["code"] == "delegation_unavailable" || e["code"] == "context_unavailable",
        "{e}"
    );
    // Not listed, also by group; operation_get of a direct write refuses too.
    match m.call("changeset_list", json!({"group_id": g})).await {
        Ok(l) => assert_eq!(l["items"], json!([]), "{l}"),
        Err(e) => assert_eq!(e["code"], "context_unavailable", "{e}"),
    }
    let e = m
        .call("operation_get", json!({"operation_id": op2}))
        .await
        .unwrap_err();
    assert!(
        e["code"] == "delegation_unavailable" || e["code"] == "context_unavailable",
        "{e}"
    );
    faults::clear();
}

use oxledger::agents;

async fn agents_delegate(w: &World) -> Uuid {
    agents::create_delegation(
        &w.pool,
        &w.policy,
        &w.alice_ctx,
        agents::DelegationInput {
            agent_client_id: w.client,
            project_id: w.web,
            proposal_ops: ["add_note", "create_version", "update_version"]
                .map(String::from)
                .to_vec(),
            writable_fields: vec![],
            max_ops: None,
            expires_in_days: Some(30),
            policies: None,
        },
    )
    .await
    .unwrap()
    .id
}
