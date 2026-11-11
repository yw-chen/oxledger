// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 05 M1.10 (§2.12, §3.13, §7.10, §7.10a): the built-in AI chat —
//! everything that does not need to reach a model: defaults, the admin
//! API (D17, endpoints, keys, enable blockers), conversations, visibility
//! to others, deletion and retention, D12, new context with carried
//! delegations, extension, rate and budget refusals, Q4, the fixed-list
//! CHECKs and the 0030 upgrade.
//! T-M1-101, 106, 107, 108, 109 (refusals), 112, 113, 114, 115, 116, 119
//! (registration), 125 (ai_conversations), 131, 134. The runs themselves
//! are in m1_10_ai_run.rs (feature `ai`). Needs PostgreSQL 18.

#[path = "ai_world/mod.rs"]
mod ai_world;
mod common;
#[path = "s6_world/mod.rs"]
mod world;

use ai_world::*;
use common::test_pool;
use oxledger::ai::admin;
use oxledger::ai::chat::{self, PageRef, SendInput};
use oxledger::ai::runner;
use oxledger::ai_team::{self, RuntimeNewContextInput};
use oxledger::error::{ErrorCode, classify};
use oxledger::policy::PolicyMode::{Direct, Review};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;
use world::*;

fn details(e: &anyhow::Error) -> Value {
    classify(e).and_then(|a| a.details).unwrap_or(Value::Null)
}

fn vars_https() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "OXLEDGER_AI_ENDPOINT_ANTHROPIC",
            "https://api.anthropic.test",
        ),
        ("OXLEDGER_AI_KEY_ANTHROPIC", KEY),
        ("OXLEDGER_AI_ENDPOINT_VLLM", "https://llm.intra.test/v1"),
        ("OXLEDGER_AI_ENDPOINT_NOKEY", "https://nokey.test"),
    ]
}

fn dns() -> Arc<Dns> {
    let d = Arc::new(Dns::default());
    d.set("api.anthropic.test", &[&["203.0.113.10"]]);
    d.set("llm.intra.test", &[&["10.0.0.5"]]);
    d.set("nokey.test", &[&["203.0.113.11"]]);
    d
}

/// The AI on with an Anthropic provider (tested by hand).
async fn on(w: &World) -> (AuthContext, Uuid) {
    install(&vars_https(), dns());
    let root = admin_ctx(w).await;
    let p = provider(
        w,
        root,
        &new_provider("Anthropic", "anthropic", "ANTHROPIC", None),
    )
    .await;
    mark_tested(w, p.id, true).await;
    enable(w, root, p.id).await;
    (root, p.id)
}

use oxledger::auth::AuthContext;

// ---------------------------------------------------------------- T-M1-115

#[tokio::test]
async fn t_m1_115_defaults_and_no_delegation() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    install(&vars_https(), dns());
    // A fresh installation: off, no tab, the built-in client disabled.
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    assert!(!s.enabled && s.provider.is_none() && s.assistant.is_none());
    assert_eq!(s.limits.runs_left_this_hour, 30);
    assert_eq!(s.limits.budget_left_today_usd, "2.000000");
    let enabled: bool = sqlx::query_scalar(
        "SELECT enabled FROM agent_clients WHERE name = 'oxledger-ai' AND builtin",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!enabled);
    let e = chat::setup(&w.app, w.alice_ctx).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::AiUnavailable);
    assert_eq!(details(&e)["kind"], "disabled");
    let root = admin_ctx(&w).await;
    let st = settings(&w, root).await;
    assert!(!st.enabled);
    assert_eq!(
        st.enable_blockers,
        vec!["no_provider".to_string(), "no_budget".into()]
    );
    // Enabling with blockers is refused, with the reasons.
    let e = put(&w, root, |i| i.enabled = true).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(details(&e)["kind"], "enable_blocked");

    // A provider not tested yet blocks; a failed test says so.
    let p = provider(
        &w,
        root,
        &new_provider("Anthropic", "anthropic", "ANTHROPIC", None),
    )
    .await;
    assert_eq!(p.model, "claude-sonnet-5-5");
    let e = put(&w, root, |i| {
        i.enabled = true;
        i.active_provider_id = Some(p.id);
        i.monthly_budget_usd = Some("100.00".into());
    })
    .await
    .unwrap_err();
    assert_eq!(
        details(&e)["enable_blockers"],
        json!(["not_tested"]),
        "{e:#}"
    );
    exec(
        &pool,
        &format!(
            "UPDATE ai_providers SET last_test_at = now() + interval '1 second' WHERE id = '{}'",
            p.id
        ),
    )
    .await;
    let e = put(&w, root, |i| {
        i.enabled = true;
        i.active_provider_id = Some(p.id);
        i.monthly_budget_usd = Some("100.00".into());
    })
    .await
    .unwrap_err();
    assert_eq!(details(&e)["enable_blockers"], json!(["test_failed"]));
    mark_tested(&w, p.id, true).await;
    let st = enable(&w, root, p.id).await;
    assert!(st.enabled && st.enable_blockers.is_empty());
    let enabled: bool = sqlx::query_scalar(
        "SELECT enabled FROM agent_clients WHERE name = 'oxledger-ai' AND builtin",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(enabled, "the built-in client follows ai_settings.enabled");

    // No delegation: the assistant exists but has no projects (chat only;
    // the tool list is empty, checked in m1_10_ai_run.rs).
    let s = assistant(&w, w.alice_ctx).await;
    let a = s.assistant.unwrap();
    assert_eq!(a.handle, "oxledger-ai@alice");
    assert_eq!(a.delegated_projects, 0);
    assert_eq!(a.context_state, "active");
    assert_eq!(s.provider.unwrap().model, "claude-sonnet-5-5");
    // The built-in runtime can never hold a token (M1.9, FK).
    let rt: Uuid = id(
        &pool,
        &format!(
            "SELECT id FROM agent_runtimes WHERE sponsor_user_id = '{}' AND kind = 'builtin'",
            w.alice
        ),
    )
    .await;
    assert_eq!(rt, a.runtime_id);

    // Turning off again (step-up) flips the client back.
    put(&w, root, |i| i.enabled = false).await.unwrap();
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    assert!(!s.enabled);
    let c = chat::create(&w.app, w.alice_ctx, &chat::NewConversation::default())
        .await
        .unwrap_err();
    assert_eq!(code(&c), ErrorCode::AiUnavailable);
}

