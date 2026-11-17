// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.7 (§3.9, R-E1–R-E11): CSV export. T-M2-80, 81, 82, 83, 84,
//! 85, 86, 86a and the audit list. Decision A4 (a): who can read can
//! export. Needs PostgreSQL 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use ox_core::filter::{FieldType, Value as FValue};
use oxledger::auth::{self, AuthContext};
use oxledger::commands;
use oxledger::custom_fields::{self, NewField};
use oxledger::error::{ErrorCode, classify};
use oxledger::exports::{self, IssueExport, LIMIT_BYTES, LIMIT_PER_MINUTE, LIMIT_ROWS, TimeExport};
use oxledger::test_support::{NewIssue, create_issue};
use oxledger::time_reports::{self, ListQuery};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use uuid::Uuid;
use world::*;

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Generous limits unless a test narrows one (each test is its own process).
fn limits(rows: usize, bytes: u64, per_minute: usize) {
    LIMIT_ROWS.store(rows, Ordering::Relaxed);
    LIMIT_BYTES.store(bytes, Ordering::Relaxed);
    LIMIT_PER_MINUTE.store(per_minute, Ordering::Relaxed);
}

fn in_project(project: &str) -> IssueExport {
    IssueExport {
        project_id: Some(project.into()),
        ..Default::default()
    }
}

/// The file's bytes.
async fn file(w: &World, who: AuthContext, input: &IssueExport) -> Vec<u8> {
    exports::issues(w.app.clone(), who, input)
        .await
        .unwrap_or_else(|e| panic!("export: {e:#}"))
        .collect()
        .await
        .expect("a whole file")
}

/// The file read back with the `csv` crate: header, then rows.
fn parse(bytes: &[u8]) -> Vec<Vec<String>> {
    assert!(bytes.starts_with(BOM), "UTF-8 with BOM");
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(&bytes[BOM.len()..]);
    r.records()
        .map(|rec| rec.unwrap().iter().map(String::from).collect())
        .collect()
}

async fn subject_issue(
    w: &World,
    project: Uuid,
    subject: &str,
    description: &str,
    private: bool,
) -> (Uuid, String) {
    create_issue(
        &w.pool,
        NewIssue {
            project_id: project,
            tracker_id: w.bug,
            status_id: w.new,
            subject,
            description,
            author_id: w.bob,
            assigned_to_id: None,
            is_private: private,
            parent_id: None,
        },
    )
    .await
    .unwrap()
}

async fn field(w: &World, name: &str, ty: FieldType, visible: bool, roles: &[Uuid]) -> i32 {
    let f = custom_fields::create_field(
        &w.pool,
        NewField {
            name,
            ty,
            description: "",
            is_required: false,
            is_for_all: true,
            trackers: &[w.bug],
            projects: &[],
            options: &[],
            visible,
            roles,
        },
    )
    .await
    .unwrap();
    custom_fields::reconcile_indexes(&w.pool).await.unwrap();
    f
}

