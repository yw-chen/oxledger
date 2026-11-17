// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.8 (§3.10, R-I1–R-I15): CSV import. T-M2-87, 88, 89, 91,
//! 92, 93, 94, 95, 97, 98 (T-M2-90 is in `m1_11_gate`). Needs PostgreSQL
//! 18; see `common::test_pool`.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use common::test_pool;
use oxledger::auth::{self, AuthContext};
use oxledger::commands;
use oxledger::error::{ErrorCode, classify};
use oxledger::imports::{
    self, ImportCloseInput, ImportRow, ImportRunInput, ImportSettingsInput, ImportStatus,
};
use serde_json::json;
use uuid::Uuid;
use world::*;

async fn upload(w: &World, who: AuthContext, kind: &str, file: &[u8]) -> imports::ImportDetail {
    imports::upload(&w.app, who, "WEB", kind, file)
        .await
        .unwrap_or_else(|e| panic!("upload: {e:#}"))
}

/// The settings as uploaded (the suggested mapping), for a test to change.
fn settings(d: &imports::ImportDetail) -> ImportSettingsInput {
    ImportSettingsInput {
        encoding: None,
        delimiter: None,
        date_format: d.status.settings.date_format.clone(),
        mapping: d.status.settings.mapping.clone(),
        value_map: d.status.settings.value_map.clone(),
        options: d.status.settings.options.clone().into(),
        lock_version: d.status.lock_version,
    }
}

async fn dry(w: &World, who: AuthContext, id: Uuid) -> ImportStatus {
    imports::validate(&w.app, who, &id.to_string())
        .await
        .unwrap_or_else(|e| panic!("validate: {e:#}"));
    imports::work_all(&w.app).await.unwrap();
    let s = imports::get(&w.app, who, &id.to_string()).await.unwrap();
    assert_eq!(s.state, "validated");
    s
}