// ---------------------------------------------------------------- T-M1-112, 113, 114, 119 (registration)

#[tokio::test]
async fn t_m1_112_113_admin_providers_and_keys() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let mut vars = vars_https();
    vars.push((
        "OXLEDGER_AI_ENDPOINT_DEEPSEEK",
        "https://api.deepseek.com/v1",
    ));
    vars.push((
        "OXLEDGER_AI_ENDPOINT_QWEN",
        "https://dashscope.aliyuncs.com",
    ));
    vars.push(("OXLEDGER_AI_ENDPOINT_bad", "https://x.test"));
    vars.push(("OXLEDGER_AI_ENDPOINT_USERINFO", "https://u:p@x.test"));
    vars.push(("OXLEDGER_AI_ENDPOINT_QUERY", "https://x.test/v1?key=1"));
    vars.push(("OXLEDGER_AI_ENDPOINT_FRAG", "https://x.test/v1#a"));
    vars.push(("OXLEDGER_AI_ENDPOINT_FTP", "ftp://x.test"));
    let (cfg, warnings) = install(&vars, dns());
    let names: BTreeSet<&str> = cfg.endpoints.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["ANTHROPIC", "NOKEY", "VLLM"].into_iter().collect(),
        "D17 hosts and bad URLs are not registered"
    );
    assert_eq!(warnings.len(), 7, "{warnings:?}");
    for wn in &warnings {
        assert!(!wn.contains("u:p@") && !wn.contains("key=1"), "{wn}");
    }
    assert!(!format!("{cfg:?}").contains(KEY));

    let root = admin_ctx(&w).await;
    // Not for other people.
    let e = admin::providers(&w.app, w.alice_ctx).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = admin::endpoints(&w.app, w.alice_ctx).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    let ep = admin::endpoints(&w.app, root).await.unwrap();
    let anth = ep.items.iter().find(|e| e.name == "ANTHROPIC").unwrap();
    assert!(anth.key_configured);
    assert_eq!(anth.origin, "https://api.anthropic.test");
    assert_eq!(serde_json::to_value(anth.address_class).unwrap(), "public");
    let vllm = ep.items.iter().find(|e| e.name == "VLLM").unwrap();
    assert!(!vllm.key_configured);
    assert_eq!(serde_json::to_value(vllm.address_class).unwrap(), "private");

    // T-M1-112: the API takes no URL, key or variable name.
    for extra in [
        json!({"base_url": "https://attacker.test"}),
        json!({"api_key_env": "DATABASE_URL"}),
        json!({"api_key": "x"}),
        json!({"url": "https://attacker.test"}),
    ] {
        let mut v =
            serde_json::to_value(new_provider("X", "anthropic", "ANTHROPIC", None)).unwrap();
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(serde_json::from_value::<admin::NewProvider>(v).is_err());
    }
    let e = admin::create_provider(
        &w.app,
        &w.policy,
        root,
        &new_provider("X", "openai_compat", "DATABASE_URL", Some("gpt-x")),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AiEndpointInvalid);
    assert_eq!(details(&e)["kind"], "unregistered");
    let e = admin::create_provider(
        &w.app,
        &w.policy,
        root,
        &new_provider("X", "anthropic", "NOKEY", None),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["kind"], "no_key");
    // An OpenAI-compatible endpoint without a key is fine; the model is required.
    let e = admin::create_provider(
        &w.app,
        &w.policy,
        root,
        &new_provider("X", "openai_compat", "VLLM", None),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    // T-M1-113: D17 names; the confirmation is required.
    for m in [
        "Qwen2.5-72B-Instruct",
        "deepseek-chat",
        "glm-4",
        "moonshot-v1",
    ] {
        let e = admin::create_provider(
            &w.app,
            &w.policy,
            root,
            &new_provider("X", "openai_compat", "VLLM", Some(m)),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::AiModelDisallowed, "{m}");
        assert_eq!(code(&e).http_status(), 422);
    }
    let mut np = new_provider("X", "openai_compat", "VLLM", Some("llama-3.3-70b"));
    np.d17_attested = false;
    let e = admin::create_provider(&w.app, &w.policy, root, &np)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["path"], "d17_attested");
    // P-6: an external service needs the organisation-account confirmation;
    // an internal one does not.
    let mut np = new_provider("A", "anthropic", "ANTHROPIC", None);
    np.org_account_attested = false;
    let e = admin::create_provider(&w.app, &w.policy, root, &np)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["path"], "org_account_attested");
    let mut np = new_provider("Local", "openai_compat", "VLLM", Some("llama-3.3-70b"));
    np.org_account_attested = false;
    let local = provider(&w, root, &np).await;
    assert!(local.org_account_attested_at.is_none());
    let anth = provider(&w, root, &new_provider("A", "anthropic", "ANTHROPIC", None)).await;
    // A change of model needs a new test (config_changed_at moves).
    mark_tested(&w, anth.id, true).await;
    exec(&pool, &format!(
        "UPDATE ai_providers SET config_changed_at = now() - interval '2 minutes',
                last_test_at = now() - interval '1 minute', last_test_ok_at = now() - interval '1 minute' WHERE id = '{}'",
        anth.id
    )).await;
    let before = anth.config_changed_at;
    let after = admin::update_provider(
        &w.app,
        &w.policy,
        root,
        anth.id,
        &admin::ProviderPatch {
            lock_version: anth.lock_version,
            name: None,
            endpoint_name: None,
            model: Some("claude-opus-5-5".into()),
            max_output_tokens: None,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
            d17_attested: true,
            org_account_attested: false,
        },
    )
    .await
    .unwrap();
    assert!(after.config_changed_at > before);
    let e = put(&w, root, |i| {
        i.enabled = true;
        i.active_provider_id = Some(anth.id);
        i.monthly_budget_usd = Some("10".into());
    })
    .await
    .unwrap_err();
    assert_eq!(details(&e)["enable_blockers"], json!(["not_tested"]));
    // A stale lock_version is a conflict.
    let e = admin::update_provider(
        &w.app,
        &w.policy,
        root,
        anth.id,
        &admin::ProviderPatch {
            lock_version: anth.lock_version,
            name: Some("B".into()),
            endpoint_name: None,
            model: None,
            max_output_tokens: None,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
            d17_attested: true,
            org_account_attested: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict);
    // Every change is audited; the key is nowhere.
    let ev = admin::events(&w.app, root).await.unwrap();
    assert!(ev.items.iter().any(|e| e.kind == "created"));
    assert!(ev.items.iter().any(|e| e.kind == "updated"));
    let dump = admin_dump(&w, root).await;
    assert!(!dump.contains(KEY), "the key leaked");
    assert!(!dump.contains("sk-test"), "part of the key leaked");
}

