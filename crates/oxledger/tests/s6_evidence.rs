// SPDX-License-Identifier: GPL-2.0-or-later
//! Contract 02 §6.7 step 7: proposal evidence is verified exactly in the
//! agent's read scope (design 03 §3.6). T-M14, I-3.
//! Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::changesets::{self, faults};
use oxledger::error::{ErrorCode, classify};
use oxledger::issue_view;
use serde_json::{Value, json};
use uuid::Uuid;
use world::*;

async fn set_desc(w: &World, i: Uuid, d: &str) {
    sqlx::query("UPDATE issues SET description = $2 WHERE id = $1")
        .bind(i)
        .bind(d)
        .execute(&w.pool)
        .await
        .unwrap();
}

fn forged(issue: Uuid) -> String {
    let body = json!({"v": 1, "src": {"kind": "issue_note", "issue_id": issue, "note_id": Uuid::now_v7()},
                      "rev": oxledger::auth::encode(&[0u8; 32]), "start": 0, "end": 3});
    format!(
        "cit1.{}",
        oxledger::auth::encode(body.to_string().as_bytes())
    )
}

#[tokio::test]
async fn s6_evidence() {
    let pool = test_pool().await;
    faults::clear();
    let w = World::build(&pool).await;
    let (i1, i2, i3) = (
        w.issue(w.web, "one").await,
        w.issue(w.web, "two").await,
        w.issue(w.web, "three").await,
    );
    for (i, d) in [
        (i1, "登入後被導回登入頁"),
        (i2, "Safari 才會"),
        (i3, "公司入口"),
    ] {
        set_desc(&w, i, d).await;
    }
    let private_note: Uuid = sqlx::query_scalar(
        "INSERT INTO issue_notes (issue_id, author_id, body, is_private) VALUES ($1,$2,'secret',true) RETURNING id",
    )
    .bind(i1)
    .bind(w.bob)
    .fetch_one(&pool)
    .await
    .unwrap();
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let mut cits = Vec::new();
    for i in [i1, i2, i3] {
        let key: String =
            sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
                .bind(i)
                .fetch_one(&pool)
                .await
                .unwrap();
        let g = issue_view::get(
            &w.app,
            agent,
            serde_json::from_value(
                json!({"key": key, "fields": ["description"], "citations": ["description"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let g = serde_json::to_value(g).unwrap();
        cits.push(g["citations"]["description"].as_str().unwrap().to_string());
    }
    let lv: i32 = sqlx::query_scalar("SELECT lock_version FROM issues WHERE id = $1")
        .bind(i1)
        .fetch_one(&pool)
        .await
        .unwrap();
    let propose = |evidence: Value| {
        let doc = json!({"command_schema_version": 1, "title": "t",
            "ops": [{"op_id": op(), "kind": "add_note", "target": tgt(i1), "body": "n"}],
            "preconditions": [{"target": tgt(i1), "lock_version": lv}],
            "evidence": evidence});
        let app = w.app.clone();
        async move { changesets::propose(&app, agent, Uuid::now_v7(), &doc, ORIGIN).await }
    };
    // All valid: stored.
    propose(json!([{"citation_id": cits[0], "quote": "登入後被導回登入頁"}]))
        .await
        .unwrap();
    let before = count(&pool, "SELECT count(*) FROM changesets").await;

    // T-M14: the 2nd quote is wrong, the 3rd source changed.
    set_desc(&w, i3, "公司入口（已改）").await;
    let e = propose(json!([
        {"citation_id": cits[0], "quote": "登入後被導回登入頁"},
        {"citation_id": cits[1], "quote": "Chrome 才會"},
        {"citation_id": cits[2], "quote": "公司入口"},
    ]))
    .await
    .unwrap_err();
    let a = classify(&e).unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(a.code, ErrorCode::SourceUnverifiable);
    assert_eq!(
        a.details.unwrap()["items"],
        json!([{"index": 1, "status": "citation_mismatch"}, {"index": 2, "status": "source_changed"}])
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM changesets").await,
        before,
        "not stored"
    );

    // I-3: an invented citation, one to a private note, garbage.
    let note_cit = {
        let body = json!({"v": 1, "src": {"kind": "issue_note", "issue_id": i1, "note_id": private_note},
                          "rev": oxledger::auth::encode(&[0u8; 32]), "start": 0, "end": 3});
        format!(
            "cit1.{}",
            oxledger::auth::encode(body.to_string().as_bytes())
        )
    };
    let e = propose(json!([
        {"citation_id": forged(i1), "quote": "abc"},
        {"citation_id": note_cit, "quote": "sec"},
        {"citation_id": "not-a-citation", "quote": "x"},
    ]))
    .await
    .unwrap_err();
    let a = classify(&e).unwrap();
    assert_eq!(a.code, ErrorCode::SourceUnverifiable);
    assert_eq!(
        a.details.unwrap()["items"],
        json!([{"index": 0, "status": "not_found"}, {"index": 1, "status": "not_found"},
               {"index": 2, "status": "invalid_citation"}])
    );
    // §8.9: at most 20 evidence references.
    let many: Vec<Value> = (0..21)
        .map(|_| json!({"citation_id": cits[0], "quote": "登入後被導回登入頁"}))
        .collect();
    let e = propose(json!(many)).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
    faults::clear();
}