async fn start(w: &World, who: AuthContext, id: Uuid) {
    let s = imports::get(&w.app, who, &id.to_string()).await.unwrap();
    imports::run(
        &w.app,
        who,
        &id.to_string(),
        Uuid::now_v7(),
        &ImportRunInput {
            lock_version: s.lock_version,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("run: {e:#}"));
}

async fn go(w: &World, who: AuthContext, id: Uuid) -> ImportStatus {
    start(w, who, id).await;
    imports::work_all(&w.app).await.unwrap();
    let s = imports::get(&w.app, who, &id.to_string()).await.unwrap();
    assert_eq!(s.state, "done");
    s
}

async fn rows(w: &World, who: AuthContext, id: Uuid) -> Vec<ImportRow> {
    imports::rows(&w.app, who, &id.to_string(), None, None)
        .await
        .unwrap()
        .items
}

fn outcome(r: &ImportRow) -> (&str, Option<&str>, Option<&str>) {
    (
        r.state.as_str(),
        r.error_code.as_deref(),
        r.error_field.as_deref(),
    )
}

async fn world(pool: &sqlx::PgPool) -> World {
    let w = World::build(pool).await;
    grant(pool, w.dev, &["import_issues", "import_time_entries"]).await;
    w
}

/// T-M2-87, T-M2-88: encodings, delimiters, a line break inside quotes.
#[tokio::test]
async fn t_m2_87_encodings_and_delimiters() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    let text = "標題,說明\n登入失敗,第一列\n\"兩行\n的標題\",第二列\n";

    let d = upload(&w, w.alice_ctx, "issues", text.as_bytes()).await;
    assert_eq!(
        (d.status.encoding.as_str(), d.status.encoding_guessed),
        ("utf-8", false)
    );
    assert_eq!(d.header, ["標題", "說明"]);
    assert_eq!(d.status.row_count, 2);
    assert_eq!(d.preview[1][0], "兩行\n的標題");
    // The export's own headers are recognised.
    assert_eq!(
        d.suggested,
        [Some("subject".to_string()), Some("description".to_string())]
    );
    assert_eq!(d.status.state, "uploaded");

    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend(text.as_bytes());
    let d = upload(&w, w.alice_ctx, "issues", &bom).await;
    assert_eq!(d.header[0], "標題");

    let mut le = b"\xFF\xFE".to_vec();
    for u in text.encode_utf16() {
        le.extend(u.to_le_bytes());
    }
    let d = upload(&w, w.alice_ctx, "issues", &le).await;
    assert_eq!(
        (d.status.encoding.as_str(), d.header[0].as_str()),
        ("utf-16le", "標題")
    );

    let (big5, _, bad) = encoding_rs::BIG5.encode(text);
    assert!(!bad);
    let d = upload(&w, w.alice_ctx, "issues", &big5).await;
    assert_eq!(
        (d.status.encoding.as_str(), d.status.encoding_guessed),
        ("big5", true)
    );
    assert_eq!(d.preview[0], ["登入失敗", "第一列"]);
    // The person confirms the guess.
    let mut s = settings(&d);
    s.encoding = Some("big5".into());
    let d = imports::put_settings(&w.app, w.alice_ctx, &d.status.id.to_string(), &s)
        .await
        .unwrap();
    assert!(!d.status.encoding_guessed);
    assert_eq!(d.status.state, "ready");

    let err = imports::upload(
        &w.app,
        w.alice_ctx,
        "WEB",
        "issues",
        b"a,b\n\xff\xfe\xfd\x80\x80,x\n",
    )
    .await
    .err()
    .unwrap();
    assert_eq!(code(&err), ErrorCode::ImportInvalidFile);
    assert_eq!(kind(&err), "encoding");

    // T-M2-88
    let d = upload(
        &w,
        w.alice_ctx,
        "issues",
        "subject;description\n\"a;b\";\"x\ny\"\n".as_bytes(),
    )
    .await;
    assert_eq!(d.status.delimiter, ";");
    assert_eq!(d.preview[0], ["a;b", "x\ny"]);
    let d = upload(
        &w,
        w.alice_ctx,
        "issues",
        b"subject\tdescription\none, two\tthree\n",
    )
    .await;
    assert_eq!(d.status.delimiter, "\t");
    assert_eq!(d.preview[0], ["one, two", "three"]);

    // Limits and the permission.
    let err = imports::upload(&w.app, w.alice_ctx, "WEB", "issues", b"subject\n")
        .await
        .err()
        .unwrap();
    assert_eq!(kind(&err), "empty");
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'import_issues'",
    )
    .await;
    let err = imports::upload(&w.app, w.alice_ctx, "WEB", "issues", b"subject\nx\n")
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::ForbiddenOp);
    // An agent or nobody: no import at all.
    let err = imports::upload(
        &w.app,
        AuthContext::anonymous(),
        "WEB",
        "issues",
        b"subject\nx\n",
    )
    .await
    .err()
    .unwrap();
    assert!(classify(&err).is_some());
}