#[tokio::test]
async fn t_m1_114_119_endpoint_addresses() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let vars = [
        ("OXLEDGER_AI_ENDPOINT_HTTPS_PRIV", "https://priv.test"),
        ("OXLEDGER_AI_ENDPOINT_HTTPS_LOOP", "https://loop.test"),
        (
            "OXLEDGER_AI_ENDPOINT_HTTP_PRIV",
            "http://ollama.test:11434/v1",
        ),
        ("OXLEDGER_AI_ENDPOINT_HTTP_PUB", "http://pub.test/v1"),
        ("OXLEDGER_AI_ENDPOINT_META", "https://meta.test"),
        ("OXLEDGER_AI_ENDPOINT_LINK", "https://link.test"),
        ("OXLEDGER_AI_ENDPOINT_ZERO", "https://zero.test"),
        ("OXLEDGER_AI_ENDPOINT_MAPPED", "http://mapped.test"),
        ("OXLEDGER_AI_ENDPOINT_MIXED", "https://mixed.test"),
    ];
    let d = Arc::new(Dns::default());
    d.set("priv.test", &[&["10.0.0.5"]]);
    d.set("loop.test", &[&["127.0.0.1"]]);
    d.set("ollama.test", &[&["10.0.0.5"]]);
    d.set("pub.test", &[&["203.0.113.7"]]);
    d.set("meta.test", &[&["169.254.169.254"]]);
    d.set("link.test", &[&["fe80::1"]]);
    d.set("zero.test", &[&["0.0.0.0"]]);
    d.set("mapped.test", &[&["::ffff:10.0.0.1"]]);
    d.set("mixed.test", &[&["203.0.113.8", "169.254.169.254"]]);
    let root = admin_ctx(&w).await;
    for allow in [false, true] {
        let mut v: Vec<(&str, &str)> = vars.to_vec();
        if allow {
            v.push(("OXLEDGER_AI_ALLOW_HTTP", "1"));
        }
        install(&v, d.clone());
        exec(
            &pool,
            "DELETE FROM ai_provider_events; DELETE FROM ai_providers",
        )
        .await;
        let try_ep = |name: &'static str| {
            let w = &w;
            async move {
                let mut p =
                    new_provider(&format!("{name}-{allow}"), "openai_compat", name, Some("m"));
                p.org_account_attested = true;
                admin::create_provider(&w.app, &w.policy, root, &p).await
            }
        };
        // https to private and loopback: always.
        try_ep("HTTPS_PRIV").await.unwrap();
        try_ep("HTTPS_LOOP").await.unwrap();
        // http to private: only with the switch; IPv4-mapped as IPv4.
        for name in ["HTTP_PRIV", "MAPPED"] {
            let r = try_ep(name).await;
            if allow {
                r.unwrap();
            } else {
                let e = r.unwrap_err();
                assert_eq!(code(&e), ErrorCode::AiEndpointInvalid);
                assert_eq!(details(&e)["kind"], "endpoint_blocked");
            }
        }
        // http to a public address, metadata, link-local, unspecified, and a
        // name with one bad address among good ones: never.
        for name in ["HTTP_PUB", "META", "LINK", "ZERO", "MIXED"] {
            let e = try_ep(name).await.unwrap_err();
            assert_eq!(
                details(&e)["kind"],
                "endpoint_blocked",
                "{name} allow={allow}"
            );
        }
    }
}

