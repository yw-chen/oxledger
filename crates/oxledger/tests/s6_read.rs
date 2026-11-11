// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 03 §7 steps 1–3 (search explain, citations) and the reads contract
//! 02 用法 2 and 4 need (`get` fields / include, `get_many`, `status_since`,
//! `last_activity_at`): the §3.9 test list and T-U8.
//! Lexer properties (§3.9 first block) are unit tests in `search.rs`.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use ox_core::filter::{FieldType, Value as CfValue};
use oxledger::auth::AuthContext;
use oxledger::citations::{self, CitationStatus};
use oxledger::commands;
use oxledger::custom_fields::{self, NewField};
use oxledger::error::{ErrorCode, NextAction};
use oxledger::issue_view;
use oxledger::service;
use oxledger::test_support::{NewIssue, create_issue};
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

// ---------------------------------------------------------------- helpers

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

async fn sync(w: &World) {
    w.app.index.sync_all(&w.pool).await.unwrap();
}

async fn search(w: &World, who: AuthContext, p: Value) -> anyhow::Result<Value> {
    let r = service::search(&w.app, who, serde_json::from_value(p).unwrap()).await?;
    Ok(serde_json::to_value(r).unwrap())
}

async fn get(w: &World, who: AuthContext, p: Value) -> anyhow::Result<Value> {
    let r = issue_view::get(&w.app, who, serde_json::from_value(p).unwrap()).await?;
    Ok(serde_json::to_value(r).unwrap())
}

async fn get_many(w: &World, who: AuthContext, p: Value) -> anyhow::Result<Value> {
    let r = issue_view::get_many(&w.app, who, serde_json::from_value(p).unwrap()).await?;
    Ok(serde_json::to_value(r).unwrap())
}

async fn verify(w: &World, who: AuthContext, p: Value) -> anyhow::Result<Value> {
    let r = citations::verify_citations(&w.app, who, serde_json::from_value(p).unwrap()).await?;
    Ok(serde_json::to_value(r).unwrap())
}

async fn source(w: &World, who: AuthContext, p: Value) -> anyhow::Result<Value> {
    let r = citations::get_source(&w.app, who, serde_json::from_value(p).unwrap()).await?;
    Ok(serde_json::to_value(r).unwrap())
}

/// verify one (citation, quote) in `mode`; the per-item result.
async fn check(w: &World, who: AuthContext, cit: &str, quote: &str, mode: &str) -> Value {
    let r = verify(
        w,
        who,
        json!({"items": [{"citation_id": cit, "quote": quote}], "mode": mode}),
    )
    .await
    .unwrap();
    r["results"][0].clone()
}

fn item<'a>(r: &'a Value, key: &str) -> &'a Value {
    r["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == key)
        .unwrap_or_else(|| panic!("{key} not in {r}"))
}

fn keys(r: &Value) -> Vec<String> {
    r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect()
}