/// T-M2-89: who a name resolves to; out of reach and unknown are the same.
#[tokio::test]
async fn t_m2_89_assignee_mapping() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    // dave exists but is no member; two members share a name.
    auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    for login in ["sam1", "sam2"] {
        let u = auth::create_user(&pool, login, "Sam Lee", PW, false)
            .await
            .unwrap();
        member(&pool, w.web, u, w.dev).await;
    }
    let file = "subject,assignee\none,BOB\ntwo,dave\nthree,nobody\nfour,sam lee\nfive,sam2\nsix,\n";
    let d = upload(&w, w.alice_ctx, "issues", file.as_bytes()).await;
    assert_eq!(d.suggested[1].as_deref(), Some("assigned_to"));
    let guesses: Vec<(String, bool, Option<String>)> = d.values["assigned_to"]
        .iter()
        .map(|g| (g.value.clone(), g.target.is_some(), g.problem.clone()))
        .collect();
    assert_eq!(guesses[0], ("BOB".into(), true, None));
    assert_eq!(guesses[1].2.as_deref(), Some("unknown_value"));
    assert_eq!(guesses[2].2.as_deref(), Some("unknown_value"));
    assert_eq!(guesses[3].2.as_deref(), Some("ambiguous_value"));
    // The choices never name someone out of reach.
    assert!(d.choices["assigned_to"].iter().all(|c| c.label != "dave"));

    let id = d.status.id;
    let s = dry(&w, w.alice_ctx, id).await;
    assert_eq!((s.ok, s.errors), (3, 3));
    let r = rows(&w, w.alice_ctx, id).await;
    assert_eq!(outcome(&r[0]), ("ok", None, None));
    assert_eq!(
        outcome(&r[1]),
        ("error", Some("unknown_value"), Some("assigned_to"))
    );
    assert_eq!(outcome(&r[1]), outcome(&r[2]));
    assert_eq!(
        outcome(&r[3]),
        ("error", Some("ambiguous_value"), Some("assigned_to"))
    );
    assert_eq!(outcome(&r[4]), ("ok", None, None));
    assert_eq!(outcome(&r[5]), ("ok", None, None));
    // A dry run writes nothing.
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 0);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM command_executions").await,
        0
    );

    // A value mapped by hand resolves; the dry run before no longer counts.
    let d = imports::preview(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    let mut set = settings(&d);
    set.value_map.insert(
        "assigned_to".into(),
        [
            ("dave".to_string(), "bob".to_string()),
            ("nobody".to_string(), String::new()),
        ]
        .into(),
    );
    let stale = set.lock_version;
    let d = imports::put_settings(&w.app, w.alice_ctx, &id.to_string(), &set)
        .await
        .unwrap();
    assert_eq!(d.status.state, "ready");
    let err = imports::run(
        &w.app,
        w.alice_ctx,
        &id.to_string(),
        Uuid::now_v7(),
        &ImportRunInput {
            lock_version: stale,
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(code(&err), ErrorCode::ImportStateInvalid);
    let s = dry(&w, w.alice_ctx, id).await;
    assert_eq!((s.ok, s.errors), (5, 1));

    // Someone else's import is one that does not exist.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let theirs = imports::get(&w.app, bob, &id.to_string())
        .await
        .err()
        .unwrap();
    let nothing = imports::get(&w.app, bob, &Uuid::now_v7().to_string())
        .await
        .err()
        .unwrap();
    assert_eq!(code(&theirs), ErrorCode::NotFound);
    assert_eq!(
        classify(&theirs).unwrap().message,
        classify(&nothing).unwrap().message
    );
    assert!(
        imports::list(&w.app, bob, "WEB")
            .await
            .unwrap()
            .items
            .is_empty()
    );

    // A mapping to a field that does not exist.
    let d = imports::preview(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    let mut set = settings(&d);
    set.mapping[0] = Some(format!("cf:{}", 987654));
    let err = imports::put_settings(&w.app, w.alice_ctx, &id.to_string(), &set)
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::ImportMappingInvalid);
}

/// T-M2-91, T-M2-95, R-I11: rows fail on their own; the report holds codes
/// and field keys, never a cell; a quiet import notifies nobody.
#[tokio::test]
async fn t_m2_91_rows_fail_alone() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    // Doing needs an estimate.
    exec(
        &pool,
        &format!(
            "INSERT INTO workflow_field_rules VALUES ('{}','{}','{}','estimated_hours','required')",
            w.bug, w.dev, w.doing
        ),
    )
    .await;
    let file = "Subject,Status,Estimated hours,Assignee,Due date,Private\n\
        CANARY-one,New,,bob,2026-12-01,no\n\
        CANARY-two,Done,,,,\n\
        CANARY-three,Doing,,,,\n\
        CANARY-four,Doing,2.5,,,yes\n\
        ,New,,,,\n\
        CANARY-six,New,,,CANARY-date,\n\
        '=CANARY-seven,,,,,\n";
    let d = upload(&w, w.alice_ctx, "issues", file.as_bytes()).await;
    let id = d.status.id;
    let s = dry(&w, w.alice_ctx, id).await;
    assert_eq!(
        (s.ok, s.errors),
        (3, 4),
        "{:?}",
        rows(&w, w.alice_ctx, id).await
    );
    let s = go(&w, w.alice_ctx, id).await;
    assert_eq!((s.done, s.ok, s.errors, s.skipped), (7, 3, 4, 0));
    let r = rows(&w, w.alice_ctx, id).await;
    assert_eq!(outcome(&r[0]), ("ok", None, None));
    assert_eq!(r[1].error_code.as_deref(), Some("invalid_transition"));
    assert_eq!(
        outcome(&r[2]),
        (
            "error",
            Some("required_field_missing"),
            Some("estimated_hours")
        )
    );
    assert_eq!(outcome(&r[3]), ("ok", None, None));
    assert_eq!(
        outcome(&r[4]),
        ("error", Some("required_field_missing"), Some("subject"))
    );
    assert_eq!(
        outcome(&r[5]),
        ("error", Some("bad_date"), Some("due_date"))
    );
    assert!(
        r[0].issue_key
            .as_deref()
            .is_some_and(|k| k.starts_with("WEB-"))
    );
    assert!(r[1].issue_key.is_none());
    // What was written: three issues, as alice, each execution naming the import.
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 3);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM issues i JOIN issue_statuses s ON s.id = i.status_id WHERE s.name = 'Doing' AND i.is_private AND i.estimated_hours = 2.5").await,
        1
    );
    // R-I7: the apostrophe the export added is gone.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM issues WHERE subject = '=CANARY-seven'"
        )
        .await,
        1
    );
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM command_executions WHERE import_id = '{id}' AND actor_id = '{}' AND origin = 'human_direct'", w.alice)).await,
        3,
        "one execution per created issue, none for a failed row"
    );

    // T-M2-95: neither the rows nor the report nor what is stored per row
    // holds a cell.
    let (name, report) = imports::report_csv(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    assert_eq!(name, "WEB-import-report.csv");
    let report = String::from_utf8(report).unwrap();
    assert!(report.starts_with("\u{feff}row,status,key,error_code,field\r\n1,ok,WEB-"));
    assert!(report.contains("\r\n6,error,,bad_date,due_date\r\n"));
    assert!(!report.contains("CANARY"));
    assert!(!serde_json::to_string(&r).unwrap().contains("CANARY"));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM import_rows r WHERE to_jsonb(r)::text LIKE '%CANARY%'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM imports WHERE settings::text LIKE '%CANARY%'"
        )
        .await,
        0
    );

    // R-I11: bob was assigned, and hears nothing.
    oxledger::notifications::dispatch_all(&pool).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM notifications").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM issue_watchers").await, 0);
    // With notify, an ordinary write's notifications.
    let d = upload(&w, w.alice_ctx, "issues", b"subject,assignee\nloud,bob\n").await;
    let mut set = settings(&d);
    set.options.notify = true;
    imports::put_settings(&w.app, w.alice_ctx, &d.status.id.to_string(), &set)
        .await
        .unwrap();
    dry(&w, w.alice_ctx, d.status.id).await;
    go(&w, w.alice_ctx, d.status.id).await;
    oxledger::notifications::dispatch_all(&pool).await.unwrap();
    assert!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM notifications WHERE recipient_id = '{}'",
                w.bob
            )
        )
        .await
            > 0
    );
}