// ---------------------------------------------------------------- conversations, T-M1-108

#[tokio::test]
async fn t_m1_108_conversations_are_private() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let (root, _) = on(&w).await;
    assistant(&w, w.alice_ctx).await;
    let c = conversation(&w, w.alice_ctx, "我的對話").await;
    let l = chat::list(&w.app, w.alice_ctx, &Default::default())
        .await
        .unwrap();
    assert_eq!(l.items.len(), 1);
    assert_eq!(l.items[0].title, "我的對話");
    assert_eq!(l.items[0].state, "open");
    // A run of alice's (stored directly: no model here).
    let run = id(
        &pool,
        &format!(
            "INSERT INTO ai_runs (conversation_id, user_id, provider_id, model, prompt_version, state, finished_at)
             SELECT '{c}', '{}', active_provider_id, 'm', 1, 'succeeded', now() FROM ai_settings RETURNING id",
            w.alice
        ),
    )
    .await;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    for other in [bob, root] {
        assert!(
            chat::list(&w.app, other, &Default::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        for e in [
            chat::messages(&w.app, other, c).await.err().unwrap(),
            chat::delete(&w.app, other, c).await.err().unwrap(),
            runner::get_run(&w.app, other, run).await.err().unwrap(),
            runner::cancel(&w.app, other, run).await.err().unwrap(),
            runner::stream(&w.app, other, run, None)
                .await
                .err()
                .unwrap(),
            send(&w, other, c, "hi", vec![], None).await.err().unwrap(),
        ] {
            assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
        }
    }
    // Titles: one line, ≤ 120 characters.
    let e = chat::create(
        &w.app,
        w.alice_ctx,
        &chat::NewConversation {
            title: Some("x".repeat(121)),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    // Paging.
    for i in 0..3 {
        conversation(&w, w.alice_ctx, &format!("c{i}")).await;
    }
    let p1 = chat::list(
        &w.app,
        w.alice_ctx,
        &chat::ConversationsQuery {
            cursor: None,
            limit: Some(3),
        },
    )
    .await
    .unwrap();
    assert_eq!(p1.items.len(), 3);
    let p2 = chat::list(
        &w.app,
        w.alice_ctx,
        &chat::ConversationsQuery {
            cursor: p1.next_cursor,
            limit: Some(3),
        },
    )
    .await
    .unwrap();
    assert_eq!(p2.items.len(), 1);
    assert!(p2.next_cursor.is_none());
}

// ---------------------------------------------------------------- T-M1-101

#[test]
fn t_m1_101_page_context_takes_references_only() {
    for bad in [
        json!({"text": "hi", "page_context": [{"kind": "issue", "key": "AUTH-1", "text": "body"}]}),
        json!({"text": "hi", "page_context": [{"kind": "issue", "content": "body"}]}),
        json!({"text": "hi", "page_context": [{"kind": "page", "html": "<p>"}]}),
        json!({"text": "hi", "page_text": "body"}),
    ] {
        assert!(serde_json::from_value::<SendInput>(bad).is_err());
    }
    let ok: SendInput = serde_json::from_value(json!({"text": "hi",
        "page_context": [{"kind": "issue", "key": "AUTH-1"}, {"kind": "project", "key": "AUTH"},
                         {"kind": "view", "id": "builtin:open"}]}))
    .unwrap();
    assert_eq!(ok.page_context.len(), 3);
    assert_eq!(
        ok.page_context[0],
        PageRef::Issue {
            key: "AUTH-1".into()
        }
    );
}

// ---------------------------------------------------------------- T-M1-107, 106

async fn stored_turn(w: &World, c: Uuid, project: Uuid) -> Uuid {
    let run = id(
        &w.pool,
        &format!(
            "INSERT INTO ai_runs (conversation_id, user_id, provider_id, model, prompt_version, state, finished_at,
                                  input_tokens, output_tokens, cost_usd, tool_calls)
             SELECT '{c}', '{}', active_provider_id, 'm', 1, 'succeeded', now(), 120, 30, 0.000810, 1
             FROM ai_settings RETURNING id",
            w.alice
        ),
    )
    .await;
    exec(
        &w.pool,
        &format!(
            "INSERT INTO ai_messages (conversation_id, run_id, seq, role, parts, source_projects) VALUES
               ('{c}', NULL, 1, 'user', '[{{\"type\":\"text\",\"text\":\"question\"}}]', '{{}}'),
               ('{c}', '{run}', 2, 'assistant', '[{{\"type\":\"tool_call\",\"id\":\"a\",\"name\":\"get\",\"input\":{{}}}}]', '{{{project}}}'),
               ('{c}', '{run}', 3, 'tool', '[{{\"type\":\"tool_result\",\"call_id\":\"a\",\"content\":\"CANARY-SECRET\",\"issues\":[]}}]', '{{{project}}}'),
               ('{c}', '{run}', 4, 'assistant', '[{{\"type\":\"text\",\"text\":\"answer CANARY-SECRET\"}}]', '{{{project}}}')"
        ),
    )
    .await;
    run
}

#[tokio::test]
async fn t_m1_107_delete_and_retention() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    on(&w).await;
    let s = assistant(&w, w.alice_ctx).await;
    delegate_ai(
        &w,
        w.alice_ctx,
        s.assistant.unwrap().principal,
        w.web,
        &[],
        false,
    )
    .await
    .unwrap();
    // Deletion: content and title go in one transaction; the run stays.
    let c = conversation(&w, w.alice_ctx, "secret title").await;
    let run = stored_turn(&w, c, w.web).await;
    let m = chat::messages(&w.app, w.alice_ctx, c).await.unwrap();
    assert_eq!(m.items.len(), 3, "tool messages are never returned");
    assert!(
        m.items
            .iter()
            .all(|x| x.parts.iter().all(|p| p["type"] != "tool_call"))
    );
    chat::delete(&w.app, w.alice_ctx, c).await.unwrap();
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_messages WHERE conversation_id = '{c}' AND parts IS NOT NULL")).await,
        0
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_conversations WHERE id = '{c}' AND title = '' AND deleted_at IS NOT NULL")).await,
        1
    );
    assert_eq!(
        code(&chat::messages(&w.app, w.alice_ctx, c).await.unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_runs WHERE id = '{run}' AND cost_usd = 0.000810 AND tool_calls = 1")).await,
        1
    );
    // Retention: past purge_after the content goes; 90 days later the rows.
    let c2 = conversation(&w, w.alice_ctx, "old").await;
    let run2 = stored_turn(&w, c2, w.web).await;
    exec(&pool, &format!("UPDATE ai_conversations SET purge_after = now() - interval '1 minute' WHERE id = '{c2}'")).await;
    let (purged, _) = chat::retention(&pool).await.unwrap();
    assert_eq!(purged, 1);
    let m = chat::messages(&w.app, w.alice_ctx, c2).await.unwrap();
    assert_eq!(m.conversation.title, "");
    assert!(
        m.items
            .iter()
            .all(|x| x.parts == vec![json!({"type": "unavailable", "reason": "purged"})])
    );
    exec(
        &pool,
        &format!(
            "UPDATE ai_conversations SET purged_at = now() - interval '91 days' WHERE id = '{c2}'"
        ),
    )
    .await;
    let (_, gone) = chat::retention(&pool).await.unwrap();
    assert!(gone >= 1);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM ai_conversations WHERE id = '{c2}'")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM ai_messages WHERE conversation_id = '{c2}'")
        )
        .await,
        0
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_runs WHERE id = '{run2}' AND conversation_id IS NULL AND cost_usd = 0.000810")).await,
        1,
        "runs keep their counts and costs"
    );
    // Retention days follow the setting.
    let c3 = conversation(&w, w.alice_ctx, "new").await;
    let days: f64 = sqlx::query_scalar(
        "SELECT (extract(epoch FROM purge_after - created_at) / 86400)::float8 FROM ai_conversations WHERE id = $1",
    )
    .bind(c3)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((days - 30.0).abs() < 0.01, "{days}");
}