/// The newest audit row, waited for (the writer records after the last byte).
async fn last_event(w: &World, more_than: i64) -> Value {
    for _ in 0..200 {
        if count(&w.pool, "SELECT count(*) FROM export_events").await > more_than {
            let v: Value = sqlx::query_scalar(
                "SELECT to_jsonb(e) - 'filter_hash' || jsonb_build_object('hash', encode(e.filter_hash, 'hex'))
                 FROM export_events e ORDER BY id DESC LIMIT 1",
            )
            .fetch_one(&w.pool)
            .await
            .unwrap();
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no export_events row");
}

// ---------------------------------------------------------------- T-M2-80, 84

#[tokio::test]
async fn csv_that_a_spreadsheet_reads_safely() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    limits(0, 0, 1000);
    let size = field(&w, "Size", FieldType::Int, true, &[]).await;
    let risky = [
        "=HYPERLINK(\"http://evil.example\",\"x\")",
        "+1",
        "-cmd",
        "@SUM(A1)",
        "＝1",
        "\tTab first",
    ];
    let mut keys = Vec::new();
    for s in risky {
        keys.push(subject_issue(&w, w.web, s, "", false).await.1);
    }
    let (plain, plain_key) = subject_issue(
        &w,
        w.web,
        "中文，逗號,和 \"引號\"",
        "第一行\n第二行, 有逗號\r\n=不是公式的第三行",
        false,
    )
    .await;
    custom_fields::set_value(&pool, plain, size, &FValue::Num(-5.0))
        .await
        .unwrap();

    let bytes = file(
        &w,
        w.alice_ctx,
        &IssueExport {
            columns: Some(vec![
                "key".into(),
                "subject".into(),
                format!("cf:{size}"),
                "is_private".into(),
                "created_at".into(),
                "author".into(),
            ]),
            include_description: true,
            ..in_project("WEB")
        },
    )
    .await;
    // T-M2-84: BOM, CRLF row ends, RFC 4180 quoting; the csv crate reads it back.
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.ends_with("\r\n"));
    let rows = parse(&bytes);
    assert_eq!(
        rows[0],
        [
            "單號",
            "標題",
            "Size",
            &rows[0][3],
            &rows[0][4],
            &rows[0][5],
            "說明"
        ]
    );
    assert_eq!(rows.len(), 1 + risky.len() + 1);
    let by_key = |k: &str| rows.iter().find(|r| r[0] == k).unwrap().clone();
    // T-M2-80: a text cell that would be a formula gets an apostrophe.
    for (k, s) in keys.iter().zip(risky) {
        assert_eq!(by_key(k)[1], format!("'{s}"), "{s:?}");
    }
    let r = by_key(&plain_key);
    assert_eq!(r[1], "中文，逗號,和 \"引號\"");
    // A number oxledger wrote is not guarded.
    assert_eq!(r[2], "-5");
    assert_eq!(r[3], "否");
    // U42: ISO 8601 with the organisation's offset.
    assert!(
        r[4].len() == 25
            && r[4].as_bytes()[10] == b'T'
            && matches!(r[4].as_bytes()[19], b'+' | b'-'),
        "{}",
        r[4]
    );
    assert_eq!(r[5], "bob");
    assert_eq!(r[6], "第一行\n第二行, 有逗號\r\n=不是公式的第三行");
    // The description is text: guarded on its first character only.
    let (_, guarded) = subject_issue(&w, w.web, "d", "=1+1", false).await;
    let bytes = file(
        &w,
        w.alice_ctx,
        &IssueExport {
            columns: Some(vec!["key".into()]),
            include_description: true,
            ..in_project("WEB")
        },
    )
    .await;
    let rows = parse(&bytes);
    assert_eq!(rows.iter().find(|r| r[0] == guarded).unwrap()[1], "'=1+1");
}

// ---------------------------------------------------------------- T-M2-81, 82