/// T-M2-92: the worker dies mid-run; the next one goes on from the first
/// pending row and nothing is written twice. Cancel keeps what was written.
#[tokio::test]
async fn t_m2_92_resume_and_cancel() {
    use std::sync::atomic::Ordering;
    let pool = test_pool().await;
    let w = world(&pool).await;
    let mut file = String::from("subject\n");
    for n in 1..=8 {
        file.push_str(&format!("row {n}\n"));
    }
    let d = upload(&w, w.alice_ctx, "issues", file.as_bytes()).await;
    let id = d.status.id;
    dry(&w, w.alice_ctx, id).await;
    // The same request twice starts one run.
    let key = Uuid::now_v7();
    let lv = imports::get(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap()
        .lock_version;
    for _ in 0..2 {
        let s = imports::run(
            &w.app,
            w.alice_ctx,
            &id.to_string(),
            key,
            &ImportRunInput { lock_version: lv },
        )
        .await
        .unwrap();
        assert_eq!(s.state, "running");
    }
    imports::STOP_AFTER.store(3, Ordering::Relaxed);
    assert!(imports::work_round(&w.app).await.is_err());
    imports::STOP_AFTER.store(0, Ordering::Relaxed);
    let s = imports::get(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    assert_eq!((s.state.as_str(), s.done), ("running", 3));
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 3);
    // A row written whose result was lost: the idempotency key replays it.
    exec(
        &pool,
        &format!("UPDATE import_rows SET state = 'pending', issue_id = NULL, execution_id = NULL WHERE import_id = '{id}' AND row_no = 3"),
    )
    .await;
    imports::work_all(&w.app).await.unwrap();
    let s = imports::get(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    assert_eq!((s.state.as_str(), s.ok), ("done", 8));
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 8);
    assert_eq!(
        count(&pool, "SELECT count(DISTINCT subject) FROM issues").await,
        8
    );
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(DISTINCT issue_id) FROM import_rows WHERE import_id = '{id}'")
        )
        .await,
        8
    );

    // Cancel: after two rows the rest is skipped; the two stay.
    let d = upload(
        &w,
        w.alice_ctx,
        "issues",
        file.replace("row", "again").as_bytes(),
    )
    .await;
    let id = d.status.id;
    dry(&w, w.alice_ctx, id).await;
    start(&w, w.alice_ctx, id).await;
    imports::STOP_AFTER.store(2, Ordering::Relaxed);
    assert!(imports::work_round(&w.app).await.is_err());
    imports::STOP_AFTER.store(0, Ordering::Relaxed);
    let s = imports::cancel(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    assert_eq!((s.state.as_str(), s.ok, s.skipped), ("canceled", 2, 6));
    imports::work_all(&w.app).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 10);
    let err = imports::cancel(&w.app, w.alice_ctx, &id.to_string())
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::ImportStateInvalid);

    // R-I13: deleting keeps the record that the import wrote; retention.
    imports::delete(&w.app, w.alice_ctx, &id.to_string())
        .await
        .unwrap();
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM imports WHERE id = '{id}' AND file IS NULL AND settings->'mapping' IS NULL")).await,
        1
    );
    let d = upload(&w, w.alice_ctx, "issues", b"subject\nnever run\n").await;
    exec(&pool, "UPDATE imports SET created_at = now() - interval '8 days', finished_at = now() - interval '2 days' WHERE finished_at IS NOT NULL OR state = 'uploaded'").await;
    let p = imports::purge(&pool).await.unwrap();
    assert_eq!(p.expired, 1);
    assert!(p.files >= 1);
    assert_eq!(
        imports::get(&w.app, w.alice_ctx, &d.status.id.to_string())
            .await
            .unwrap()
            .state,
        "expired"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM imports WHERE file IS NOT NULL").await,
        0
    );
}