#[tokio::test]
async fn t_m1_105_106_recheck_and_d12() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    on(&w).await;
    let s = assistant(&w, w.alice_ctx).await;
    let principal = s.assistant.unwrap().principal;
    delegate_ai(&w, w.alice_ctx, principal, w.web, &[], false)
        .await
        .unwrap();
    delegate_ai(&w, w.alice_ctx, principal, w.auth_p, &[], false)
        .await
        .unwrap();
    let c = conversation(&w, w.alice_ctx, "t").await;
    let i = w.issue(w.auth_p, "auth issue").await;
    let key: String = sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 LIMIT 1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    stored_turn(&w, c, w.auth_p).await;
    // The tool result names the issue.
    exec(&pool, &format!(
        "UPDATE ai_messages SET parts = jsonb_set(parts, '{{0,issues}}', '[\"{i}\"]') WHERE conversation_id = '{c}' AND role = 'tool'"
    )).await;
    let m = chat::messages(&w.app, w.alice_ctx, c).await.unwrap();
    assert!(serde_json::to_string(&m).unwrap().contains("CANARY-SECRET"));
    // T-M1-105: the issue turns private → the reply is a placeholder, for
    // alice too (her agents could not read it).
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{i}'"),
    )
    .await;
    let m = chat::messages(&w.app, w.alice_ctx, c).await.unwrap();
    let text = serde_json::to_string(&m).unwrap();
    assert!(!text.contains("CANARY-SECRET"), "{text}");
    assert!(text.contains("source_revoked"));
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = false WHERE id = '{i}'"),
    )
    .await;
    let _ = key;
    // T-M1-106: D12 on AUTH empties messages drawn from AUTH in the same
    // transaction; others stay.
    let c2 = conversation(&w, w.alice_ctx, "web").await;
    stored_turn(&w, c2, w.web).await;
    let mut tx = pool.begin().await.unwrap();
    oxledger::redaction::derived(&mut tx, w.auth_p, w.alice)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_messages WHERE conversation_id = '{c}' AND role <> 'user' AND parts IS NOT NULL")).await,
        0
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM ai_messages WHERE conversation_id = '{c2}' AND parts IS NOT NULL")).await,
        4
    );
    let m = chat::messages(&w.app, w.alice_ctx, c).await.unwrap();
    assert!(serde_json::to_string(&m).unwrap().contains("redacted"));
}