/// The highlighted substrings of an evidence entry, and their strengths.
fn lit(ev: &Value) -> Vec<(String, String)> {
    let chars: Vec<char> = ev["text"].as_str().unwrap().chars().collect();
    ev["highlights"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let (s, e) = (
                h["start"].as_u64().unwrap() as usize,
                h["end"].as_u64().unwrap() as usize,
            );
            (
                chars[s..e].iter().collect(),
                h["strength"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn has_key(v: &Value, k: &str) -> bool {
    match v {
        Value::Object(m) => m.contains_key(k) || m.values().any(|x| has_key(x, k)),
        Value::Array(a) => a.iter().any(|x| has_key(x, k)),
        _ => false,
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

/// A citation for a source that does not exist.
fn ghost_field() -> String {
    let body = json!({"v": 1, "src": {"kind": "issue_field", "issue_id": Uuid::now_v7(), "field": "subject"},
                      "rev": oxledger::auth::encode(&[0u8; 32]), "start": 0, "end": 3});
    format!(
        "cit1.{}",
        oxledger::auth::encode(body.to_string().as_bytes())
    )
}

fn ghost_note(issue: Uuid) -> String {
    let body = json!({"v": 1, "src": {"kind": "issue_note", "issue_id": issue, "note_id": Uuid::now_v7()},
                      "rev": oxledger::auth::encode(&[0u8; 32]), "start": 0, "end": 3});
    format!(
        "cit1.{}",
        oxledger::auth::encode(body.to_string().as_bytes())
    )
}

async fn note(w: &World, issue: Uuid, body: &str, private: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO issue_notes (issue_id, author_id, body, is_private) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(issue)
    .bind(w.bob)
    .bind(body)
    .bind(private)
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

async fn submit(w: &World, ops: Vec<Value>) -> commands::CommandResult {
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &json!({"command_schema_version": 1, "ops": ops, "preconditions": []}),
    )
    .await
    .unwrap()
}

/// One op on an existing issue, with its current lock_version as precondition.
async fn act(w: &World, issue: Uuid, op: Value) -> commands::CommandResult {
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(issue)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &json!({"command_schema_version": 1, "ops": [op],
                "preconditions": [{"target": tgt(issue), "lock_version": lv}]}),
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------- §3.9 CJK, identifiers, Unicode, score

#[tokio::test]
async fn search_evidence_highlights() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (_, k1) = mk(&w, w.web, "使用者登入失敗", "", false).await;
    let (_, k2) = mk(&w, w.web, "密碼問題", "輸入密碼後登入", false).await;
    let (_, k3) = mk(&w, w.web, "結帳", "付款失敗，錢沒有扣到", false).await;
    let (_, k4) = mk(&w, w.web, "OAuth", "回 invalid_grant 錯誤", false).await;
    let (_, k5) = mk(&w, w.web, "OAuth2", "回 invalid request 錯誤", false).await;
    let (_, u1) = mk(&w, w.web, "😀登入 emoji", "", false).await;
    let (_, u2) = mk(&w, w.web, "👨\u{200D}👩\u{200D}👧登入 family", "", false).await;
    let (_, u3) = mk(&w, w.web, "crlf", "a\r\n登入", false).await;
    let (_, u4) = mk(&w, w.web, "İ登入", "", false).await;
    let (_, u5) = mk(&w, w.web, "e\u{0301}登入", "", false).await;
    sync(&w).await;

    // Overlapping bigrams: one highlight, no character twice.
    let r = search(&w, a, json!({"q": "登入失敗"})).await.unwrap();
    let it = item(&r, &k1);
    assert_eq!(it["match"]["strength"], "primary");
    assert_eq!(it["evidence"][0]["text"], "使用者登入失敗");
    assert_eq!(
        lit(&it["evidence"][0]),
        vec![("登入失敗".into(), "primary".into())]
    );

    // 登入 does not light 輸入's 入.
    let r = search(&w, a, json!({"q": "登入", "limit": 20}))
        .await
        .unwrap();
    let it = item(&r, &k2);
    assert_eq!(
        it["match"],
        json!({"fields": ["description"], "strength": "primary"})
    );
    assert_eq!(
        lit(&it["evidence"][0]),
        vec![("登入".into(), "primary".into())]
    );
    assert_eq!(it["evidence"][0]["field"], "description");
    assert_eq!(it["evidence"][0]["evidence_status"], "current");
    // Code point offsets equal JavaScript's Array.from indices (fixed cases).
    for (k, field, start) in [
        (&u1, "subject", 1),
        (&u2, "subject", 5),
        (&u3, "description", 3),
        (&u4, "subject", 1),
        (&u5, "subject", 2),
    ] {
        let ev = &item(&r, k)["evidence"][0];
        assert_eq!(ev["field"], field, "{k}");
        assert_eq!(
            ev["highlights"][0],
            json!({"start": start, "end": start + 2, "strength": "primary"}),
            "{k}: {ev}"
        );
        // The snippet is the cited text verbatim.
        let v = check(&w, a, s(&ev["citation_id"]), s(&ev["text"]), "exact").await;
        assert_eq!(v["status"], "valid", "{k}: {v}");
    }
    // Ranks are 1..n; there is no score anywhere.
    let ranks: Vec<u64> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["rank"].as_u64().unwrap())
        .collect();
    assert_eq!(ranks, (1..=ranks.len() as u64).collect::<Vec<_>>());
    assert!(!has_key(&r, "score"), "{r}");

    // 付不了錢: only unigrams match → fallback, 付 and 錢 lit.
    let r = search(&w, a, json!({"q": "付不了錢"})).await.unwrap();
    let it = item(&r, &k3);
    assert_eq!(it["match"]["strength"], "fallback");
    assert_eq!(
        lit(&it["evidence"][0]),
        vec![
            ("付".into(), "fallback".into()),
            ("錢".into(), "fallback".into())
        ]
    );

    // Identifiers: whole vs segment only; a full-weight repeat makes it primary.
    let r = search(&w, a, json!({"q": "invalid_grant"})).await.unwrap();
    assert_eq!(keys(&r)[0], k4);
    assert_eq!(
        lit(&item(&r, &k4)["evidence"][0]),
        vec![("invalid_grant".into(), "primary".into())]
    );
    assert_eq!(item(&r, &k5)["match"]["strength"], "fallback");
    assert_eq!(
        lit(&item(&r, &k5)["evidence"][0]),
        vec![("invalid".into(), "fallback".into())]
    );
    let r = search(&w, a, json!({"q": "invalid_grant invalid"}))
        .await
        .unwrap();
    assert_eq!(item(&r, &k5)["match"]["strength"], "primary");
}

// ---------------------------------------------------------------- §3.4 step 1, parameters

#[tokio::test]
async fn search_query_classes_and_fields() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (_, k) = mk(&w, w.web, "登入", "", false).await;
    sync(&w).await;

    // Non-blank without terms: invalid_query, never "list everything".
    let e = search(&w, a, json!({"q": "!!，。"})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidQuery);
    let body = oxledger::error::classify(&e).unwrap().body(Uuid::nil());
    assert_eq!(body.next_actions, &[NextAction::FixInput]);
    assert!(!body.retryable);
    // Blank q is no q: SQL only, match null, no evidence.
    let r = search(&w, a, json!({"q": "   "})).await.unwrap();
    assert_eq!(
        r["consistency"],
        json!({"status": "not_applicable", "reason": "sql_only"})
    );
    let it = item(&r, &k);
    assert_eq!(it["match"], Value::Null);
    assert_eq!(it["evidence"], json!([]));
    assert!(it.get("rank").is_none());
    let r = search(&w, a, json!({"q": "登入"})).await.unwrap();
    assert_eq!(r["consistency"]["status"], "eventual");
    assert_eq!(
        (r["outcome"].clone(), r["complete"].clone()),
        (json!("ok"), json!(true))
    );

    // explain / fields validation.
    let e = search(&w, a, json!({"q": "登入", "explain": "verbose"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let d = oxledger::error::classify(&e).unwrap().details.unwrap();
    assert_eq!(d["allowed"], json!(["none", "brief", "full"]));
    let e = search(&w, a, json!({"fields": ["key", "score"]}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let d = oxledger::error::classify(&e).unwrap().details.unwrap();
    assert_eq!(d["path"], "fields");
    for f in ["lock_version", "priority", "created_at", "status_category"] {
        assert!(d["allowed"].as_array().unwrap().contains(&json!(f)), "{f}");
    }
    let e = search(&w, a, json!({"fields": ["cf:Nope"]}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    // Only the requested fields (and key), lock_version from SQL.
    let r = search(
        &w,
        a,
        json!({"q": "登入", "fields": ["subject", "lock_version", "priority", "created_at"], "explain": "none"}),
    )
    .await
    .unwrap();
    let it = item(&r, &k);
    let mut names: Vec<&str> = it.as_object().unwrap().keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "created_at",
            "evidence",
            "key",
            "lock_version",
            "match",
            "priority",
            "rank",
            "subject"
        ]
    );
    assert_eq!(it["lock_version"], 0);
    assert_eq!(it["priority"], "Normal");
    // explain none: match but no snippets or citations.
    assert_eq!(it["evidence"], json!([]));
    assert_eq!(it["match"]["strength"], "primary");
}

// ---------------------------------------------------------------- §3.9 index out of sync

#[tokio::test]
async fn stale_index_terms_never_produce_evidence() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (gone, kg) = mk(&w, w.web, "登入流程", "", false).await;
    let (weak, kw) = mk(&w, w.web, "登入錯誤", "", false).await;
    let (_, kk) = mk(&w, w.web, "登入正常", "", false).await;
    sync(&w).await;
    // SQL changes; the index is not synced and still holds the old terms.
    for (id, subject) in [(gone, "其他流程"), (weak, "登記與輸入錯誤")] {
        sqlx::query("UPDATE issues SET subject = $2 WHERE id = $1")
            .bind(id)
            .bind(subject)
            .execute(&pool)
            .await
            .unwrap();
    }
    for explain in ["none", "brief", "full"] {
        let r = search(&w, a, json!({"q": "登入", "explain": explain}))
            .await
            .unwrap();
        let ks = keys(&r);
        assert!(
            !ks.contains(&kg),
            "{explain}: dropped, not shown from the old text: {r}"
        );
        assert!(ks.contains(&kk) && ks.contains(&kw), "{explain}");
        // Only single-character matches remain: fallback, from the new text.
        let it = item(&r, &kw);
        assert_eq!(it["match"]["strength"], "fallback", "{explain}");
        if explain != "none" {
            assert_eq!(it["evidence"][0]["text"], "登記與輸入錯誤");
            assert_eq!(it["evidence"][0]["evidence_status"], "rechecked");
            assert!(!r.to_string().contains("登入流程"));
        }
    }
}

// ---------------------------------------------------------------- §3.9 budget, paging

#[tokio::test]
async fn evidence_budget_and_rank_across_pages() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let filler = "這是一段很長的說明文字，用來把命中的位置分開。".repeat(8);
    let desc = format!("開頭登入一次。{filler}中間又登入。{filler}最後登入結束。");
    for n in 0..60 {
        let subject: &'static str = Box::leak(format!("案例 {n} 登入").into_boxed_str());
        let d: &'static str = Box::leak(desc.clone().into_boxed_str());
        mk(&w, w.web, subject, d, false).await;
    }
    sync(&w).await;

    // brief, limit 50: at most one snippet per item, total within limit × 120.
    let r = search(&w, a, json!({"q": "登入", "limit": 50, "explain": "brief"}))
        .await
        .unwrap();
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 50);
    let mut total = 0;
    for it in items {
        let ev = it["evidence"].as_array().unwrap();
        assert_eq!(ev.len(), 1, "{it}");
        let n = s(&ev[0]["text"]).chars().count();
        assert!(n <= 60 + 2 * 13, "brief window ~60: {n}");
        total += n;
    }
    assert!(total <= 50 * 120, "{total}");
    let bytes = r.to_string().len();
    assert!(bytes < 120_000, "brief response size bounded: {bytes}");
    assert_eq!(r["evidence_truncated"], false);

    // full: over 8000 code points of snippets → evidence_truncated, and every
    // citation in the response verifies against its returned text.
    let r = search(&w, a, json!({"q": "登入", "limit": 50, "explain": "full"}))
        .await
        .unwrap();
    assert_eq!(r["evidence_truncated"], true);
    let mut total = 0;
    let mut pairs = Vec::new();
    for it in r["items"].as_array().unwrap() {
        let ev = it["evidence"].as_array().unwrap();
        assert!(!ev.is_empty(), "each item keeps its best snippet");
        for e in ev {
            total += s(&e["text"]).chars().count();
            pairs.push(json!({"citation_id": e["citation_id"], "quote": e["text"]}));
        }
    }
    assert!(total <= 8000, "{total}");
    for chunk in pairs.chunks(50) {
        let v = verify(&w, a, json!({"items": chunk})).await.unwrap();
        for res in v["results"].as_array().unwrap() {
            assert_eq!(res["status"], "valid", "{res}");
        }
    }

    // rank continues across pages.
    let p1 = search(&w, a, json!({"q": "登入", "limit": 20, "explain": "none"}))
        .await
        .unwrap();
    let p2 = search(
        &w,
        a,
        json!({"q": "登入", "limit": 20, "explain": "none", "cursor": p1["next_cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(p2["items"][0]["rank"], 21);
    let mut all = keys(&p1);
    all.extend(keys(&p2));
    let mut dedup = all.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 40, "no repeats across pages");
}

// ---------------------------------------------------------------- §3.9 citation checks

#[tokio::test]
async fn citations_exact_reanchor_and_get_source() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let (id, k) = mk(
        &w,
        w.web,
        "SSO 登入後跳回登入頁",
        "從公司入口登入後又被導回登入頁",
        false,
    )
    .await;
    sync(&w).await;

    let g = get(
        &w,
        a,
        json!({"key": k, "fields": ["subject", "description"], "citations": ["subject", "description"]}),
    )
    .await
    .unwrap();
    let cs = s(&g["citations"]["subject"]).to_string();
    let cd = s(&g["citations"]["description"]).to_string();
    let desc = "從公司入口登入後又被導回登入頁";
    assert_eq!(check(&w, a, &cd, desc, "exact").await["status"], "valid");
    let v = check(&w, a, &cd, "從公司入口", "exact").await;
    assert_eq!(v["status"], "citation_mismatch");
    assert_eq!(v["next_actions"], json!(["fix_input"]));
    // citations must be a subset of fields, and only text fields.
    let e = get(
        &w,
        a,
        json!({"key": k, "fields": ["subject"], "citations": ["description"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    assert_eq!(
        oxledger::error::classify(&e).unwrap().details.unwrap()["path"],
        "citations"
    );

    // get_source: subquote → a precise citation; ambiguous when repeated.
    let r = source(
        &w,
        a,
        json!({"citation_id": cd, "quote": "公司入口", "selection": "subquote", "context_chars": 3}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "valid", "{r}");
    assert_eq!(r["quote"], "公司入口");
    assert_eq!(r["context"]["text"], "從公司入口登入後");
    assert_eq!(r["context"]["highlights"][0]["start"], 1);
    let sub = s(&r["citation_id"]).to_string();
    assert_eq!(
        check(&w, a, &sub, "公司入口", "exact").await["status"],
        "valid"
    );
    let r = source(
        &w,
        a,
        json!({"citation_id": cd, "quote": "登入", "selection": "subquote"}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "ambiguous");
    assert_eq!(r["details"]["count"], 2);
    assert_eq!(r["next_actions"], json!(["fix_input", "ask_human"]));
    // range: the quote must be the whole cited range; context stays in the field.
    let r = source(
        &w,
        a,
        json!({"citation_id": sub, "quote": "公司入口", "context_chars": 2000}),
    )
    .await
    .unwrap();
    assert_eq!(r["context"]["text"], desc, "never extends into the subject");

    // Change the description: the subject citation is unaffected (per-field rev).
    let search_r = search(&w, a, json!({"q": "登入"})).await.unwrap();
    let ev = item(&search_r, &k)["evidence"][0].clone();
    sqlx::query("UPDATE issues SET description = $2 WHERE id = $1")
        .bind(id)
        .bind("前言。從公司入口登入後又被導回登入頁")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        check(&w, a, &cs, "SSO 登入後跳回登入頁", "exact").await["status"],
        "valid"
    );
    if ev["field"] == "subject" {
        assert_eq!(
            check(&w, a, s(&ev["citation_id"]), s(&ev["text"]), "exact").await["status"],
            "valid"
        );
    }
    let v = check(&w, a, &sub, "公司入口", "exact").await;
    assert_eq!(v["status"], "source_changed");
    assert_eq!(
        v["details"],
        json!({"suggested_tool": "get_source", "mode": "reanchor"})
    );
    let v = check(&w, a, &sub, "公司入口", "reanchor").await;
    assert_eq!(v["status"], "moved", "{v}");
    let moved = s(&v["citation_id"]).to_string();
    assert_eq!(
        check(&w, a, &moved, "公司入口", "exact").await["status"],
        "valid"
    );
    assert_eq!(
        check(&w, a, &cd, "登入", "reanchor").await["status"],
        "ambiguous"
    );
    assert_eq!(
        check(&w, a, &cd, "不存在的字", "reanchor").await["status"],
        "source_changed"
    );
    let r = source(
        &w,
        a,
        json!({"citation_id": sub, "quote": "公司入口", "mode": "reanchor"}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "moved");

    // Malformed, unknown kind, bad range, over-long range.
    let forge = |src: Value, start: u64, end: u64| {
        let body = json!({"v": 1, "src": src, "rev": oxledger::auth::encode(&[0u8; 32]), "start": start, "end": end});
        format!(
            "cit1.{}",
            oxledger::auth::encode(body.to_string().as_bytes())
        )
    };
    for bad in [
        "garbage".to_string(),
        "cit1.!!!".to_string(),
        forge(
            json!({"kind": "custom_field", "issue_id": id, "field_id": 1}),
            0,
            3,
        ),
        forge(
            json!({"kind": "issue_field", "issue_id": id, "field": "subject"}),
            3,
            3,
        ),
        forge(
            json!({"kind": "issue_field", "issue_id": id, "field": "subject"}),
            0,
            2001,
        ),
        forge(
            json!({"kind": "issue_field", "issue_id": id, "field": "priority"}),
            0,
            3,
        ),
    ] {
        let v = check(&w, a, &bad, "abc", "exact").await;
        assert_eq!(v["status"], "invalid_citation", "{bad}: {v}");
        assert_eq!(v["next_actions"], json!(["fix_input"]));
    }
    // Whole-request errors: empty quote, batch over 50.
    let e = verify(&w, a, json!({"items": [{"citation_id": cd, "quote": ""}]}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);
    let many: Vec<Value> = (0..51)
        .map(|_| json!({"citation_id": cd, "quote": "x"}))
        .collect();
    let e = verify(&w, a, json!({"items": many})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidArgument);

    // The crate function propose uses (contract 02 §6.7 step 7).
    let mut conn = pool.acquire().await.unwrap();
    for (cit, quote, want) in [
        (moved.as_str(), "公司入口", CitationStatus::Valid),
        (moved.as_str(), "公司", CitationStatus::CitationMismatch),
        (sub.as_str(), "公司入口", CitationStatus::SourceChanged),
        ("garbage", "x", CitationStatus::InvalidCitation),
        (ghost_field().as_str(), "abc", CitationStatus::NotFound),
        (moved.as_str(), "", CitationStatus::CitationMismatch),
    ] {
        let got = citations::verify_one(&mut conn, a, cit, quote)
            .await
            .unwrap();
        assert_eq!(got, want, "{cit} {quote}");
    }
}

// ---------------------------------------------------------------- §3.9 permissions (people)

#[tokio::test]
async fn people_lose_access_to_citations_and_private_notes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // dave: WEB member with view_issues only (default visibility, no private notes).
    let dave = oxledger::auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let rep = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','default') RETURNING id",
    )
    .await;
    grant(&pool, rep, &["view_issues"]).await;
    let m = member(&pool, w.web, dave, rep).await;
    let d = AuthContext::test_user(dave);
    let (x, kx) = mk(&w, w.web, "公開的單", "內容", false).await;
    let (y, _) = mk(&w, w.web, "另一張", "別的", false).await;
    let public = note(&w, x, "公開留言", false).await;
    let private = note(&w, x, "私有留言", true).await;
    let other = note(&w, y, "另一張的留言", false).await;

    // recent_notes: a hidden note takes no slot and leaves no trace.
    let gd = get(&w, d, json!({"key": kx, "fields": ["subject"], "include": ["recent_notes", "recent_events"], "note_limit": 1}))
        .await
        .unwrap();
    assert_eq!(gd["recent_notes"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(gd["recent_notes"]["items"][0]["note_id"], json!(public));
    assert_eq!(gd["recent_notes"]["truncated"], false);
    assert!(!gd.to_string().contains("私有留言"));
    let ga = get(
        &w,
        a,
        json!({"key": kx, "fields": ["subject"], "include": ["recent_notes"]}),
    )
    .await
    .unwrap();
    let notes = ga["recent_notes"]["items"].as_array().unwrap();
    assert_eq!(notes.len(), 2, "alice (view_private_notes) sees both");
    let cit_of = |n: Uuid| {
        notes
            .iter()
            .find(|x| x["note_id"] == json!(n))
            .map(|x| s(&x["citation_id"]).to_string())
            .unwrap()
    };
    let (c_pub, c_priv) = (cit_of(public), cit_of(private));
    assert_eq!(
        check(&w, a, &c_priv, "私有留言", "exact").await["status"],
        "valid"
    );

    // dave: the private note's citation is not_found, byte for byte the same
    // as a note that does not exist.
    let hidden = check(&w, d, &c_priv, "私有留言", "exact").await;
    let missing = check(&w, d, &ghost_note(x), "私有留言", "exact").await;
    assert_eq!(hidden["status"], "not_found");
    assert_eq!(hidden.to_string(), missing.to_string());
    let hs = source(&w, d, json!({"citation_id": c_priv, "quote": "私有留言"}))
        .await
        .unwrap();
    let ms = source(
        &w,
        d,
        json!({"citation_id": ghost_note(x), "quote": "私有留言"}),
    )
    .await
    .unwrap();
    assert_eq!(hs.to_string(), ms.to_string());
    assert_eq!(
        check(&w, d, &c_pub, "公開留言", "exact").await["status"],
        "valid"
    );

    // A forged pair (readable issue + another issue's note) is not_found, even for alice.
    let ga_y = get(
        &w,
        a,
        json!({"key": format!("WEB-{}", 2), "fields": ["subject"], "include": ["recent_notes"]}),
    )
    .await
    .unwrap();
    let c_other = s(&ga_y["recent_notes"]["items"][0]["citation_id"]).to_string();
    let forged = {
        let raw = c_other.strip_prefix("cit1.").unwrap();
        let mut v: Value =
            serde_json::from_slice(&oxledger::auth::decode_bytes(raw).unwrap()).unwrap();
        v["src"]["issue_id"] = json!(x);
        format!("cit1.{}", oxledger::auth::encode(v.to_string().as_bytes()))
    };
    assert_eq!(
        check(&w, a, &c_other, "另一張的留言", "exact").await["status"],
        "valid"
    );
    let f = check(&w, a, &forged, "另一張的留言", "exact").await;
    assert_eq!(
        f.to_string(),
        check(&w, a, &ghost_note(x), "另一張的留言", "exact")
            .await
            .to_string()
    );
    let _ = other;

    // The note becomes private → not_found for dave.
    sqlx::query("UPDATE issue_notes SET is_private = true WHERE id = $1")
        .bind(public)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        check(&w, d, &c_pub, "公開留言", "exact").await["status"],
        "not_found"
    );

    // The issue becomes private → its field citations are not_found for dave,
    // the same bytes as a missing issue; get says not_found too.
    let gx = get(
        &w,
        d,
        json!({"key": kx, "fields": ["subject"], "citations": ["subject"]}),
    )
    .await
    .unwrap();
    let c_subj = s(&gx["citations"]["subject"]).to_string();
    sqlx::query("UPDATE issues SET is_private = true WHERE id = $1")
        .bind(x)
        .execute(&pool)
        .await
        .unwrap();
    let hidden = check(&w, d, &c_subj, "公開的單", "exact").await;
    assert_eq!(
        hidden.to_string(),
        check(&w, d, &ghost_field(), "公開的單", "exact")
            .await
            .to_string()
    );
    assert_eq!(
        code(
            &get(&w, d, json!({"key": kx, "fields": ["subject"]}))
                .await
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    // Membership removed → not_found for a public issue too.
    sqlx::query("UPDATE issues SET is_private = false WHERE id = $1")
        .bind(x)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        check(&w, d, &c_subj, "公開的單", "exact").await["status"],
        "valid"
    );
    sqlx::query("DELETE FROM member_roles WHERE member_id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM members WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        check(&w, d, &c_subj, "公開的單", "exact").await["status"],
        "not_found"
    );
}

/// `last_activity_at` is the latest activity the caller can see: a private
/// note moves it for alice but not for a reader without private notes.
#[tokio::test]
async fn last_activity_ignores_hidden_notes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dave = oxledger::auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    let rep = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reporter','default') RETURNING id",
    )
    .await;
    grant(&pool, rep, &["view_issues"]).await;
    member(&pool, w.web, dave, rep).await;
    let d = AuthContext::test_user(dave);
    let r = submit(
        &w,
        vec![json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": w.web, "tracker_id": w.bug, "fields": {"subject": "看動態"}})],
    )
    .await;
    let key = r.effects[0].key.clone();
    let issue: Uuid = r.effects[0].issue_id;
    act(
        &w,
        issue,
        json!({"op_id": op(), "kind": "add_note", "target": tgt(issue), "body": "公開進度"}),
    )
    .await;
    act(&w, issue, json!({"op_id": op(), "kind": "add_note", "target": tgt(issue), "body": "內部討論", "is_private": true})).await;
    let at = |kind: &str| {
        let pool = pool.clone();
        let kind = kind.to_string();
        async move {
            sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
                "SELECT e.at FROM issue_events e LEFT JOIN issue_notes n ON n.id = e.note_id
                 WHERE e.issue_id = $1 AND e.kind = 'note_added' AND n.is_private = ($2 = 'private')",
            )
            .bind(issue)
            .bind(kind)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (t_pub, t_priv) = (at("public").await, at("private").await);
    assert!(t_priv >= t_pub);
    let p = json!({"key": key, "fields": ["last_activity_at"], "include": ["recent_events"]});
    let gd = get(&w, d, p.clone()).await.unwrap();
    let ga = get(&w, w.alice_ctx, p).await.unwrap();
    assert_eq!(gd["last_activity_at"], json!(t_pub));
    assert_eq!(ga["last_activity_at"], json!(t_priv));
    // recent_events: the private note's event is simply not there.
    let kinds = |g: &Value| {
        g["recent_events"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| s(&e["kind"]).to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(kinds(&gd), vec!["note_added", "created"]);
    assert_eq!(kinds(&ga), vec!["note_added", "note_added", "created"]);
    assert_eq!(gd["recent_events"]["truncated"], false);
}

// ---------------------------------------------------------------- §3.9 permissions (agent credentials)

#[tokio::test]
async fn agent_reads_only_the_delegated_scope() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    // Everything that changes members, roles or fields comes before the delegation.
    let secret = custom_fields::create_field(
        &pool,
        NewField {
            name: "Secret",
            ty: FieldType::String,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: false,
            roles: &[w.dev],
        },
    )
    .await
    .unwrap();
    let open = custom_fields::create_field(
        &pool,
        NewField {
            name: "Public",
            ty: FieldType::String,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible: true,
            roles: &[],
        },
    )
    .await
    .unwrap();
    let (pa, ka) = mk(&w, w.web, "登入 公開單", "登入的說明", false).await;
    let (_pp, kp) = mk(&w, w.web, "登入 私有單", "私有的說明", true).await;
    let (_pb, kb) = mk(&w, w.auth_p, "登入 AUTH 單", "AUTH 的說明", false).await;
    custom_fields::set_value(&pool, pa, secret, &CfValue::Text("機密值".into()))
        .await
        .unwrap();
    custom_fields::set_value(&pool, pa, open, &CfValue::Text("公開值".into()))
        .await
        .unwrap();
    note(&w, pa, "公開留言", false).await;
    note(&w, pa, "私有留言", true).await;
    sync(&w).await;

    // alice (SPA) reads everything: the data exists.
    let cite = |k: &str| {
        let k = k.to_string();
        let w = &w;
        async move {
            get(w, a, json!({"key": k, "fields": ["subject"], "citations": ["subject"], "include": ["recent_notes"]}))
                .await
                .unwrap()
        }
    };
    let ga = cite(&ka).await;
    let gp = cite(&kp).await;
    let gb = cite(&kb).await;
    let alice_cf = get(&w, a, json!({"key": ka, "fields": ["cf:Secret"]}))
        .await
        .unwrap();
    assert_eq!(alice_cf["custom_fields"][0]["value"], "機密值");
    let priv_note = ga["recent_notes"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["is_private"] == true)
        .unwrap()["citation_id"]
        .clone();
    let c_p = s(&gp["citations"]["subject"]).to_string();
    let c_b = s(&gb["citations"]["subject"]).to_string();
    let c_a = s(&ga["citations"]["subject"]).to_string();
    for (c, q) in [(&c_p, "登入 私有單"), (&c_b, "登入 AUTH 單")] {
        assert_eq!(check(&w, a, c, q, "exact").await["status"], "valid");
    }

    let web = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(web.context_id).await;

    // verify_citations / get_source: hand-made or human citations → not_found,
    // byte-identical to a source that does not exist.
    let ghost = check(&w, agent, &ghost_field(), "x", "exact").await;
    for (c, q) in [
        (c_p.clone(), "登入 私有單"),
        (c_b.clone(), "登入 AUTH 單"),
        (s(&priv_note).to_string(), "私有留言"),
    ] {
        let v = check(&w, agent, &c, q, "exact").await;
        assert_eq!(v.to_string(), ghost.to_string(), "{q}");
        let gs = source(&w, agent, json!({"citation_id": c, "quote": q}))
            .await
            .unwrap();
        let gg = source(&w, agent, json!({"citation_id": ghost_field(), "quote": q}))
            .await
            .unwrap();
        assert_eq!(gs.to_string(), gg.to_string());
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            citations::verify_one(&mut conn, agent, &c, q)
                .await
                .unwrap(),
            CitationStatus::NotFound
        );
    }
    assert_eq!(
        check(&w, agent, &c_a, "登入 公開單", "exact").await["status"],
        "valid"
    );

    // get / get_many: out of scope = not_found; private notes and hidden fields omitted.
    for k in [&kp, &kb] {
        let e = get(&w, agent, json!({"key": k, "fields": ["subject"]}))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
    }
    let gm = get_many(
        &w,
        agent,
        json!({"keys": [ka, kp, kb, "WEB-999"], "fields": ["subject"], "include": ["recent_notes", "recent_events"]}),
    )
    .await
    .unwrap();
    let outcomes: Vec<&str> = gm["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| s(&i["outcome"]))
        .collect();
    assert_eq!(outcomes, vec!["ok", "error", "error", "error"]);
    assert_eq!(
        gm["items"][1]["error"].to_string().replace(&kp, "K"),
        gm["items"][3]["error"].to_string().replace("WEB-999", "K")
    );
    let notes = gm["items"][0]["issue"]["recent_notes"]["items"]
        .as_array()
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["body"], "公開留言");
    assert!(!gm.to_string().contains("私有留言"));
    let e = get(&w, agent, json!({"key": ka, "fields": ["cf:Secret"]}))
        .await
        .unwrap_err();
    let d = oxledger::error::classify(&e).unwrap().details.unwrap();
    assert!(
        !d.to_string().contains("Secret"),
        "hidden field names are not listed: {d}"
    );
    assert!(
        d["allowed"]
            .as_array()
            .unwrap()
            .contains(&json!("cf:Public"))
    );
    let g = get(&w, agent, json!({"key": ka})).await.unwrap();
    assert!(!g.to_string().contains("機密值"), "{g}");
    assert!(g.to_string().contains("公開值"));

    // search: only the delegated, non-private issue.
    let r = search(&w, agent, json!({"q": "登入"})).await.unwrap();
    assert_eq!(keys(&r), vec![ka.clone()]);

    // Two projects: both readable; revoking AUTH removes only AUTH.
    let auth = w.delegate(w.auth_p).await.unwrap();
    let r = search(&w, agent, json!({"q": "登入"})).await.unwrap();
    let mut ks = keys(&r);
    ks.sort();
    assert_eq!(ks, {
        let mut v = vec![ka.clone(), kb.clone()];
        v.sort();
        v
    });
    assert_eq!(
        check(&w, agent, &c_b, "登入 AUTH 單", "exact").await["status"],
        "valid"
    );
    oxledger::agents::revoke_delegation(&pool, &w.alice_ctx, auth.id)
        .await
        .unwrap();
    assert_eq!(
        check(&w, agent, &c_b, "登入 AUTH 單", "exact").await["status"],
        "not_found"
    );
    assert_eq!(
        check(&w, agent, &c_a, "登入 公開單", "exact").await["status"],
        "valid"
    );

    // A security change in R: every read tool fails as a whole.
    sqlx::query("UPDATE issues SET is_private = true WHERE id = $1")
        .bind(pa)
        .execute(&pool)
        .await
        .unwrap();
    let errs = [
        search(&w, agent, json!({"q": "登入"})).await.unwrap_err(),
        get(&w, agent, json!({"key": ka})).await.unwrap_err(),
        get_many(&w, agent, json!({"keys": [ka]}))
            .await
            .unwrap_err(),
        verify(
            &w,
            agent,
            json!({"items": [{"citation_id": c_a, "quote": "登入 公開單"}]}),
        )
        .await
        .unwrap_err(),
        source(
            &w,
            agent,
            json!({"citation_id": c_a, "quote": "登入 公開單"}),
        )
        .await
        .unwrap_err(),
    ];
    for e in &errs {
        assert_eq!(code(e), ErrorCode::ContextUnavailable, "{e:#}");
    }

    // A new context; once its only delegation is revoked, the batch fails as a
    // whole instead of answering not_found per citation.
    sqlx::query("UPDATE issues SET is_private = false WHERE id = $1")
        .bind(pa)
        .execute(&pool)
        .await
        .unwrap();
    let ctx2 = w.new_context().await;
    let web2 = w.delegate(w.web).await.unwrap();
    assert_eq!(web2.context_id, ctx2);
    let (_, agent2) = w.token(ctx2).await;
    assert_eq!(
        check(&w, agent2, &c_a, "登入 公開單", "exact").await["status"],
        "valid"
    );
    oxledger::agents::revoke_delegation(&pool, &w.alice_ctx, web2.id)
        .await
        .unwrap();
    let e = verify(
        &w,
        agent2,
        json!({"items": [{"citation_id": c_a, "quote": "登入 公開單"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable);
    let e = source(
        &w,
        agent2,
        json!({"citation_id": c_a, "quote": "登入 公開單"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::DelegationUnavailable);
}

// ---------------------------------------------------------------- T-U8 and 用法 2 / 4

#[tokio::test]
async fn status_since_t_u8_and_usage_shapes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let a = w.alice_ctx;
    let r = submit(
        &w,
        vec![json!({"op_id": op(), "kind": "create_issue", "bind": "n", "project_id": w.web, "tracker_id": w.bug,
                    "fields": {"subject": "卡住的單", "description": "等 QA 重現", "assigned_to_id": w.bob}})],
    )
    .await;
    let key = r.effects[0].key.clone();
    let issue = r.effects[0].issue_id;
    let created: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM issues WHERE id = $1")
            .bind(issue)
            .fetch_one(&pool)
            .await
            .unwrap();
    let since = |g: &Value| (g["status_since"].clone(), g["status_since_known"].clone());
    let p = json!({"key": key, "fields": ["subject", "status", "assignee", "status_since", "last_activity_at"],
                   "include": ["recent_notes", "recent_events"], "note_limit": 5, "event_limit": 10});

    // Never changed: the creation time.
    let g = get(&w, a, p.clone()).await.unwrap();
    assert_eq!(since(&g), (json!(created), json!(true)));
    assert_eq!(g["assignee"], "bob");
    assert_eq!(g["assignee_kind"], "user");

    // A real transition moves it; a no_effect one does not.
    act(
        &w,
        issue,
        json!({"op_id": op(), "kind": "transition", "target": tgt(issue), "to_status_id": w.doing}),
    )
    .await;
    let moved_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT at FROM issue_events WHERE issue_id = $1 AND kind = 'status_changed'",
    )
    .bind(issue)
    .fetch_one(&pool)
    .await
    .unwrap();
    let g = get(&w, a, p.clone()).await.unwrap();
    assert_eq!(since(&g), (json!(moved_at), json!(true)));
    assert_eq!(g["status"], "Doing");
    let r = act(
        &w,
        issue,
        json!({"op_id": op(), "kind": "transition", "target": tgt(issue), "to_status_id": w.doing}),
    )
    .await;
    assert_eq!(r.items[0].status, "no_effect");
    act(
        &w,
        issue,
        json!({"op_id": op(), "kind": "add_note", "target": tgt(issue), "body": "還在等 QA"}),
    )
    .await;
    let g = get(&w, a, p.clone()).await.unwrap();
    assert_eq!(
        since(&g),
        (json!(moved_at), json!(true)),
        "no_effect transition does not count"
    );
    // 用法 4 shape: one call answers status, owner, since, activity and the latest note.
    assert_eq!(g["recent_notes"]["items"][0]["body"], "還在等 QA");
    assert!(g["recent_notes"]["items"][0]["citation_id"].is_string());
    let ev = g["recent_events"]["items"].as_array().unwrap();
    assert_eq!(ev[0]["kind"], "note_added");
    assert_eq!(ev[1]["kind"], "status_changed");
    assert_eq!(ev[1]["detail"]["new"]["name"], "Doing");
    assert_eq!(g["last_activity_at"], ev[0]["at"]);
    let note_cit = s(&g["recent_notes"]["items"][0]["citation_id"]).to_string();
    assert_eq!(
        check(&w, a, &note_cit, "還在等 QA", "exact").await["status"],
        "valid"
    );

    // The status event redacted: unknown, not guessed.
    let redaction: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_event_redactions (event_id, redacted_by, reason_code, reason)
         SELECT id, $2, 'test', 'test' FROM issue_events WHERE issue_id = $1 AND kind = 'status_changed'
         RETURNING id",
    )
    .bind(issue)
    .bind(w.alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE issue_events SET payload = '\"[已遮蔽]\"', redaction_id = $2
         WHERE issue_id = $1 AND kind = 'status_changed'",
    )
    .bind(issue)
    .bind(redaction)
    .execute(&pool)
    .await
    .unwrap();
    let g = get(&w, a, p.clone()).await.unwrap();
    assert_eq!(since(&g), (Value::Null, json!(false)));
    let ev = g["recent_events"]["items"].as_array().unwrap();
    assert_eq!(ev[1]["redacted"], true);
    assert!(ev[1].get("detail").is_none());

    // No trustworthy history at all (row written outside the command engine).
    let (_, k2) = mk(&w, w.web, "沒有歷史", "", false).await;
    let g = get(&w, a, json!({"key": k2, "fields": ["status_since"]}))
        .await
        .unwrap();
    assert_eq!(since(&g), (Value::Null, json!(false)));

    // 用法 2: get with actions, search with lock_version, get_many for detail.
    let g = get(
        &w,
        a,
        json!({"key": key, "fields": ["subject", "description", "tracker", "lock_version"], "include": ["actions"]}),
    )
    .await
    .unwrap();
    assert_eq!(g["description"], "等 QA 重現");
    assert!(g["lock_version"].is_number());
    assert!(g["actions"]["duplicate_of"].is_object(), "{g}");
    let (_, k3) = mk(&w, w.web, "QA 重現失敗", "QA 重現步驟", false).await;
    sync(&w).await;
    let r = search(
        &w,
        a,
        json!({"q": "QA 重現", "project": "WEB", "tracker": "Bug", "limit": 6,
               "fields": ["key", "subject", "status", "lock_version"], "explain": "brief"}),
    )
    .await
    .unwrap();
    let it = item(&r, &k3);
    assert!(it["lock_version"].is_number() && it["evidence"][0]["citation_id"].is_string());
    let gm = get_many(
        &w,
        a,
        json!({"keys": [key, k3], "fields": ["description", "lock_version"]}),
    )
    .await
    .unwrap();
    assert_eq!(gm["items"][1]["issue"]["description"], "QA 重現步驟");
    assert!(
        gm["items"][0]["issue"].get("subject").is_none(),
        "only the requested fields"
    );
    // Without fields, get keeps the full current detail.
    let full = get(&w, a, json!({"key": key})).await.unwrap();
    assert!(full["allowed_statuses"].is_array() && full["description"].is_string());
    // Parameter limits.
    for bad in [
        json!({"key": key, "include": ["history"]}),
        json!({"key": key, "include": ["recent_notes"], "note_limit": 11}),
        json!({"key": key, "include": ["recent_events"], "event_limit": 21}),
        json!({"key": key, "fields": ["status_since", "nope"]}),
    ] {
        assert_eq!(
            code(&get(&w, a, bad).await.unwrap_err()),
            ErrorCode::InvalidArgument
        );
    }
    let many: Vec<String> = (0..11).map(|i| format!("WEB-{i}")).collect();
    assert_eq!(
        code(&get_many(&w, a, json!({"keys": many})).await.unwrap_err()),
        ErrorCode::InvalidArgument
    );
}

// ---------------------------------------------------------------- §3.7 error table

#[test]
fn design_03_error_codes() {
    use NextAction::*;
    for (c, status, retryable, next) in [
        (ErrorCode::InvalidQuery, 400, false, vec![FixInput]),
        (ErrorCode::InvalidFilter, 400, false, vec![FixInput]),
        (
            ErrorCode::AmbiguousReference,
            422,
            false,
            vec![FixInput, AskHuman],
        ),
        (ErrorCode::InvalidCursor, 400, false, vec![FixInput]),
        (ErrorCode::InvalidArgument, 400, false, vec![FixInput]),
        (ErrorCode::InvalidCitation, 400, false, vec![FixInput]),
        (ErrorCode::NotFound, 404, false, vec![FixInput, Stop]),
        (ErrorCode::SearchUnavailable, 503, true, vec![WaitAndRetry]),
        (
            ErrorCode::ContextUnavailable,
            409,
            false,
            vec![AskHuman, Stop],
        ),
    ] {
        assert_eq!(c.http_status(), status, "{}", c.as_str());
        assert_eq!(c.retryable(), retryable, "{}", c.as_str());
        assert_eq!(c.next_actions(), next.as_slice(), "{}", c.as_str());
    }
    // Per-item statuses (§3.7 second table).
    for (st, next) in [
        (CitationStatus::Valid, vec![]),
        (CitationStatus::Moved, vec![FixInput]),
        (CitationStatus::SourceChanged, vec![FixInput]),
        (CitationStatus::CitationMismatch, vec![FixInput]),
        (CitationStatus::Ambiguous, vec![FixInput, AskHuman]),
        (CitationStatus::NotFound, vec![FixInput, Stop]),
        (CitationStatus::InvalidCitation, vec![FixInput]),
    ] {
        assert_eq!(st.next_actions(), next.as_slice(), "{}", st.as_str());
    }
}