/// T-M2-93: `import_issues` without `add_issues`: every row is refused.
#[tokio::test]
async fn t_m2_93_import_without_add() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'add_issues'",
    )
    .await;
    let d = upload(&w, w.alice_ctx, "issues", b"subject\none\ntwo\n").await;
    let id = d.status.id;
    let s = dry(&w, w.alice_ctx, id).await;
    assert_eq!((s.ok, s.errors), (0, 2));
    go(&w, w.alice_ctx, id).await;
    let r = rows(&w, w.alice_ctx, id).await;
    assert!(
        r.iter()
            .all(|x| x.error_code.as_deref() == Some("forbidden_op")),
        "{r:?}"
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 0);

    // The owner is locked while the import waits: nothing is written and
    // the import ends, instead of an error on every row.
    grant(&pool, w.dev, &["add_issues"]).await;
    let d = upload(
        &w,
        w.alice_ctx,
        "issues",
        b"subject
one
two
",
    )
    .await;
    dry(&w, w.alice_ctx, d.status.id).await;
    start(&w, w.alice_ctx, d.status.id).await;
    exec(
        &pool,
        "UPDATE principals SET status = 'locked' WHERE login = 'alice'",
    )
    .await;
    imports::work_all(&w.app).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 0);
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM imports WHERE id = '{}' AND state = 'failed'",
                d.status.id
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM import_rows WHERE import_id = '{}' AND state = 'skipped'",
                d.status.id
            )
        )
        .await,
        2
    );
    exec(
        &pool,
        "UPDATE principals SET status = 'active' WHERE login = 'alice'",
    )
    .await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'add_issues'",
    )
    .await;
    let alice = sign_in(&pool, &w.policy, "alice").await;
    let w = World {
        alice_ctx: alice,
        ..w
    };

    // The owner loses the import permission before the worker gets to it.
    grant(&pool, w.dev, &["add_issues"]).await;
    let d = upload(&w, w.alice_ctx, "issues", b"subject\none\n").await;
    dry(&w, w.alice_ctx, d.status.id).await;
    start(&w, w.alice_ctx, d.status.id).await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'import_issues'",
    )
    .await;
    imports::work_all(&w.app).await.unwrap();
    let s = imports::get(&w.app, w.alice_ctx, &d.status.id.to_string())
        .await
        .unwrap();
    assert_eq!((s.state.as_str(), s.skipped), ("failed", 1));
    assert_eq!(count(&pool, "SELECT count(*) FROM issues").await, 0);
}