#[tokio::test]
async fn an_export_holds_what_its_reader_sees() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    limits(0, 0, 1000);
    // dave: public issues and his own, no restricted field.
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    let dave = auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, dave, viewer).await;
    let dave = sign_in(&pool, &w.policy, "dave").await;
    let secret = field(&w, "Margin", FieldType::String, false, &[w.dev]).await;

    let (open, open_key) = subject_issue(&w, w.web, "open issue", "", false).await;
    let (_, private_key) = subject_issue(
        &w,
        w.web,
        "CANARY-PRIVATE-SUBJECT",
        "CANARY-PRIVATE-BODY",
        true,
    )
    .await;
    custom_fields::set_value(&pool, open, secret, &FValue::Text("CANARY-MARGIN".into()))
        .await
        .unwrap();
    // A note: never exported (B-25).
    commands::submit(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &note_doc(open, 0, "CANARY-NOTE"),
    )
    .await
    .unwrap();

    // T-M2-82: the same conditions, two readers.
    let whole = IssueExport {
        include_description: true,
        ..in_project("WEB")
    };
    let a = String::from_utf8(file(&w, w.alice_ctx, &whole).await).unwrap();
    let d = String::from_utf8(file(&w, dave, &whole).await).unwrap();
    assert!(a.contains("CANARY-PRIVATE-SUBJECT") && a.contains("CANARY-PRIVATE-BODY"));
    assert!(a.contains(&private_key) && a.contains(&open_key));
    assert!(d.contains(&open_key));
    for canary in [
        "CANARY-PRIVATE",
        &private_key,
        "CANARY-MARGIN",
        "CANARY-NOTE",
    ] {
        assert!(!d.contains(canary), "{canary}");
    }
    assert!(!a.contains("CANARY-NOTE"));

    // T-M2-81: the restricted field, asked for by name.
    let with = |c: String| IssueExport {
        columns: Some(vec!["key".into(), c]),
        ..in_project("WEB")
    };
    let a = parse(&file(&w, w.alice_ctx, &with(format!("cf:{secret}"))).await);
    assert_eq!(a[0], ["單號", "Margin"]);
    assert!(
        a.iter()
            .any(|r| r[0] == open_key && r[1] == "CANARY-MARGIN")
    );
    let refused = |e: anyhow::Error| {
        let a = classify(&e).unwrap();
        (a.code, a.message.clone(), a.details.clone())
    };
    let hidden = refused(
        exports::issues(w.app.clone(), dave, &with(format!("cf:{secret}")))
            .await
            .err()
            .unwrap(),
    );
    let missing = refused(
        exports::issues(w.app.clone(), dave, &with("cf:987654".into()))
            .await
            .err()
            .unwrap(),
    );
    assert_eq!(hidden, missing, "one answer for unreadable and unknown");
    assert_eq!(hidden.0, ErrorCode::InvalidArgument);
    assert_eq!(
        hidden.2,
        Some(json!({"path": "columns[1]", "reason": "unknown_field"}))
    );
    // A project dave is not in: as if it did not exist.
    let err = exports::issues(w.app.clone(), dave, &in_project("AUTH"))
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::NotFound);

    // T-M2-81: one saved view with a column that was restricted after the
    // view was saved (a project view cannot be saved with one), two
    // readers. Her file has the column; his has neither it nor its value.
    grant(&pool, w.dev, &["manage_public_views"]).await;
    let secret = field(&w, "Cost", FieldType::String, true, &[w.dev]).await;
    custom_fields::set_value(&pool, open, secret, &FValue::Text("CANARY-COST".into()))
        .await
        .unwrap();
    let shared = oxledger::views::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &oxledger::views::NewView {
            scope: "project".into(),
            project_id: Some("WEB".into()),
            kind: "list".into(),
            name: "with margin".into(),
            def: json!({"columns": ["key", format!("cf:{secret}")]}),
        },
    )
    .await
    .unwrap()
    .id;
    sqlx::query("UPDATE field_defs SET visible = false WHERE id = $1")
        .bind(secret)
        .execute(&pool)
        .await
        .unwrap();
    let by_view = IssueExport {
        view_id: Some(shared.to_string()),
        ..Default::default()
    };
    let a = parse(&file(&w, w.alice_ctx, &by_view).await);
    assert_eq!(a[0], ["單號", "Cost"]);
    assert!(a.iter().any(|r| r[0] == open_key && r[1] == "CANARY-COST"));
    let d = file(&w, dave, &by_view).await;
    assert_eq!(parse(&d)[0], ["單號"]);
    let d = String::from_utf8(d).unwrap();
    assert!(d.contains(&open_key));
    assert!(!d.contains("CANARY") && !d.contains("Cost") && !d.contains(&private_key));

    // Someone else's personal view answers like one that does not exist.
    let own = oxledger::views::create(
        &w.app,
        w.alice_ctx,
        Uuid::now_v7(),
        &oxledger::views::NewView {
            scope: "personal".into(),
            project_id: None,
            kind: "list".into(),
            name: "alice's own".into(),
            def: json!({"columns": ["key"]}),
        },
    )
    .await
    .unwrap()
    .id;
    let by = |v: String| IssueExport {
        view_id: Some(v),
        ..Default::default()
    };
    let theirs = refused(
        exports::issues(w.app.clone(), dave, &by(own))
            .await
            .err()
            .unwrap(),
    );
    let nothing = refused(
        exports::issues(w.app.clone(), dave, &by(Uuid::now_v7().to_string()))
            .await
            .err()
            .unwrap(),
    );
    assert_eq!(theirs, nothing);
    assert_eq!(theirs.0, ErrorCode::NotFound);

    // R-E8: on record before the first byte, whether or not anyone reads
    // the file to its end.
    let before = count(&pool, "SELECT count(*) FROM export_events").await;
    let started = exports::issues(w.app.clone(), w.alice_ctx, &in_project("WEB"))
        .await
        .unwrap();
    assert_eq!(
        count(&pool, "SELECT count(*) FROM export_events").await,
        before + 1
    );
    drop(started);
}