// ---------------------------------------------------------------- T-M1-116, 134

#[tokio::test]
async fn t_m1_116_new_context_carries_delegations() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    on(&w).await;
    let s = assistant(&w, w.alice_ctx).await;
    let a = s.assistant.unwrap();
    delegate_ai(
        &w,
        w.alice_ctx,
        a.principal,
        w.web,
        &[("add_note", Review)],
        false,
    )
    .await
    .unwrap();
    delegate_ai(&w, w.alice_ctx, a.principal, w.auth_p, &[], false)
        .await
        .unwrap();
    let c = conversation(&w, w.alice_ctx, "old").await;
    // alice leaves AUTH: its epoch moves and the context is quarantined.
    exec(&pool, &format!(
        "DELETE FROM member_roles WHERE member_id IN (SELECT id FROM members WHERE project_id = '{}' AND principal_id = '{}');
         DELETE FROM members WHERE project_id = '{}' AND principal_id = '{}'",
        w.auth_p, w.alice, w.auth_p, w.alice
    )).await;
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    assert_eq!(s.assistant.unwrap().context_state, "quarantined");
    // One click: a new context with the delegations still permitted.
    ai_team::runtime_new_context(
        &w.app,
        &w.policy,
        w.alice_ctx,
        a.runtime_id,
        &RuntimeNewContextInput {
            fresh_context_ack: true,
            carry_over_delegations: true,
        },
    )
    .await
    .unwrap();
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    let na = s.assistant.unwrap();
    assert_eq!(na.context_state, "active");
    assert_eq!(na.delegated_projects, 1, "only WEB is still permitted");
    let (proj, mode): (Uuid, String) = sqlx::query_as(
        "SELECT d.project_id, p.mode::text FROM delegations d
         JOIN delegation_command_policies p ON p.delegation_id = d.id AND p.command_kind = 'add_note'
         WHERE d.agent_principal_id = $1 AND d.revoked_at IS NULL",
    )
    .bind(a.principal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((proj, mode.as_str()), (w.web, "review"), "same policies");
    // The old conversation is read-only.
    let l = chat::list(&w.app, w.alice_ctx, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        l.items.iter().find(|x| x.id == c).unwrap().state,
        "read_only"
    );
    let e = send(&w, w.alice_ctx, c, "hi", vec![], None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AiConversationReadOnly);
    // A new conversation is bound to the new context.
    let c2 = conversation(&w, w.alice_ctx, "new").await;
    assert_eq!(
        l.items.iter().filter(|x| x.id == c2).count(),
        0,
        "listed after creation only"
    );
    let _ = Direct;
}