/// T-M2-94: undo closes what the import created and nobody touched since.
#[tokio::test]
async fn t_m2_94_close_created() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    sqlx::query("INSERT INTO workflow_transitions VALUES ($1,$2,$3,$4,'always')")
        .bind(w.bug)
        .bind(w.dev)
        .bind(w.new)
        .bind(w.done)
        .execute(&pool)
        .await
        .unwrap();
    let mut file = String::from("subject\n");
    for n in 1..=6 {
        file.push_str(&format!("made {n}\n"));
    }
    let d = upload(&w, w.alice_ctx, "issues", file.as_bytes()).await;
    let id = d.status.id;
    dry(&w, w.alice_ctx, id).await;
    go(&w, w.alice_ctx, id).await;
    // Three of them are changed afterwards.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let touched: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT i.id, i.lock_version FROM issues i WHERE i.subject IN ('made 2', 'made 4', 'made 5')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for (i, lv) in &touched {
        commands::submit(&w.app, bob, Uuid::now_v7(), &note_doc(*i, *lv, "seen"))
            .await
            .unwrap();
    }
    let close = ImportCloseInput {
        status_id: Some(w.done),
    };
    let err = imports::close_created(
        &w.app,
        w.alice_ctx,
        &id.to_string(),
        &ImportCloseInput {
            status_id: Some(w.doing),
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);
    let r = imports::close_created(&w.app, w.alice_ctx, &id.to_string(), &close)
        .await
        .unwrap();
    assert_eq!((r.closed, r.already), (3, 0));
    let failed: Vec<(i32, &str)> = r
        .failed
        .iter()
        .map(|f| (f.row_no, f.reason.as_str()))
        .collect();
    assert_eq!(
        failed,
        [
            (2, "changed_since"),
            (4, "changed_since"),
            (5, "changed_since")
        ]
    );
    assert!(r.failed.iter().all(|f| f.issue_key.is_some()));
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM issues WHERE status_id = '{}'", w.done)
        )
        .await,
        3
    );
    // Again: nothing new.
    let r = imports::close_created(&w.app, w.alice_ctx, &id.to_string(), &close)
        .await
        .unwrap();
    assert_eq!((r.closed, r.already, r.failed.len()), (0, 3, 3));
    // Not someone else's to undo.
    let err = imports::close_created(&w.app, bob, &id.to_string(), &close)
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::NotFound);
}

/// T-M2-97: a redaction in the project drops the files imports still keep.
#[tokio::test]
async fn t_m2_97_redaction_drops_files() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    let d = upload(&w, w.alice_ctx, "issues", b"subject\nsecret row\n").await;
    let other = imports::upload(&w.app, w.alice_ctx, "AUTH", "issues", b"subject\nkept\n")
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    oxledger::redaction::derived(&mut tx, w.web, w.alice)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(&pool, &format!("SELECT count(*) FROM imports WHERE id = '{}' AND file IS NULL AND purged_at IS NOT NULL", d.status.id)).await,
        1
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM imports WHERE id = '{}' AND file IS NOT NULL",
                other.status.id
            )
        )
        .await,
        1
    );
    // Without its file an import cannot be checked or run.
    let err = imports::validate(&w.app, w.alice_ctx, &d.status.id.to_string())
        .await
        .err()
        .unwrap();
    assert_eq!(code(&err), ErrorCode::ImportStateInvalid);
    let s = imports::get(&w.app, w.alice_ctx, &d.status.id.to_string())
        .await
        .unwrap();
    assert!(!s.file_available);
}