// ---------------------------------------------------------------- T-M2-83, 86a

#[tokio::test]
async fn limits_refuse_before_the_first_byte_or_cut_the_file() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    for _ in 0..4 {
        w.issue(w.web, "one of four").await;
    }
    // T-M2-83: one row too many; the answer never says how many there are.
    limits(3, 0, 1000);
    let err = exports::issues(w.app.clone(), w.alice_ctx, &in_project("WEB"))
        .await
        .err()
        .unwrap();
    let a = classify(&err).unwrap();
    assert_eq!(a.code, ErrorCode::ExportTooLarge);
    assert_eq!(a.details, Some(json!({"limit": 3})));
    assert_eq!(count(&pool, "SELECT count(*) FROM export_events").await, 0);
    limits(4, 0, 1000);
    assert_eq!(
        parse(&file(&w, w.alice_ctx, &in_project("WEB")).await).len(),
        5
    );
    let before = count(&pool, "SELECT count(*) FROM export_events").await;

    // T-M2-86a: past the byte limit the file is cut, and the audit says so.
    limits(0, 120, 1000);
    let cut = exports::issues(w.app.clone(), w.alice_ctx, &in_project("WEB"))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        cut.is_err(),
        "an incomplete file is an error, not a short file"
    );
    let ev = last_event(&w, before).await;
    assert_eq!(ev["truncated"], true);
    // What was sent, not what would have been: settled after the cut.
    let mut sent = ev["bytes"].as_i64().unwrap();
    for _ in 0..100 {
        if sent > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sent = last_event(&w, before).await["bytes"].as_i64().unwrap();
    }
    assert!(sent > 0 && sent <= 120, "{sent}");

    // One export at a time for a person; another person is not held up.
    limits(0, 0, 1000);
    for _ in 0..450 {
        w.issue(w.web, "filler").await;
    }
    let slow = exports::issues(w.app.clone(), w.alice_ctx, &in_project("WEB"))
        .await
        .unwrap();
    let err = exports::issues(w.app.clone(), w.alice_ctx, &in_project("WEB"))
        .await
        .err()
        .unwrap();
    let a = classify(&err).unwrap();
    assert_eq!(a.code, ErrorCode::RateLimited);
    assert_eq!(a.details, Some(json!({"reason": "export_running"})));
    let bob = sign_in(&pool, &w.policy, "bob").await;
    assert!(
        exports::issues(w.app.clone(), bob, &in_project("WEB"))
            .await
            .unwrap()
            .collect()
            .await
            .is_ok()
    );
    assert_eq!(parse(&slow.collect().await.unwrap()).len(), 1 + 454);
    // The place is free again once the file is out.
    let mut freed = false;
    for _ in 0..200 {
        match exports::issues(w.app.clone(), w.alice_ctx, &in_project("AUTH")).await {
            Ok(e) => {
                e.collect().await.unwrap();
                freed = true;
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    assert!(freed);

    // Five a minute (here: two).
    let carol = sign_in(&pool, &w.policy, "carol").await;
    limits(0, 0, 2);
    for _ in 0..2 {
        file(&w, carol, &in_project("WEB")).await;
        // The writer frees the place after its last byte.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let err = exports::issues(w.app.clone(), carol, &in_project("WEB"))
        .await
        .err()
        .unwrap();
    let a = classify(&err).unwrap();
    assert_eq!(a.code, ErrorCode::RateLimited);
    assert_eq!(a.details, Some(json!({"reason": "export_rate"})));
    assert!(a.retry_after_ms.is_some());
}

// ---------------------------------------------------------------- T-M2-85, 86

#[tokio::test]
async fn every_export_is_on_record_and_only_people_export() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    limits(0, 0, 1000);
    w.issue(w.web, "one").await;
    w.issue(w.web, "two").await;
    let input = IssueExport {
        where_: Some(json!({"field": "status", "op": "eq", "value": w.new.to_string()})),
        columns: Some(vec!["key".into(), "subject".into()]),
        ..in_project("WEB")
    };
    let bytes = file(&w, w.alice_ctx, &input).await;
    let ev = last_event(&w, 0).await;
    assert_eq!(ev["kind"], "issues");
    assert_eq!(ev["actor_id"], json!(w.alice));
    assert_eq!(ev["project_id"], json!(w.web));
    assert_eq!(ev["columns"], json!(["key", "subject"]));
    assert_eq!(ev["row_count"], 2);
    assert_eq!(ev["bytes"], bytes.len());
    assert_eq!(ev["truncated"], false);
    let hash = ev["hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 64);
    // Neither the conditions nor a value of them is stored anywhere in the row.
    assert!(!ev.to_string().contains(&w.new.to_string()) && !ev.to_string().contains("status"));
    // The same conditions give the same hash; another secret, another hash.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    file(&w, w.alice_ctx, &input).await;
    assert_eq!(last_event(&w, 1).await["hash"], hash);
    exec(
        &pool,
        "UPDATE server_secrets SET secret = sha256(secret) WHERE name = 'export_filter'",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    file(&w, w.alice_ctx, &input).await;
    assert_ne!(last_event(&w, 2).await["hash"], hash);

    // The audit list: system administrators only.
    let err = exports::events(&w.app, w.alice_ctx, None)
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);
    auth::create_user(&pool, "root", "root", PW, true)
        .await
        .unwrap();
    let root = sign_in(&pool, &w.policy, "root").await;
    let list = exports::events(&w.app, root, None).await.unwrap();
    assert_eq!(list.items.len(), 3);
    assert_eq!(list.items[0].actor.name, "alice");
    assert_eq!(list.items[0].project.as_deref(), Some("WEB"));
    assert!(list.next_cursor.is_none());

    // T-M2-86: no agent, nobody anonymous.
    let ctx = w.delegate(w.web).await.unwrap().context_id;
    let agent = w.token(ctx).await.1;
    for who in [agent, AuthContext::anonymous()] {
        let err = exports::issues(w.app.clone(), who, &in_project("WEB"))
            .await
            .err()
            .unwrap();
        assert_eq!(code(&err), ErrorCode::Unauthenticated);
        let err = exports::time_entries(w.app.clone(), who, &TimeExport::default())
            .await
            .err()
            .unwrap();
        assert_eq!(code(&err), ErrorCode::Unauthenticated);
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM export_events").await, 3);
}

// ---------------------------------------------------------------- a view; time entries

#[tokio::test]
async fn a_view_and_the_time_report_export_too() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    limits(0, 0, 1000);
    grant(&pool, w.dev, &["view_time_entries", "log_time"]).await;
    // Time tracking is on; everyone sees only their own entries.
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    exec(&pool, "UPDATE roles SET time_entries_visibility = 'own'").await;
    let i = w.issue(w.web, "timed").await;
    w.issue(w.auth_p, "elsewhere").await;
    // A built-in view runs with the caller's sight; its own columns.
    let bytes = file(
        &w,
        w.alice_ctx,
        &IssueExport {
            view_id: Some("builtin:involving_me".into()),
            ..Default::default()
        },
    )
    .await;
    let rows = parse(&bytes);
    assert_eq!(rows[0][0], "單號");
    assert!(rows[0].len() > 2);
    // view_id and conditions do not mix.
    let err = exports::issues(
        w.app.clone(),
        w.alice_ctx,
        &IssueExport {
            view_id: Some("builtin:involving_me".into()),
            project_id: Some("WEB".into()),
            ..Default::default()
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);

    let act = id(
        &pool,
        "SELECT id FROM time_entry_activities ORDER BY is_default DESC LIMIT 1",
    )
    .await;
    for (who, hours, comment) in [
        (w.alice, "1.50", "=cmd|' /C calc'!A0"),
        (w.alice, "0.25", "寫測試, 兩個欄位"),
        (w.bob, "7.25", "bob's"),
    ] {
        sqlx::query(
            "INSERT INTO time_entries (project_id, issue_id, user_id, author_id, activity_id, spent_on, hours, comments)
             VALUES ($1, $2, $3, $3, $4, '2026-11-05', $5::numeric, $6)",
        )
        .bind(w.web)
        .bind(i)
        .bind(who)
        .bind(act)
        .bind(hours)
        .bind(comment)
        .execute(&pool)
        .await
        .unwrap();
    }
    let input = TimeExport {
        project: Some("WEB".into()),
        ..Default::default()
    };
    let before = count(&pool, "SELECT count(*) FROM export_events").await;
    let e = exports::time_entries(w.app.clone(), w.alice_ctx, &input)
        .await
        .unwrap();
    assert!(e.filename.starts_with("WEB-time-entries-") && e.filename.ends_with(".csv"));
    let rows = parse(&e.collect().await.unwrap());
    assert_eq!(
        rows[0],
        [
            "日期",
            "專案",
            "單號",
            "標題",
            "使用者",
            "活動",
            "時數",
            "說明"
        ]
    );
    // Exactly what the list shows her.
    let seen = time_reports::list(
        &w.app,
        w.alice_ctx,
        &ListQuery {
            project: Some("WEB".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len() - 1, seen.items.len());
    assert_eq!(seen.items.len(), 2);
    // bob's entry is not hers to see, so it is not in her file.
    assert!(!rows.iter().any(|r| r[6] == "7.25" || r[7] == "bob's"));
    let mine = rows.iter().find(|r| r[6] == "1.50").unwrap();
    assert_eq!(mine[0], "2026-11-05");
    assert_eq!(mine[1], "WEB");
    assert_eq!(mine[3], "timed");
    assert_eq!(mine[7], "'=cmd|' /C calc'!A0");
    assert!(rows.iter().any(|r| r[7] == "寫測試, 兩個欄位"));
    let ev = last_event(&w, before).await;
    assert_eq!(ev["kind"], "time_entries");
    assert_eq!(ev["row_count"], seen.items.len());

    // The row limit holds here too.
    limits(1, 0, 1000);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let err = exports::time_entries(w.app.clone(), w.alice_ctx, &input)
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::ExportTooLarge);

    // A wiki download leaves its row.
    exports::wiki_downloaded(&w.app, w.alice, w.web, 12)
        .await
        .unwrap();
    let ev = last_event(&w, before + 1).await;
    assert_eq!(ev["kind"], "wiki_page");
    assert_eq!(ev["bytes"], 12);
}