#[tokio::test]
async fn t_m1_134_extend_delegations() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    on(&w).await;
    let s = assistant(&w, w.alice_ctx).await;
    let a = s.assistant.unwrap();
    let d = delegate_ai(
        &w,
        w.alice_ctx,
        a.principal,
        w.web,
        &[("add_note", Review)],
        true,
    )
    .await
    .unwrap();
    // Six days left.
    exec(&pool, &format!(
        "UPDATE delegations SET created_at = now() - interval '24 days', expires_at = now() + interval '6 days' WHERE id = '{}'",
        d.id
    )).await;
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    assert!(s.assistant.as_ref().unwrap().expiring_soon);
    let s = chat::extend(&w.app, &w.policy, w.alice_ctx).await.unwrap();
    let a2 = s.assistant.unwrap();
    assert!(!a2.expiring_soon);
    assert!(a2.delegations_expire_at.unwrap() > chrono::Utc::now() + chrono::Duration::days(29));
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM delegations WHERE id = '{}' AND revoked_at IS NOT NULL",
                d.id
            )
        )
        .await,
        1
    );
    let (mode, time): (String, bool) = sqlx::query_as(
        "SELECT p.mode::text, d.read_own_time FROM delegations d
         JOIN delegation_command_policies p ON p.delegation_id = d.id AND p.command_kind = 'add_note'
         WHERE d.agent_principal_id = $1 AND d.revoked_at IS NULL",
    )
    .bind(a.principal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((mode.as_str(), time), ("review", true));
}

// ---------------------------------------------------------------- T-M1-109 (refusals), 131

#[tokio::test]
async fn t_m1_109_rate_and_budget_refusals() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let (root, _) = on(&w).await;
    assistant(&w, w.alice_ctx).await;
    let c = conversation(&w, w.alice_ctx, "t").await;
    // The reservation formula (V15): 3 / 15 USD, 4096 output tokens.
    let r: String = sqlx::query_scalar(
        "SELECT (ceil(50000 * input_usd_per_mtok + max_output_tokens * output_usd_per_mtok) / 1000000)::numeric(10,6)::text
         FROM ai_providers",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(r, "0.211440");
    // Hourly: two runs already this hour with a limit of 2.
    put(&w, root, |i| i.user_runs_per_hour = 2).await.unwrap();
    for m in [50, 10] {
        exec(&pool, &format!(
            "INSERT INTO ai_runs (conversation_id, user_id, provider_id, model, prompt_version, state, finished_at, started_at)
             SELECT '{c}', '{}', active_provider_id, 'm', 1, 'succeeded', now(), now() - interval '{m} minutes' FROM ai_settings",
            w.alice
        )).await;
    }
    let e = send(&w, w.alice_ctx, c, "hi", vec![], None)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::QuotaExceeded);
    let d = details(&e);
    assert_eq!(d["scope"], "ai_user_hour");
    let retry: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(d["retry_at"].clone()).unwrap();
    let want = chrono::Utc::now() + chrono::Duration::minutes(10);
    assert!((retry - want).num_seconds().abs() < 30, "{retry} vs {want}");
    let s = chat::status(&w.app, w.alice_ctx).await.unwrap();
    assert_eq!(s.limits.runs_left_this_hour, 0);
    put(&w, root, |i| i.user_runs_per_hour = 30).await.unwrap();
    // Daily: 0.10 a day is less than one reservation.
    put(&w, root, |i| i.user_daily_budget_usd = "0.10".into())
        .await
        .unwrap();
    let e = send(&w, w.alice_ctx, c, "hi", vec![], None)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["scope"], "ai_user_day");
    let tomorrow: bool = sqlx::query_scalar(
        "SELECT $1::timestamptz = (((now() AT TIME ZONE timezone)::date + 1)::timestamp AT TIME ZONE timezone) FROM org_settings",
    )
    .bind(serde_json::from_value::<chrono::DateTime<chrono::Utc>>(details(&e)["retry_at"].clone()).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        tomorrow,
        "retry_at is tomorrow 00:00 in the organisation's time zone"
    );
    put(&w, root, |i| i.user_daily_budget_usd = "2.00".into())
        .await
        .unwrap();
    // Monthly: 100 % spent.
    exec(&pool, "INSERT INTO ai_budget_months (month, spent_usd)
                 SELECT date_trunc('month', (now() AT TIME ZONE timezone))::date, 100 FROM org_settings
                 ON CONFLICT (month) DO UPDATE SET spent_usd = 100").await;
    let e = send(&w, w.alice_ctx, c, "hi", vec![], None)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["scope"], "org_month");
    assert!(details(&e)["retry_at"].is_string());
    // Nothing was reserved by the refusals.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM ai_budget_months WHERE reserved_usd <> 0"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM ai_usage_daily WHERE reserved_usd <> 0"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn t_m1_131_provider_change_needs_ack() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let (root, a) = on(&w).await;
    assistant(&w, w.alice_ctx).await;
    let c = conversation(&w, w.alice_ctx, "t").await;
    // First send with A (the run ends at once in a build without the AI
    // client, or talks to nobody: the endpoint does not resolve).
    let r = send(&w, w.alice_ctx, c, "hi", vec![], None).await.unwrap();
    wait_run(&w, r.run_id).await;
    // Switch to B.
    let mut nb = new_provider("Local", "openai_compat", "VLLM", Some("llama-3.3-70b"));
    nb.org_account_attested = false;
    let b = provider(&w, root, &nb).await;
    mark_tested(&w, b.id, false).await;
    put(&w, root, |i| i.active_provider_id = Some(b.id))
        .await
        .unwrap();
    for ack in [None, Some(a), Some(Uuid::now_v7())] {
        let e = send(&w, w.alice_ctx, c, "again", vec![], ack)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::AiProviderChanged);
        let d = details(&e);
        assert_eq!(d["provider"]["id"], json!(b.id));
        assert_eq!(d["provider"]["name"], "Local");
        assert_eq!(d["provider"]["model"], "llama-3.3-70b");
    }
    let r = send(&w, w.alice_ctx, c, "again", vec![], Some(b.id))
        .await
        .unwrap();
    wait_run(&w, r.run_id).await;
    let m = chat::messages(&w.app, w.alice_ctx, c).await.unwrap();
    assert_eq!(
        m.conversation.state, "open",
        "a provider change never archives"
    );
    assert!(
        serde_json::to_string(&m)
            .unwrap()
            .contains("provider_changed"),
        "the notice is stored"
    );
    // Retiring A changes nothing for the conversation; retiring B (in use)
    // turns the AI off.
    admin::retire_provider(&w.app, &w.policy, root, a)
        .await
        .unwrap();
    let s = settings(&w, root).await;
    assert!(s.enabled);
    admin::retire_provider(&w.app, &w.policy, root, b.id)
        .await
        .unwrap();
    let s = settings(&w, root).await;
    assert!(!s.enabled && s.active_provider_id.is_none());
    let l = chat::list(&w.app, w.alice_ctx, &Default::default())
        .await
        .unwrap();
    assert_eq!(l.items[0].state, "open");
}