/// T-M2-98: time entries for others, issues out of sight, people who
/// cannot log time here.
#[tokio::test]
async fn t_m2_98_time_entries() {
    let pool = test_pool().await;
    let w = world(&pool).await;
    exec(
        &pool,
        "INSERT INTO enabled_modules SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING",
    )
    .await;
    grant(&pool, w.dev, &["view_time_entries", "log_time"]).await;
    // dave is a member who cannot log time.
    let watcher = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Watcher','all') RETURNING id",
    )
    .await;
    grant(&pool, watcher, &["view_issues"]).await;
    let dave = auth::create_user(&pool, "dave", "dave", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, dave, watcher).await;
    let seen = w.issue(w.web, "timed").await;
    let hidden = w.issue(w.auth_p, "elsewhere").await;
    let key = |i: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current",
            )
            .bind(i)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (seen, hidden) = (key(seen).await, key(hidden).await);
    // carol is in WEB only: AUTH's issue is out of her sight.
    let carol = sign_in(&pool, &w.policy, "carol").await;
    let file = format!(
        "日期,時數,單號,使用者,說明\n\
         2026-11-02,1.5,{seen},,CANARY own\n\
         2026-11-03,2,#{seen},bob,CANARY for bob\n\
         2026-11-03,1,{hidden},,\n\
         2026-11-03,1,WEB-99999,,\n\
         2026-11-03,1,,dave,\n\
         2026-11-03,abc,,,\n\
         2026/11/04,0.25,,,\n"
    );
    let d = upload(&w, carol, "time_entries", file.as_bytes()).await;
    assert_eq!(
        d.suggested,
        ["spent_on", "hours", "issue", "user", "comments"].map(|s| Some(s.to_string()))
    );
    assert!(d.choices["user"].iter().all(|c| c.label != "dave"));
    let id_ = d.status.id;
    dry(&w, carol, id_).await;
    let r = rows(&w, carol, id_).await;
    assert_eq!(outcome(&r[0]), ("ok", None, None));
    // Without log_time_for_other_users.
    assert_eq!(r[1].error_code.as_deref(), Some("forbidden_op"));
    assert_eq!(
        outcome(&r[2]),
        ("error", Some("unknown_value"), Some("issue"))
    );
    assert_eq!(outcome(&r[2]), outcome(&r[3]));
    assert_eq!(
        outcome(&r[4]),
        ("error", Some("unknown_value"), Some("user"))
    );
    assert_eq!(outcome(&r[5]), ("error", Some("bad_number"), Some("hours")));
    assert_eq!(outcome(&r[6]), ("ok", None, None));
    assert_eq!(count(&pool, "SELECT count(*) FROM time_entries").await, 0);

    grant(&pool, w.dev, &["log_time_for_other_users"]).await;
    go(&w, carol, id_).await;
    let r = rows(&w, carol, id_).await;
    assert_eq!(outcome(&r[1]), ("ok", None, None));
    assert!(r[1].time_entry_id.is_some());
    assert_eq!(
        outcome(&r[2]),
        ("error", Some("unknown_value"), Some("issue"))
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM time_entries WHERE deleted_at IS NULL"
        )
        .await,
        3
    );
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT count(*) FROM time_entries WHERE user_id = '{}' AND author_id = '{}'",
                w.bob, w.carol
            )
        )
        .await,
        1
    );
    let report = String::from_utf8(
        imports::report_csv(&w.app, carol, &id_.to_string())
            .await
            .unwrap()
            .1,
    )
    .unwrap();
    assert!(!report.contains("CANARY") && !report.contains(&hidden));

    // R-I12: undo deletes the entries the import logged (as the caller: it
    // takes the right to delete them).
    grant(&pool, w.dev, &["edit_time_entries"]).await;
    let r = imports::close_created(
        &w.app,
        carol,
        &id_.to_string(),
        &ImportCloseInput { status_id: None },
    )
    .await
    .unwrap();
    assert_eq!((r.closed, r.failed.len()), (3, 0), "{:?}", r.failed);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM time_entries WHERE deleted_at IS NULL"
        )
        .await,
        0
    );
    let r = imports::close_created(
        &w.app,
        carol,
        &id_.to_string(),
        &ImportCloseInput { status_id: None },
    )
    .await
    .unwrap();
    assert_eq!((r.closed, r.already), (0, 3));
    let _ = json!({});
}