// ---------------------------------------------------------------- fixed lists, guard, upgrade

fn quoted(def: &str) -> BTreeSet<String> {
    def.split('\'')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect()
}

async fn constraint(pool: &sqlx::PgPool, table: &str, column_hint: &str) -> BTreeSet<String> {
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = $1::regclass AND contype = 'c'
           AND pg_get_constraintdef(oid) LIKE 'CHECK ((' || $2 || ' = ANY%'",
    )
    .bind(table)
    .bind(column_hint)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(defs.len(), 1, "{table}.{column_hint}: {defs:?}");
    quoted(&defs[0])
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn fixed_lists_match_the_program() {
    let pool = test_pool().await;
    use oxledger::ai;
    assert_eq!(
        constraint(&pool, "ai_runs", "state").await,
        set(&ai::RUN_STATES)
    );
    assert_eq!(
        constraint(&pool, "ai_runs", "stop_reason").await,
        set(&ai::STOP_REASONS)
    );
    assert_eq!(
        constraint(&pool, "ai_providers", "kind").await,
        set(&ai::PROVIDER_KINDS)
    );
    assert_eq!(
        constraint(&pool, "ai_provider_events", "kind").await,
        set(&ai::PROVIDER_EVENT_KINDS)
    );
    assert_eq!(
        constraint(&pool, "ai_messages", "role").await,
        set(&ai::MESSAGE_ROLES)
    );
    // changesets has two origin lists; the agent one is named by 0030.
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'changesets_origin_agent_ck'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(quoted(&def), set(&ai::AGENT_ORIGINS));
}

#[tokio::test]
async fn t_m1_125_conversation_user_guard() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    on(&w).await;
    let a = assistant(&w, w.alice_ctx).await.assistant.unwrap();
    let ctx: Uuid = id(
        &pool,
        &format!(
            "SELECT id FROM agent_work_contexts WHERE runtime_id = '{}'",
            a.runtime_id
        ),
    )
    .await;
    // The agent principal as the conversation's user: refused by the guard
    // (before the foreign keys).
    let r = sqlx::query(
        "INSERT INTO ai_conversations (user_id, agent_principal_id, runtime_id, context_id, purge_after)
         VALUES ($1, $1, $2, $3, now())",
    )
    .bind(a.principal)
    .bind(a.runtime_id)
    .bind(ctx)
    .execute(&pool)
    .await
    .unwrap_err();
    let code = r
        .as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.to_string());
    assert_eq!(code.as_deref(), Some("23514"), "{r}");
    sqlx::query(
        "INSERT INTO ai_conversations (user_id, agent_principal_id, runtime_id, context_id, purge_after)
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(w.alice)
    .bind(a.principal)
    .bind(a.runtime_id)
    .bind(ctx)
    .execute(&pool)
    .await
    .unwrap();
}
