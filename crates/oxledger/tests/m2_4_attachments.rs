// SPDX-License-Identifier: GPL-2.0-or-later
//! Design 06 M2.4 (attachments, A2 (a)), the upload half: R-A1 (who may
//! upload, the raw body), R-A2 (limits), R-A3 (types from the first
//! bytes), R-A4 (one blob per content and project), R-A11 (off without a
//! directory). T-M2-50 (types), T-M2-54, T-M2-59. Needs PostgreSQL 18.

mod common;
#[path = "s6_world/mod.rs"]
mod world;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use common::test_pool;
use futures_util::{StreamExt, stream};
use http_body_util::BodyExt;
use oxledger::api::{self, ApiConfig, LOGIN_COOKIE, SESSION_COOKIE};
use oxledger::attachments::{self, FilesConfig, UploadView};
use oxledger::auth::{AuthContext, SessionPolicy};
use oxledger::error::ErrorCode;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::path::Path;
use tower::ServiceExt;
use uuid::Uuid;
use world::*;

const ORIGIN: &str = "https://spa.test";
const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, 1, 2, 3,
];

fn files(dir: &Path) -> FilesConfig {
    FilesConfig {
        free_space_override: Some(u64::MAX / 4),
        ..FilesConfig::at(dir)
    }
}

/// One chunk per piece.
fn chunks(pieces: &[&[u8]]) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> + Unpin {
    stream::iter(
        pieces
            .iter()
            .map(|p| Ok(Bytes::copy_from_slice(p)))
            .collect::<Vec<_>>(),
    )
}

async fn up(
    w: &World,
    who: AuthContext,
    project: &str,
    name: &str,
    bytes: &[u8],
) -> anyhow::Result<UploadView> {
    attachments::upload(
        &w.app,
        who,
        project,
        Some(name),
        Some(bytes.len() as u64),
        chunks(&[bytes]),
    )
    .await
}

fn tmp_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(".tmp"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// T-M2-50 (the upload side): the type comes from the first bytes only.
#[tokio::test]
async fn types_come_from_the_first_bytes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));

    let html = b"<!doctype html><script>alert(1)</script>";
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>";
    let pdf = b"%PDF-1.7\n%...";
    let gif = b"GIF89a\x01\x00\x01\x00";
    let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF";
    let webp = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
    for (name, bytes, want) in [
        ("fake.png", &html[..], "application/octet-stream"),
        ("a.svg", &svg[..], "application/octet-stream"),
        ("a.pdf", &pdf[..], "application/octet-stream"),
        ("real.png", PNG, "image/png"),
        ("a.gif", &gif[..], "image/gif"),
        ("a.jpg", &jpeg[..], "image/jpeg"),
        ("a.webp", &webp[..], "image/webp"),
    ] {
        let v = up(&w, w.alice_ctx, "WEB", name, bytes).await.unwrap();
        assert_eq!(v.content_type, want, "{name}");
        assert_eq!(v.filename, name);
        assert_eq!(v.size, bytes.len() as i64);
        let stored: String = sqlx::query_scalar(
            "SELECT b.sniffed_type FROM uploads u JOIN attachment_blobs b ON b.id = u.blob_id
             WHERE u.id = $1",
        )
        .bind(v.upload_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, want);
    }
    assert!(attachments::is_image("image/webp"));
    assert!(!attachments::is_image("image/svg+xml"));
    assert_eq!(tmp_files(dir.path()), 0);
}

/// T-M2-54, R-A4: the same content in one project is one blob (and one
/// file); in another project it is another.
#[tokio::test]
async fn one_blob_per_content_and_project() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));

    let a = up(&w, w.alice_ctx, "WEB", "a.png", PNG).await.unwrap();
    let b = up(&w, w.alice_ctx, "WEB", "b.png", PNG).await.unwrap();
    let c = up(&w, w.alice_ctx, "AUTH", "a.png", PNG).await.unwrap();
    assert_ne!(a.upload_id, b.upload_id);
    let blob = |u: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT blob_id, project_id FROM uploads WHERE id = $1",
            )
            .bind(u)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (ba, pa) = blob(a.upload_id).await;
    let (bb, _) = blob(b.upload_id).await;
    let (bc, pc) = blob(c.upload_id).await;
    assert_eq!(ba, bb);
    assert_ne!(ba, bc);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM attachment_blobs").await,
        2
    );
    let pa_file = attachments::blob_path(dir.path(), pa, ba);
    assert_eq!(std::fs::read(&pa_file).unwrap(), PNG);
    assert!(attachments::blob_path(dir.path(), pc, bc).exists());
    assert_eq!(tmp_files(dir.path()), 0);
    // Each upload counts towards the day, the shared blob or not.
    assert_eq!(
        count(
            &pool,
            &format!(
                "SELECT bytes FROM upload_daily_usage WHERE user_id = '{}'",
                w.alice
            )
        )
        .await,
        3 * PNG.len() as i64
    );
}

/// R-A1: who may upload, R-A11: off without a directory, and file names.
#[tokio::test]
async fn who_may_upload() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();

    // Off: forbidden_op attachments_disabled.
    attachments::install(FilesConfig::default());
    let e = up(&w, w.alice_ctx, "WEB", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(
        oxledger::error::classify(&e).unwrap().details.unwrap()["reason"],
        "attachments_disabled"
    );
    attachments::install(files(dir.path()));

    // A reader without a permission that attaches files: forbidden_op; a
    // project they cannot see, and one that does not exist: not_found.
    let reader = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Reader','all') RETURNING id",
    )
    .await;
    grant(&pool, reader, &["view_issues"]).await;
    let zed = oxledger::auth::create_user(&pool, "zed", "zed", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, zed, reader).await;
    let zctx = sign_in(&pool, &w.policy, "zed").await;
    let e = up(&w, zctx, "WEB", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    let e = up(&w, zctx, "AUTH", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = up(&w, zctx, "NOPE", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // A member who sees the project but attaches nothing: forbidden_op.
    let timer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Timer','own') RETURNING id",
    )
    .await;
    grant(&pool, timer, &["log_time"]).await;
    member(&pool, w.auth_p, zed, timer).await;
    let e = up(&w, zctx, "AUTH", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // A closed project takes no writes.
    exec(
        &pool,
        "UPDATE projects SET status = 'closed' WHERE key = 'AUTH'",
    )
    .await;
    let e = up(&w, w.alice_ctx, "AUTH", "a.png", PNG).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // Wiki writers may upload.
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects WHERE key = 'WEB'",
    )
    .await;
    grant(&pool, reader, &["view_wiki_pages", "edit_wiki_pages"]).await;
    up(&w, zctx, "WEB", "a.png", PNG).await.unwrap();

    // File names.
    for bad in [
        "",
        "   ",
        "a/b.png",
        "a\\b.png",
        "a\u{202E}gnp.exe",
        "a\nb",
        &"x".repeat(256),
    ] {
        let e = up(&w, w.alice_ctx, "WEB", bad, PNG).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue, "{bad:?}");
    }
    let v = up(&w, w.alice_ctx, "WEB", "  報告 v2.png ", PNG)
        .await
        .unwrap();
    assert_eq!(v.filename, "報告 v2.png");
    let e = attachments::upload(&w.app, w.alice_ctx, "WEB", None, Some(1), chunks(&[b"x"]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
}

/// T-M2-59 (the service side): the limits.
#[tokio::test]
async fn upload_limits() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = files(dir.path());
    cfg.max_bytes = 100;
    cfg.daily_bytes = 250;
    attachments::install(cfg.clone());

    // A Content-Length over the limit: refused unread.
    let never = stream::poll_fn(|_| -> std::task::Poll<Option<Result<Bytes, Infallible>>> {
        panic!("the body was read")
    });
    let e = attachments::upload(&w.app, w.alice_ctx, "WEB", Some("big"), Some(101), never)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PayloadTooLarge);

    // No Content-Length: refused unread (§3.1: it is required).
    let never = stream::poll_fn(|_| -> std::task::Poll<Option<Result<Bytes, Infallible>>> {
        panic!("the body was read")
    });
    let e = attachments::upload(&w.app, w.alice_ctx, "WEB", Some("x"), None, never)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);

    // More bytes than the limit (and than the Content-Length): stopped at
    // the limit, nothing left behind.
    let big = vec![7u8; 60];
    let e = attachments::upload(
        &w.app,
        w.alice_ctx,
        "WEB",
        Some("big"),
        Some(100),
        chunks(&[&big, &big]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PayloadTooLarge);
    assert_eq!(tmp_files(dir.path()), 0);

    // A request dropped half way (the client went away): no temporary file.
    let stalled = stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from_static(b"half"))])
        .chain(stream::pending());
    let r = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        attachments::upload(&w.app, w.alice_ctx, "WEB", Some("x"), Some(50), stalled),
    )
    .await;
    assert!(r.is_err(), "the upload should still be waiting");
    assert_eq!(tmp_files(dir.path()), 0);

    // Longer or shorter than its Content-Length: stopped, nothing left.
    for (len, pieces) in [(10u64, vec![&big[..8], &big[..8]]), (20, vec![&big[..8]])] {
        let e = attachments::upload(
            &w.app,
            w.alice_ctx,
            "WEB",
            Some("x"),
            Some(len),
            chunks(&pieces),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidRequest, "{len}");
        assert_eq!(tmp_files(dir.path()), 0);
    }
    // Empty.
    let e = attachments::upload(&w.app, w.alice_ctx, "WEB", Some("x"), Some(0), chunks(&[]))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM attachment_blobs").await,
        0
    );

    // The day's bytes: 100 + 100 fit, the next 100 do not.
    let a = vec![1u8; 100];
    let b = vec![2u8; 100];
    let c = vec![3u8; 100];
    up(&w, w.alice_ctx, "WEB", "a", &a).await.unwrap();
    up(&w, w.alice_ctx, "WEB", "b", &b).await.unwrap();
    let e = up(&w, w.alice_ctx, "WEB", "c", &c).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PayloadTooLarge);
    // Someone else's day is their own.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    up(&w, bob, "WEB", "c", &c).await.unwrap();

    // Three at the same time, not four.
    let slots: Vec<_> = (0..3)
        .map(|_| attachments::hold_upload_slot(w.bob).unwrap())
        .collect();
    let e = up(&w, bob, "WEB", "d", &[9u8; 10]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::RateLimited);
    drop(slots);
    up(&w, bob, "WEB", "d", &[9u8; 10]).await.unwrap();

    // A nearly full disk.
    cfg.free_space_override = Some(cfg.min_free_bytes - 1);
    attachments::install(cfg.clone());
    let e = up(&w, bob, "WEB", "e", &[8u8; 10]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InsufficientStorage);
    assert_eq!(
        oxledger::error::classify(&e).unwrap().code.http_status(),
        507
    );

    // Unused uploads together: 200 MB (counted from the database).
    cfg.free_space_override = Some(u64::MAX / 4);
    cfg.daily_bytes = u64::MAX / 4;
    attachments::install(cfg);
    exec(
        &pool,
        &format!(
            "UPDATE uploads SET size = 200 * 1024 * 1024 WHERE uploader_id = '{}'
               AND id = (SELECT id FROM uploads WHERE uploader_id = '{}' LIMIT 1)",
            w.bob, w.bob
        ),
    )
    .await;
    let e = up(&w, bob, "WEB", "f", &[6u8; 10]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PayloadTooLarge);
}

// ---------------------------------------------------------------- HTTP

async fn login(r: &Router, login: &str) -> (String, String) {
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
                .header(header::ORIGIN, ORIGIN)
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
    (session, csrf)
}

async fn post_raw(
    r: &Router,
    who: Option<&(String, String)>,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut b = Request::post(format!("/api/v1{path}"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len());
    if let Some((s, c)) = who {
        b = b
            .header(header::COOKIE, format!("{SESSION_COOKIE}={s}"))
            .header("x-csrf-token", c);
    }
    let resp = r
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// R-A1 over HTTP: the raw body, 201, anonymous 401, the CSRF token.
#[tokio::test]
async fn upload_over_http() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    let r = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let alice = login(&r, "alice").await;
    let path = "/projects/WEB/uploads?filename=shot.png";

    let (s, v) = post_raw(
        &r,
        Some(&alice),
        path,
        "application/octet-stream",
        PNG.to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["data"]["content_type"], "image/png");
    assert_eq!(v["data"]["filename"], "shot.png");
    assert!(v["data"]["upload_id"].is_string());

    // Anonymous: 401 (even on a public project).
    exec(&pool, "UPDATE projects SET is_public = true").await;
    let (s, v) = post_raw(&r, None, path, "application/octet-stream", PNG.to_vec()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");

    // JSON is not an upload; the CSRF token is still needed.
    let (s, v) = post_raw(&r, Some(&alice), path, "application/json", b"{}".to_vec()).await;
    assert_eq!(v["error"]["code"], "csrf_failed", "{s} {v}");
    let wrong = (alice.0.clone(), "nope".to_string());
    let (_, v) = post_raw(
        &r,
        Some(&wrong),
        path,
        "application/octet-stream",
        PNG.to_vec(),
    )
    .await;
    assert_eq!(v["error"]["code"], "csrf_failed");
    // Elsewhere octet-stream is still refused.
    let (_, v) = post_raw(
        &r,
        Some(&alice),
        "/commands",
        "application/octet-stream",
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(v["error"]["code"], "csrf_failed");
    assert_eq!(count(&pool, "SELECT count(*) FROM uploads").await, 1);
}

// ---------------------------------------------------------------- a2: attaching and reading

const HTML: &[u8] = b"<html><script>alert(1)</script></html>";

async fn key_of(pool: &sqlx::PgPool, issue: Uuid) -> String {
    sqlx::query_scalar("SELECT key FROM issue_keys WHERE issue_id = $1 AND is_current")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn run(w: &World, who: AuthContext, ops: Value) -> anyhow::Result<Value> {
    let mut doc = json!({"command_schema_version": 1, "ops": ops});
    // add_note on an existing issue carries its lock_version.
    if let Some(i) = doc["ops"][0]["target"]["id"].as_str() {
        let i: Uuid = i.parse().unwrap();
        let lv = lock_version(&w.pool, i).await;
        doc["preconditions"] = json!([{"target": {"type": "issue", "id": i}, "lock_version": lv}]);
    }
    oxledger::commands::submit(&w.app, who, Uuid::now_v7(), &doc)
        .await
        .map(|r| serde_json::to_value(r).unwrap())
}

fn details(e: &anyhow::Error) -> Value {
    oxledger::error::classify(e)
        .unwrap()
        .details
        .unwrap_or(Value::Null)
}

fn ids_of(r: &Value) -> Vec<Uuid> {
    serde_json::from_value(r["items"][0]["attachment_ids"].clone()).unwrap()
}

async fn view(w: &World, who: AuthContext, key: &str) -> Value {
    let p = serde_json::from_value(json!({"key": key, "include": ["recent_notes"]})).unwrap();
    serde_json::to_value(oxledger::issue_view::get(&w.app, who, p).await.unwrap()).unwrap()
}

async fn first_note(pool: &sqlx::PgPool, issue: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at LIMIT 1")
        .bind(issue)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn lock_version(pool: &sqlx::PgPool, issue: Uuid) -> i64 {
    count(
        pool,
        &format!("SELECT lock_version::bigint FROM issues WHERE id = '{issue}'"),
    )
    .await
}

/// R-A7, R-A5, T-M2-52, T-M2-53: create_issue and add_note with
/// attachments, attach, delete_attachment, who reads what.
#[tokio::test]
async fn attach_read_and_delete() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    let a = w.alice_ctx;

    // create_issue with an attachment (renamed).
    let u = up(&w, a, "WEB", "shot.png", PNG).await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "create_issue", "project_id": w.web,
                "tracker_id": w.bug, "fields": {"subject": "with a file"},
                "attachments": [{"upload_id": u.upload_id, "filename": "畫面.png"}]}]),
    )
    .await
    .unwrap();
    let issue: Uuid = serde_json::from_value(r["items"][0]["issue_id"].clone()).unwrap();
    let on_issue = ids_of(&r)[0];
    assert_eq!(
        r["effects"][0]["events"],
        json!(["created", "attachment_added"])
    );
    let key = key_of(&pool, issue).await;
    let v = view(&w, a, &key).await;
    assert_eq!(v["attachments"][0]["filename"], "畫面.png");
    assert_eq!(v["attachments"][0]["content_type"], "image/png");
    assert_eq!(v["attachments"][0]["author"], "alice");
    // An upload is used once.
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "upload_unknown");

    // A private note with an HTML file named .png.
    let h = up(&w, a, "WEB", "evil.png", HTML).await.unwrap();
    assert_eq!(h.content_type, "application/octet-stream");
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue},
                "body": "see file", "is_private": true,
                "attachments": [{"upload_id": h.upload_id}]}]),
    )
    .await
    .unwrap();
    let on_note = ids_of(&r)[0];
    let v = view(&w, a, &key).await;
    assert_eq!(
        v["recent_notes"]["items"][0]["attachments"][0]["id"],
        json!(on_note)
    );
    assert_eq!(
        v["attachments"].as_array().unwrap().len(),
        1,
        "a note's attachments stay on the note"
    );

    // zed reads the issue but not private notes (T-M2-52).
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','all') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    let zed = oxledger::auth::create_user(&pool, "zed", "zed", PW, false)
        .await
        .unwrap();
    member(&pool, w.web, zed, viewer).await;
    let z = sign_in(&pool, &w.policy, "zed").await;
    attachments::read::get(&w.app, z, on_issue).await.unwrap();
    let hidden = attachments::read::get(&w.app, z, on_note)
        .await
        .unwrap_err();
    let missing = attachments::read::get(&w.app, z, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&hidden), ErrorCode::NotFound);
    assert_eq!(format!("{hidden}"), format!("{missing}"));
    let v = view(&w, z, &key).await;
    assert!(v["recent_notes"]["items"].as_array().unwrap().is_empty());
    // A private issue zed cannot see.
    let secret = w.issue(w.web, "secret").await;
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{secret}'"),
    )
    .await;
    exec(
        &pool,
        "UPDATE roles SET issues_visibility = 'default' WHERE name = 'Viewer'",
    )
    .await;
    let s = up(&w, a, "WEB", "s.png", b"secret bytes").await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach",
                "target": {"issue": key_of(&pool, secret).await},
                "attachments": [{"upload_id": s.upload_id}]}]),
    )
    .await
    .unwrap();
    let on_secret = ids_of(&r)[0];
    let e = attachments::read::get(&w.app, z, on_secret)
        .await
        .unwrap_err();
    assert_eq!(format!("{e}"), format!("{missing}"));

    // T-M2-53: another's upload, an expired one, another project's, none:
    // one shape.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let theirs = up(&w, bob, "WEB", "b.png", b"bob's").await.unwrap();
    let expired = up(&w, a, "WEB", "x.png", b"expired").await.unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE uploads SET expires_at = now() - interval '1 second' WHERE id = '{}'",
            expired.upload_id
        ),
    )
    .await;
    let elsewhere = up(&w, a, "AUTH", "y.png", PNG).await.unwrap();
    let mut shapes = Vec::new();
    for u in [
        theirs.upload_id,
        expired.upload_id,
        elsewhere.upload_id,
        Uuid::now_v7(),
    ] {
        let e = run(
            &w,
            a,
            json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                    "attachments": [{"upload_id": u}]}]),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidValue);
        shapes.push((format!("{e}"), details(&e)));
    }
    assert!(shapes.iter().all(|s| *s == shapes[0]), "{shapes:?}");
    assert_eq!(shapes[0].1["kind"], "upload_unknown");
    // More than 10 in a document.
    let many: Vec<Value> = (0..11)
        .map(|_| json!({"upload_id": Uuid::now_v7()}))
        .collect();
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue}, "body": "x", "attachments": many}]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "attachment_limit");

    // attach keeps the issue's lock_version.
    let lv = lock_version(&pool, issue).await;
    let f = up(&w, a, "WEB", "later.txt", b"later").await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": f.upload_id}]}]),
    )
    .await
    .unwrap();
    assert_eq!(r["effects"][0]["lock_version"], json!(lv));
    assert_eq!(lock_version(&pool, issue).await, lv);
    let later = ids_of(&r)[0];
    // attach to the private note.
    let g = up(&w, a, "WEB", "g.png", b"gg").await.unwrap();
    run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach",
                "target": {"note": first_note(&pool, issue).await},
                "attachments": [{"upload_id": g.upload_id}]}]),
    )
    .await
    .unwrap();
    // zed may not delete.
    let e = run(
        &w,
        z,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": on_issue}}]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    // bob (edit_issues) deletes alice's; then it is gone.
    let r = run(
        &w,
        bob,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": later}}]),
    )
    .await
    .unwrap();
    assert_eq!(r["effects"][0]["events"], json!(["attachment_removed"]));
    let e = attachments::read::get(&w.app, a, later).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = run(
        &w,
        bob,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": later}}]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM issue_events WHERE issue_id = $1 AND kind LIKE 'attachment%' ORDER BY seq",
    )
    .bind(issue)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        [
            "attachment_added",
            "attachment_added",
            "attachment_added",
            "attachment_added",
            "attachment_removed"
        ]
    );
}

/// R-A9: agents never attach (refused before parsing) nor send the
/// attachment ops; the routes are for people.
#[tokio::test]
async fn agents_do_not_attach() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    let issue = w.issue(w.web, "x").await;
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let doc = json!({"command_schema_version": 1,
                     "ops": [{"op_id": Uuid::now_v7(), "kind": "add_note",
                              "target": {"type": "issue", "id": issue}, "body": "x",
                              "attachments": [{"upload_id": Uuid::now_v7()}]}],
                     "preconditions": [{"target": {"type": "issue", "id": issue}, "lock_version": 0}]});
    let e =
        oxledger::changesets::command_submit(&w.app, agent, Uuid::now_v7(), &doc, false, ORIGIN)
            .await
            .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    assert_eq!(details(&e)["reason"], "attachments_people_only");
    let doc = json!({"command_schema_version": 1,
                     "ops": [{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                              "target": {"attachment": Uuid::now_v7()}}]});
    let e =
        oxledger::changesets::command_submit(&w.app, agent, Uuid::now_v7(), &doc, false, ORIGIN)
            .await
            .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest);
    let e = attachments::read::get(&w.app, agent, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(details(&e)["reason"], "attachments_people_only");
}

/// R-W14: wiki_put with attachments; the page lists them; who deletes.
#[tokio::test]
async fn wiki_attachments() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(
        &pool,
        w.dev,
        &["view_wiki_pages", "view_wiki_edits", "edit_wiki_pages"],
    )
    .await;
    let a = w.alice_ctx;
    let u = up(&w, a, "WEB", "plan.png", PNG).await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page": "Plan"},
                "body": "# Plan", "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let page: Uuid = serde_json::from_value(r["items"][0]["wiki_page_id"].clone()).unwrap();
    let att = ids_of(&r)[0];
    let p = oxledger::wiki::page(&w.app, a, "WEB", "Plan")
        .await
        .unwrap();
    assert_eq!(p.attachments.len(), 1);
    assert_eq!(p.attachments[0].can_delete, Some(true));
    assert!(p.can.attach);
    let v = attachments::read::get(&w.app, a, att).await.unwrap();
    assert_eq!(v.wiki_title.as_deref(), Some("Plan"));
    assert_eq!(v.wiki_version, Some(1));
    // bob holds no delete_wiki_pages and is not the author.
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let p = oxledger::wiki::page(&w.app, bob, "WEB", "Plan")
        .await
        .unwrap();
    assert_eq!(p.attachments[0].can_delete, Some(false));
    let del = json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                      "target": {"attachment": att}}]);
    let e = run(&w, bob, del.clone()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);
    run(&w, a, del).await.unwrap();
    let p = oxledger::wiki::page(&w.app, a, "WEB", "Plan")
        .await
        .unwrap();
    assert!(p.attachments.is_empty());
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM wiki_events WHERE page_id = $1 AND kind LIKE 'attachment%' ORDER BY seq",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kinds, ["attachment_added", "attachment_removed"]);
}

async fn get_raw(
    r: &Router,
    who: Option<&(String, String)>,
    path: &str,
) -> axum::response::Response {
    let mut b = Request::get(path.to_string());
    if let Some((s, _)) = who {
        b = b.header(header::COOKIE, format!("{SESSION_COOKIE}={s}"));
    }
    r.clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// T-M2-50, T-M2-51: download headers; the signed redirect (R-A6).
#[tokio::test]
async fn downloads() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    let a = w.alice_ctx;
    let issue = w.issue(w.web, "files").await;
    let key = key_of(&pool, issue).await;
    let png = up(&w, a, "WEB", "截圖 \"1\".png", PNG).await.unwrap();
    let html = up(&w, a, "WEB", "page.png", HTML).await.unwrap();
    let svg = up(
        &w,
        a,
        "WEB",
        "logo.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .await
    .unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": png.upload_id}, {"upload_id": html.upload_id},
                                {"upload_id": svg.upload_id}]}]),
    )
    .await
    .unwrap();
    let ids = ids_of(&r);
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let alice = login(&router, "alice").await;
    let content = |id: Uuid| format!("/api/v1/attachments/{id}/content");
    let resp = get_raw(&router, Some(&alice), &content(ids[0])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers().clone();
    assert_eq!(h[header::CONTENT_TYPE], "image/png");
    assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(
        h[header::CONTENT_SECURITY_POLICY],
        "sandbox; default-src 'none'"
    );
    assert_eq!(h[header::CACHE_CONTROL], "private, no-store");
    let cd = h[header::CONTENT_DISPOSITION].to_str().unwrap();
    assert!(cd.starts_with("inline; "), "{cd}");
    assert!(
        cd.contains("filename*=UTF-8''%E6%88%AA%E5%9C%96%20%221%22.png"),
        "{cd}"
    );
    assert!(!cd.contains("\"1\""), "{cd}");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], PNG);
    for id in &ids[1..] {
        let resp = get_raw(&router, Some(&alice), &content(*id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        let cd = resp.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cd.starts_with("attachment; "), "{cd}");
    }
    let resp = get_raw(&router, Some(&alice), &content(Uuid::now_v7())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = get_raw(
        &router,
        Some(&alice),
        &format!("/api/v1/attachments/{}", ids[0]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A download origin: a signed 302, served there; a changed link is
    // not_found.
    attachments::install(FilesConfig {
        origin: Some("https://files.test".into()),
        ..files(dir.path())
    });
    let resp = get_raw(&router, Some(&alice), &content(ids[0])).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let to = resp.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    let path = to.strip_prefix("https://files.test").unwrap().to_string();
    assert!(path.starts_with(&format!("/files/{}?u=", ids[0])), "{path}");
    let resp = get_raw(&router, None, &path).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_SECURITY_POLICY],
        "sandbox; default-src 'none'"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], PNG);
    let other = path.replace(&ids[0].to_string(), &ids[1].to_string());
    assert_eq!(
        get_raw(&router, None, &other).await.status(),
        StatusCode::NOT_FOUND
    );
    let last = if path.ends_with('0') { "1" } else { "0" };
    let forged = format!("{}{last}", &path[..path.len() - 1]);
    assert_eq!(
        get_raw(&router, None, &forged).await.status(),
        StatusCode::NOT_FOUND
    );
}

/// The review's findings: a retracted note's files leave the history; a
/// closed project's reason; duplicate uploads; unknown uploads on every
/// path (a purged blob too); owner revisions refuse attachments; `/files`
/// with a malformed link; an anonymous reader's signed link; PDF is a
/// download.
#[tokio::test]
async fn review_fixes() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(&pool, w.dev, &["view_wiki_pages", "edit_wiki_pages"]).await;
    let a = w.alice_ctx;
    let issue = w.issue(w.web, "review").await;
    let key = key_of(&pool, issue).await;

    // The same upload twice in one op: invalid_request, as across ops.
    let u = up(&w, a, "WEB", "twice.txt", b"twice").await.unwrap();
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": u.upload_id}, {"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidRequest, "{e:#}");

    // upload_unknown on create_issue, add_note and wiki_put; and for an
    // upload whose blob is gone.
    let nobody = json!([{"upload_id": Uuid::now_v7()}]);
    for ops in [
        json!([{"op_id": Uuid::now_v7(), "kind": "create_issue", "project_id": w.web,
                "tracker_id": w.bug, "fields": {"subject": "s"}, "attachments": nobody}]),
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue}, "body": "b", "attachments": nobody}]),
        json!([{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page": "P"}, "body": "b", "attachments": nobody}]),
    ] {
        let e = run(&w, a, ops).await.unwrap_err();
        assert_eq!(kind(&e), "upload_unknown", "{e:#}");
    }
    let gone = up(&w, a, "WEB", "gone.txt", b"gone").await.unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE attachment_blobs SET state = 'purged', purged_at = now(), purge_reason = 'gc'
             WHERE id = (SELECT blob_id FROM uploads WHERE id = '{}')",
            gone.upload_id
        ),
    )
    .await;
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": gone.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "upload_unknown", "{e:#}");

    // A retracted note: its files leave the history, names included, and
    // nothing more is attached to it.
    let h = up(&w, a, "WEB", "hidden-name.txt", b"hidden")
        .await
        .unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue}, "body": "with a file",
                "attachments": [{"upload_id": h.upload_id}]}]),
    )
    .await
    .unwrap();
    let exec_id = r["execution_id"].as_str().unwrap().to_string();
    let note = first_note(&pool, issue).await;
    let history = |w: &World| {
        let p = serde_json::from_value(json!({"key": key, "include": ["recent_events"]})).unwrap();
        let app = w.app.clone();
        async move {
            serde_json::to_value(oxledger::issue_view::get(&app, a, p).await.unwrap())
                .unwrap()
                .to_string()
        }
    };
    assert!(history(&w).await.contains("hidden-name.txt"));
    exec(
        &pool,
        &format!(
            "UPDATE issue_notes SET retracted_at = now(), retracted_by_execution_id = '{exec_id}'
             WHERE id = '{note}'"
        ),
    )
    .await;
    assert!(!history(&w).await.contains("hidden-name.txt"));
    let more = up(&w, a, "WEB", "more.txt", b"more").await.unwrap();
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"note": note},
                "attachments": [{"upload_id": more.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");

    // A closed project: project_not_writable, for attach and delete.
    let pdf = up(&w, a, "WEB", "report.pdf", b"%PDF-1.4\n%%EOF\n")
        .await
        .unwrap();
    assert_eq!(pdf.content_type, "application/octet-stream");
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": pdf.upload_id}]}]),
    )
    .await
    .unwrap();
    let pdf_id = ids_of(&r)[0];
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": more.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["reason"], "project_not_writable", "{e:#}");
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": pdf_id}}]),
    )
    .await
    .unwrap_err();
    assert_eq!(details(&e)["reason"], "project_not_writable", "{e:#}");
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'active' WHERE id = '{}'",
            w.web
        ),
    )
    .await;

    // An owner's revision of an agent's proposal takes no attachments.
    let d = w.delegate(w.web).await.unwrap();
    let (_, agent) = w.token(d.context_id).await;
    let lv = lock_version(&pool, issue).await as i32;
    let p = oxledger::changesets::command_submit(
        &w.app,
        agent,
        Uuid::now_v7(),
        &note_doc(issue, lv, "proposed"),
        true,
        ORIGIN,
    )
    .await
    .unwrap();
    let cs: Uuid =
        serde_json::from_value(serde_json::to_value(p).unwrap()["changeset_id"].clone()).unwrap();
    let mut revised = note_doc(issue, lv, "revised");
    revised["ops"][0]["attachments"] = json!([{"upload_id": more.upload_id}]);
    let input = json!({"expected_current_version": 1, "proposal": revised.clone()});
    let e = oxledger::changesets::owner_revise(&w.app, a, Uuid::now_v7(), cs, &input)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["reason"], "attachments_people_only", "{e:#}");
    let input = json!({"version": 1, "expected_current_version": 1, "proposal": revised,
                       "revision_review_token": "t"});
    let e = oxledger::changesets::revise_and_apply(&w.app, a, Uuid::now_v7(), cs, &input)
        .await
        .unwrap_err();
    assert_eq!(details(&e)["reason"], "attachments_people_only", "{e:#}");

    // Over HTTP: a PDF downloads; another's unreadable file and a
    // malformed signed link are not_found.
    let router = api::router(
        w.app.clone(),
        ApiConfig {
            spa_origin: ORIGIN.into(),
            session: SessionPolicy::default(),
        },
    );
    let alice = login(&router, "alice").await;
    let resp = get_raw(
        &router,
        Some(&alice),
        &format!("/api/v1/attachments/{pdf_id}/content"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cd = resp.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_string();
    assert!(cd.starts_with("attachment; "), "{cd}");
    let hidden_id: Uuid = sqlx::query_scalar("SELECT id FROM attachments WHERE note_id = $1")
        .bind(note)
        .fetch_one(&pool)
        .await
        .unwrap();
    let resp = get_raw(
        &router,
        Some(&alice),
        &format!("/api/v1/attachments/{hidden_id}/content"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    for path in [
        "/files/not-a-uuid?u=x&e=1&s=00".to_string(),
        format!("/files/{pdf_id}"),
        format!("/files/{pdf_id}?u=nope&e=soon&s=zz"),
        "/files/%FF?u=x&e=1&s=00".to_string(),
    ] {
        let resp = get_raw(&router, None, &path).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).expect("a JSON error body");
        assert!(v.to_string().contains("not_found"), "{v}");
    }

    // An anonymous reader of a public project is redirected too, and the
    // link is checked again as anonymous.
    let anon_role = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility, builtin)
         VALUES ('Anonymous','default','anonymous') RETURNING id",
    )
    .await;
    grant(&pool, anon_role, &["view_issues"]).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET is_public = true WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    attachments::install(FilesConfig {
        origin: Some("https://files.test".into()),
        ..files(dir.path())
    });
    let to = match attachments::read::content(&w.app, AuthContext::anonymous(), pdf_id)
        .await
        .unwrap()
    {
        attachments::read::Download::Redirect(to) => to,
        attachments::read::Download::File(_) => panic!("not redirected"),
    };
    let path = to.strip_prefix("https://files.test").unwrap().to_string();
    assert!(path.contains(&format!("u={}", Uuid::nil())), "{path}");
    assert_eq!(get_raw(&router, None, &path).await.status(), StatusCode::OK);
    // The project turns private: the same link no longer serves.
    exec(
        &pool,
        &format!(
            "UPDATE projects SET is_public = false WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    assert_eq!(
        get_raw(&router, None, &path).await.status(),
        StatusCode::NOT_FOUND
    );
}

fn exec_of(r: &Value) -> Uuid {
    serde_json::from_value(r["execution_id"].clone()).unwrap()
}

async fn undo_preview(w: &World, who: AuthContext, exec: Uuid) -> Value {
    serde_json::to_value(oxledger::undo::preview(&w.app, who, exec).await.unwrap()).unwrap()
}

async fn undo(w: &World, who: AuthContext, exec: Uuid) -> anyhow::Result<Value> {
    oxledger::undo::apply(
        &w.app,
        who,
        Uuid::now_v7(),
        exec,
        &oxledger::undo::UndoInput::default(),
    )
    .await
    .map(|r| serde_json::to_value(r).unwrap())
}

/// R-A7 (U18): a person's delete_attachment is undone within 30 days (the
/// file comes back), and that undo is undone (deleted again); expired,
/// redacted, closed projects and others' executions are refused.
#[tokio::test]
async fn undo_delete() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(&pool, w.dev, &["view_wiki_pages", "edit_wiki_pages"]).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let issue = w.issue(w.web, "undo").await;
    let key = key_of(&pool, issue).await;
    let attach = |name: &'static str, bytes: &'static [u8]| {
        let w = &w;
        let key = key.clone();
        async move {
            let u = up(w, a, "WEB", name, bytes).await.unwrap();
            let r = run(
                w,
                a,
                json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                        "attachments": [{"upload_id": u.upload_id}]}]),
            )
            .await
            .unwrap();
            ids_of(&r)[0]
        }
    };
    let del = |id: Uuid| {
        let w = &w;
        async move {
            let r = run(
                w,
                a,
                json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                        "target": {"attachment": id}}]),
            )
            .await
            .unwrap();
            exec_of(&r)
        }
    };

    // Delete, preview, put back.
    let f = attach("back.txt", b"back").await;
    let d = del(f).await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["attachment_id"], json!(f));
    assert_eq!(pv["attachments"][0]["change"], "deleted");
    assert_eq!(pv["attachments"][0]["issue_key"], json!(key));
    assert_eq!(pv["attachments"][0]["restorable"], true, "{pv}");
    // Only the person who deleted it undoes it.
    let e = oxledger::undo::preview(&w.app, bob, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let r = undo(&w, a, d).await.unwrap();
    assert_eq!(r["attachments_restored"], 1);
    let restore = exec_of(&r);
    attachments::read::get(&w.app, a, f).await.unwrap();
    let v = view(&w, a, &key).await;
    assert_eq!(v["attachments"][0]["id"], json!(f));
    // Once.
    let e = undo(&w, a, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "changed_since");
    // The undo's undo deletes it again.
    let pv = undo_preview(&w, a, restore).await;
    assert_eq!(pv["attachments"][0]["change"], "restored");
    let r = undo(&w, a, restore).await.unwrap();
    assert_eq!(r["attachments_restored"], 1);
    let e = attachments::read::get(&w.app, a, f).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM issue_events WHERE issue_id = $1 AND kind LIKE 'attachment%' ORDER BY seq",
    )
    .bind(issue)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        [
            "attachment_added",
            "attachment_removed",
            "attachment_added",
            "attachment_removed"
        ]
    );

    // Older than 30 days.
    let old = attach("old.txt", b"old").await;
    let d = del(old).await;
    exec(
        &pool,
        &format!(
            "ALTER TABLE command_executions DISABLE TRIGGER command_executions_append_only;
             UPDATE command_executions SET created_at = now() - interval '31 days' WHERE id = '{d}';
             ALTER TABLE command_executions ENABLE TRIGGER command_executions_append_only"
        ),
    )
    .await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "expired");
    let e = undo(&w, a, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);

    // Redacted content does not come back.
    let red = attach("red.txt", b"red").await;
    let d = del(red).await;
    exec(
        &pool,
        &format!(
            "WITH r AS (INSERT INTO attachment_redactions (attachment_id, redacted_by, reason_code, reason)
                        SELECT id, author_id, 'confidential', 'test' FROM attachments
                        WHERE id = '{red}'
                        RETURNING id)
             UPDATE attachments SET redacted_at = now(), redaction_id = (SELECT id FROM r)
             WHERE id = '{red}'"
        ),
    )
    .await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "redacted");

    // A closed project.
    let shut = attach("shut.txt", b"shut").await;
    let d = del(shut).await;
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'closed' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "project_not_writable");
    let e = undo(&w, a, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    exec(
        &pool,
        &format!(
            "UPDATE projects SET status = 'active' WHERE id = '{}'",
            w.web
        ),
    )
    .await;
    undo(&w, a, d).await.unwrap();

    // Deleted again by someone else since: changed_since.
    let again = attach("again.txt", b"again").await;
    let d = del(again).await;
    let restore = exec_of(&undo(&w, a, d).await.unwrap());
    run(
        &w,
        bob,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": again}}]),
    )
    .await
    .unwrap();
    let pv = undo_preview(&w, a, restore).await;
    assert_eq!(pv["attachments"][0]["reason"], "changed_since");

    // A wiki page's attachment.
    let u = up(&w, a, "WEB", "plan.png", PNG).await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page": "Plan"},
                "body": "# Plan", "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let att = ids_of(&r)[0];
    let d = del(att).await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["wiki_title"], "Plan");
    assert_eq!(pv["wiki"], json!([]));
    undo(&w, a, d).await.unwrap();
    let p = oxledger::wiki::page(&w.app, a, "WEB", "Plan")
        .await
        .unwrap();
    assert_eq!(p.attachments.len(), 1);
}

/// The undo review's findings: a later execution blocks the undo of an
/// earlier one; skip_attachments; a replayed operation; notes (private,
/// retracted); lost permission; attach is not undone; wiki events.
#[tokio::test]
async fn undo_delete_more() {
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(
        &pool,
        w.dev,
        &["view_wiki_pages", "edit_wiki_pages", "delete_wiki_pages"],
    )
    .await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let issue = w.issue(w.web, "more").await;
    let key = key_of(&pool, issue).await;
    let up_attach = |name: &'static str, target: Value| {
        let w = &w;
        async move {
            let u = up(w, a, "WEB", name, name.as_bytes()).await.unwrap();
            let r = run(
                w,
                a,
                json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": target,
                        "attachments": [{"upload_id": u.upload_id}]}]),
            )
            .await
            .unwrap();
            (ids_of(&r)[0], exec_of(&r))
        }
    };
    let del = |who: AuthContext, id: Uuid| {
        let w = &w;
        async move {
            exec_of(
                &run(
                    w,
                    who,
                    json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                            "target": {"attachment": id}}]),
                )
                .await
                .unwrap(),
            )
        }
    };

    // attach itself is not undone (delete_attachment removes a file).
    let (f, attach_exec) = up_attach("f.txt", json!({"issue": key})).await;
    let e = oxledger::undo::preview(&w.app, a, attach_exec)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);

    // E1 alice deletes, E2 alice undoes, E3 bob deletes, E4 bob undoes:
    // E2's undo is changed_since (E3, E4 came later).
    let e1 = del(a, f).await;
    let e2 = exec_of(&undo(&w, a, e1).await.unwrap());
    let e3 = del(bob, f).await;
    undo(&w, bob, e3).await.unwrap();
    let pv = undo_preview(&w, a, e2).await;
    assert_eq!(pv["attachments"][0]["reason"], "changed_since", "{pv}");
    let e = undo(&w, a, e2).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    attachments::read::get(&w.app, a, f).await.unwrap();

    // skip_attachments: nothing left to do; a replayed operation answers
    // the same and writes once.
    let (g, _) = up_attach("g.txt", json!({"issue": key})).await;
    let d = del(a, g).await;
    let skip = oxledger::undo::UndoInput {
        skip_attachments: vec![g],
        ..Default::default()
    };
    let e = oxledger::undo::apply(&w.app, a, Uuid::now_v7(), d, &skip)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    let op = Uuid::now_v7();
    let none = oxledger::undo::UndoInput::default();
    let r1 = oxledger::undo::apply(&w.app, a, op, d, &none)
        .await
        .unwrap();
    let r2 = oxledger::undo::apply(&w.app, a, op, d, &none)
        .await
        .unwrap();
    assert_eq!(r1, r2);
    let n = count(
        &pool,
        &format!("SELECT count(*) FROM execution_attachment_changes WHERE attachment_id = '{g}'"),
    )
    .await;
    assert_eq!(n, 2);
    // The second delete's row names the undo of the undo.
    let again = exec_of(&undo(&w, a, r1.execution_id).await.unwrap());
    let by: Uuid =
        sqlx::query_scalar("SELECT deleted_by_execution_id FROM attachments WHERE id = $1")
            .bind(g)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, again);

    // Lost permission: bob (a member through the Developer role) deleted
    // one; without edit_issues his undo is refused.
    let (h, _) = up_attach("h.txt", json!({"issue": key})).await;
    let d = del(bob, h).await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission IN ('edit_issues', 'edit_own_issues')",
    )
    .await;
    let pv = undo_preview(&w, bob, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "permission", "{pv}");
    let e = undo(&w, bob, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);
    assert_eq!(details(&e)["reasons"][0]["reason"], "permission");
    grant(&pool, w.dev, &["edit_issues"]).await;

    // A note's attachment: the note retracted is container_gone.
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue}, "body": "n"}]),
    )
    .await
    .unwrap();
    let note_exec = exec_of(&r);
    let note: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(issue)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (on_note, _) = up_attach("n.txt", json!({"note": note})).await;
    let d = del(a, on_note).await;
    exec(
        &pool,
        &format!(
            "UPDATE issue_notes SET retracted_at = now(), retracted_by_execution_id = '{note_exec}'
             WHERE id = '{note}'"
        ),
    )
    .await;
    let pv = undo_preview(&w, a, d).await;
    assert_eq!(pv["attachments"][0]["reason"], "container_gone", "{pv}");
    let e = undo(&w, a, d).await.unwrap_err();
    assert_eq!(details(&e)["reasons"][0]["reason"], "container_gone");

    // A private note the deleter can no longer read: left out of the
    // preview (no file name), refused.
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": issue}, "body": "secret",
                "is_private": true}]),
    )
    .await
    .unwrap();
    let _ = r;
    let private: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 AND is_private ORDER BY created_at DESC LIMIT 1",
    )
    .bind(issue)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (on_private, _) = up_attach("p-secret.txt", json!({"note": private})).await;
    let d = del(bob, on_private).await;
    exec(
        &pool,
        "DELETE FROM role_permissions WHERE permission = 'view_private_notes'",
    )
    .await;
    let pv = undo_preview(&w, bob, d).await;
    assert_eq!(pv["attachments"], json!([]), "{pv}");
    assert!(!pv.to_string().contains("p-secret.txt"));
    let e = undo(&w, bob, d).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable);

    // Wiki: the events of the delete, the undo and its undo.
    let u = up(&w, a, "WEB", "w.png", PNG).await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page": "Doc"},
                "body": "# Doc", "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let page: Uuid = serde_json::from_value(r["items"][0]["wiki_page_id"].clone()).unwrap();
    let att = ids_of(&r)[0];
    let d = del(a, att).await;
    let back = exec_of(&undo(&w, a, d).await.unwrap());
    undo(&w, a, back).await.unwrap();
    let ev: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT kind, command_execution_id FROM wiki_events
         WHERE page_id = $1 AND kind LIKE 'attachment%' ORDER BY seq",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();
    let kinds: Vec<&str> = ev.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "attachment_added",
            "attachment_removed",
            "attachment_added",
            "attachment_removed"
        ]
    );
    assert_eq!(ev[1].1, Some(d));
    assert_eq!(ev[2].1, Some(back));
}

async fn blob_of(pool: &sqlx::PgPool, attachment: Uuid) -> (Uuid, String) {
    sqlx::query_as(
        "SELECT b.id, b.state FROM attachments a JOIN attachment_blobs b ON b.id = a.blob_id
         WHERE a.id = $1",
    )
    .bind(attachment)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn attach_upload(w: &World, who: AuthContext, key: &str, upload: Uuid) -> Uuid {
    let r = run(
        w,
        who,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                "attachments": [{"upload_id": upload}]}]),
    )
    .await
    .unwrap();
    ids_of(&r)[0]
}

/// R-A8, T-M2-56, T-M2-57: the hourly clean-up and the daily orphan scan.
#[tokio::test]
async fn files_gc() {
    use attachments::gc;
    use std::time::Duration;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    let a = w.alice_ctx;
    let issue = w.issue(w.web, "gc").await;
    let key = key_of(&pool, issue).await;
    let attach = |name: &'static str| {
        let w = &w;
        let key = key.clone();
        async move {
            let u = up(w, a, "WEB", name, name.as_bytes()).await.unwrap();
            let r = run(
                w,
                a,
                json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"issue": key},
                        "attachments": [{"upload_id": u.upload_id}]}]),
            )
            .await
            .unwrap();
            ids_of(&r)[0]
        }
    };
    let live = attach("live.txt").await;
    let gone = attach("gone.txt").await;
    let d = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": gone}}]),
    )
    .await
    .unwrap();
    let delete_exec = exec_of(&d);
    let path = |blob: Uuid| attachments::blob_path(dir.path(), w.web, blob);
    let (live_blob, _) = blob_of(&pool, live).await;
    let (gone_blob, _) = blob_of(&pool, gone).await;
    assert!(path(gone_blob).exists());

    // 29 days after the delete: kept (an undo may still put it back).
    let age = |days: i32| {
        let pool = pool.clone();
        async move {
            exec(
                &pool,
                &format!(
                    "UPDATE attachments SET deleted_at = now() - make_interval(days => {days})
                     WHERE id = '{gone}'"
                ),
            )
            .await;
        }
    };
    age(29).await;
    // An unused upload, expired.
    let stale = up(&w, a, "WEB", "stale.txt", b"stale").await.unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE uploads SET expires_at = now() - interval '1 second' WHERE id = '{}'",
            stale.upload_id
        ),
    )
    .await;
    let stale_blob: Uuid = sqlx::query_scalar("SELECT blob_id FROM uploads WHERE id = $1")
        .bind(stale.upload_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    // An upload not used yet, not expired: kept.
    let pending = up(&w, a, "WEB", "pending.txt", b"pending").await.unwrap();
    let r = gc::hourly(&pool, dir.path()).await.unwrap();
    assert_eq!(r.uploads_removed, 1, "{r:?}");
    assert_eq!(r.blobs_purged, 1, "{r:?}");
    assert_eq!(r.files_failed, 0);
    assert!(!path(stale_blob).exists());
    assert_eq!(blob_of(&pool, gone).await.1, "present");
    assert!(path(gone_blob).exists());
    assert_eq!(blob_of(&pool, live).await.1, "present");
    let n = count(
        &pool,
        &format!(
            "SELECT count(*) FROM uploads WHERE id = '{}'",
            pending.upload_id
        ),
    )
    .await;
    assert_eq!(n, 1);

    // 31 days: purged, the file removed; the undo then has nothing to put
    // back.
    age(31).await;
    let r = gc::hourly(&pool, dir.path()).await.unwrap();
    assert_eq!(r.blobs_purged, 1, "{r:?}");
    assert_eq!(blob_of(&pool, gone).await.1, "purged");
    assert!(!path(gone_blob).exists());
    assert!(path(live_blob).exists());
    let purge_reason: String =
        sqlx::query_scalar("SELECT purge_reason FROM attachment_blobs WHERE id = $1")
            .bind(gone_blob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(purge_reason, "gc");
    // (The execution itself is recent: only the content is gone.)
    let pv = undo_preview(&w, a, delete_exec).await;
    assert_eq!(pv["attachments"][0]["reason"], "content_gone", "{pv}");
    let e = undo(&w, a, delete_exec).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::UndoNotApplicable, "{e:#}");
    assert!(attachments::read::get(&w.app, a, gone).await.is_err());

    // An old delete does not purge a generation another row still holds:
    // an unused upload of it, or a delete within the window.
    let held = attach("held.txt").await;
    let (held_blob, _) = blob_of(&pool, held).await;
    run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": held}}]),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE attachments SET deleted_at = now() - interval '31 days' WHERE id = '{held}'"
        ),
    )
    .await;
    let reup = up(&w, a, "WEB", "held2.txt", b"held.txt").await.unwrap();
    let reup_blob: Uuid = sqlx::query_scalar("SELECT blob_id FROM uploads WHERE id = $1")
        .bind(reup.upload_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reup_blob, held_blob);
    let r = gc::hourly(&pool, dir.path()).await.unwrap();
    assert_eq!(r.blobs_purged, 0, "{r:?}");
    assert_eq!(blob_of(&pool, held).await.1, "present");
    let twin = attach_upload(&w, a, &key, reup.upload_id).await;
    run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "delete_attachment",
                "target": {"attachment": twin}}]),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &format!(
            "UPDATE attachments SET deleted_at = now() - interval '5 days' WHERE id = '{twin}'"
        ),
    )
    .await;
    let r = gc::hourly(&pool, dir.path()).await.unwrap();
    assert_eq!(r.blobs_purged, 0, "{r:?}");
    assert!(path(held_blob).exists());
    // The same content again: a new generation (R-A12's rule for any
    // purge).
    let again = attach("gone.txt").await;
    let (again_blob, state) = blob_of(&pool, again).await;
    assert_ne!(again_blob, gone_blob);
    assert_eq!(state, "present");
    // Nothing more to do.
    let r = gc::hourly(&pool, dir.path()).await.unwrap();
    assert_eq!(r.blobs_purged, 0, "{r:?}");

    // T-M2-57: a file whose transaction never committed, a dead temporary
    // file and a stray name; young files are left alone.
    let orphan = path(Uuid::now_v7());
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();
    let tmp = dir.path().join(".tmp").join("dead.part");
    std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    std::fs::write(&tmp, b"dead").unwrap();
    std::fs::write(dir.path().join("README"), b"x").unwrap();
    let hour = Duration::from_secs(3600);
    let s = gc::orphan_scan(&pool, dir.path(), hour, hour)
        .await
        .unwrap();
    assert_eq!((s.orphans_removed, s.temp_removed), (0, 0), "{s:?}");
    assert!(orphan.exists() && tmp.exists());
    let s = gc::orphan_scan(&pool, dir.path(), Duration::ZERO, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(s.orphans_removed, 1, "{s:?}");
    assert_eq!(s.temp_removed, 1, "{s:?}");
    assert_eq!(s.unknown, 1, "{s:?}");
    assert!(!orphan.exists() && !tmp.exists());
    // Present generations stay.
    assert!(path(live_blob).exists());
    assert!(path(again_blob).exists());
    let pending_blob: Uuid = sqlx::query_scalar("SELECT blob_id FROM uploads WHERE id = $1")
        .bind(pending.upload_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(path(pending_blob).exists());
}

fn why() -> oxledger::redaction::RedactInput {
    oxledger::redaction::RedactInput {
        reason_code: "confidential".into(),
        reason: "pasted by mistake".into(),
        attachments: Vec::new(),
    }
}

async fn redact(
    w: &World,
    who: AuthContext,
    target: oxledger::redaction::Target,
    input: &oxledger::redaction::RedactInput,
) -> anyhow::Result<oxledger::redaction::Redacted> {
    oxledger::redaction::redact(&w.app, who, &w.policy, Uuid::now_v7(), &target, input).await
}

async fn downloadable(w: &World, who: AuthContext, id: Uuid) -> bool {
    match attachments::read::content(&w.app, who, id).await {
        Ok(_) => true,
        Err(e) => {
            assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
            false
        }
    }
}

/// §3.13, R-A12: T-M2-55 (one content, every attachment of it), T-M2-58
/// (uploaded again: a new generation), T-M2-21a (a note's attachments),
/// T-M2-21b (the description's: only the ticked ones).
#[tokio::test]
async fn attachment_redaction() {
    use oxledger::redaction::Target;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    grant(&pool, w.dev, &["manage_project"]).await;
    let a = w.alice_ctx;
    let bob = sign_in(&pool, &w.policy, "bob").await;
    let one = w.issue(w.web, "one").await;
    let secret = w.issue(w.web, "secret").await;
    exec(
        &pool,
        &format!("UPDATE issues SET is_private = true WHERE id = '{secret}'"),
    )
    .await;
    let attach = |who: AuthContext, issue: Uuid, name: &'static str, bytes: &'static [u8]| {
        let w = &w;
        let pool = pool.clone();
        async move {
            let u = up(w, who, "WEB", name, bytes).await.unwrap();
            let r = run(
                w,
                who,
                json!([{"op_id": Uuid::now_v7(), "kind": "attach",
                        "target": {"issue": key_of(&pool, issue).await},
                        "attachments": [{"upload_id": u.upload_id}]}]),
            )
            .await
            .unwrap();
            (ids_of(&r)[0], u)
        }
    };
    let blob = |id: Uuid| {
        let pool = pool.clone();
        async move { blob_of(&pool, id).await }
    };

    // T-M2-55: the same content on another, private issue goes too.
    let (leak, _) = attach(a, one, "token.txt", b"api key 123").await;
    let (twin, _) = attach(a, secret, "copy.txt", b"api key 123").await;
    let (gen1, _) = blob(leak).await;
    assert_eq!(blob(twin).await.0, gen1);
    let file1 = attachments::blob_path(dir.path(), w.web, gen1);
    assert!(file1.exists());
    let epoch = |pool: sqlx::PgPool| async move {
        count(
            &pool,
            "SELECT security_epoch FROM projects WHERE key = 'WEB'",
        )
        .await
    };
    let before = epoch(pool.clone()).await;
    let r = redact(&w, a, Target::Attachment { id: leak }, &why())
        .await
        .unwrap();
    assert!(r.changed);
    assert_eq!(r.security_epoch, before + 1);
    assert_eq!(epoch(pool.clone()).await, before + 1);
    assert!(!downloadable(&w, a, leak).await);
    assert!(!downloadable(&w, a, twin).await);
    assert_eq!(blob(leak).await.1, "purged");
    assert!(!file1.exists());
    let m = attachments::read::get(&w.app, a, leak).await.unwrap();
    assert_eq!(m.meta.filename, "[已遮蔽]");
    assert!(m.meta.redacted);
    let m = attachments::read::get(&w.app, a, twin).await.unwrap();
    assert_eq!(m.meta.filename, "copy.txt");
    assert!(m.meta.redacted);
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'filename' FROM issue_events
         WHERE kind LIKE 'attachment%' AND payload->>'attachment_id' = $1::text",
    )
    .bind(leak)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(names, ["[已遮蔽]"]);
    // Again: nothing new.
    let r = redact(&w, a, Target::Attachment { id: leak }, &why())
        .await
        .unwrap();
    assert!(!r.changed);
    // Someone who cannot see it gets not_found; without manage_project a
    // refusal.
    let z = oxledger::auth::create_user(&pool, "zed", "zed", PW, false)
        .await
        .unwrap();
    let viewer = id(
        &pool,
        "INSERT INTO roles (name, issues_visibility) VALUES ('Viewer','default') RETURNING id",
    )
    .await;
    grant(&pool, viewer, &["view_issues"]).await;
    member(&pool, w.web, z, viewer).await;
    let zed = sign_in(&pool, &w.policy, "zed").await;
    let e = redact(&w, zed, Target::Attachment { id: twin }, &why())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let (plain, _) = attach(a, one, "plain.txt", b"plain").await;
    let e = redact(&w, zed, Target::Attachment { id: plain }, &why())
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::ForbiddenOp);

    // T-M2-58: the same content again, by alice and by bob: ordinary
    // uploads, a new generation, downloadable; the old ones stay redacted.
    let (again, ua) = attach(a, one, "again.txt", b"api key 123").await;
    let (again_b, ub) = attach(bob, one, "again-b.txt", b"api key 123").await;
    assert_eq!(ua.content_type, "application/octet-stream");
    assert_eq!(ub.size, ua.size);
    let (gen2, state) = blob(again).await;
    assert_ne!(gen2, gen1);
    assert_eq!(state, "present");
    assert_eq!(blob(again_b).await.0, gen2);
    assert!(downloadable(&w, a, again).await);
    assert!(!downloadable(&w, a, leak).await);

    // T-M2-21a: a note's attachments go with the note.
    let u = up(&w, a, "WEB", "note.png", PNG).await.unwrap();
    run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": one}, "body": "see",
                "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let note: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(one)
    .fetch_one(&pool)
    .await
    .unwrap();
    let on_note: Uuid = sqlx::query_scalar("SELECT id FROM attachments WHERE note_id = $1")
        .bind(note)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(downloadable(&w, a, on_note).await);
    let key = key_of(&pool, one).await;
    redact(
        &w,
        a,
        Target::Note {
            key: key.clone(),
            note,
        },
        &why(),
    )
    .await
    .unwrap();
    assert!(!downloadable(&w, a, on_note).await);
    assert_eq!(blob(on_note).await.1, "purged");

    // T-M2-21b: an issue event's redaction takes the ticked issue-level
    // attachments only.
    let (x, _) = attach(a, one, "x.txt", b"xx").await;
    let (y, _) = attach(a, one, "y.txt", b"yy").await;
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND note_id IS NULL ORDER BY seq LIMIT 1",
    )
    .bind(one)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Only this issue's issue-level attachments.
    let mut bad = why();
    bad.attachments = vec![on_note];
    let e = redact(
        &w,
        a,
        Target::IssueEvent {
            key: key.clone(),
            event,
        },
        &bad,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
    bad.attachments = vec![twin];
    let e = redact(
        &w,
        a,
        Target::IssueEvent {
            key: key.clone(),
            event,
        },
        &bad,
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
    // Not with other targets.
    let e = redact(&w, a, Target::Attachment { id: x }, &bad)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");
    let mut tick = why();
    tick.attachments = vec![x];
    redact(&w, a, Target::IssueEvent { key, event }, &tick)
        .await
        .unwrap();
    assert!(!downloadable(&w, a, x).await);
    assert!(downloadable(&w, a, y).await);
}

/// Review fixes: an attachment marked only through shared content is still
/// redacted by name later; a redacted note takes no new attachments; an
/// issue event redacted before still takes ticked attachments; a wiki
/// attachment; the tick limit.
#[tokio::test]
async fn attachment_redaction_more() {
    use oxledger::redaction::Target;
    let pool = test_pool().await;
    let w = World::build(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    attachments::install(files(dir.path()));
    exec(
        &pool,
        "INSERT INTO enabled_modules (project_id, module) SELECT id, 'wiki' FROM projects",
    )
    .await;
    grant(
        &pool,
        w.dev,
        &[
            "manage_project",
            "view_wiki_pages",
            "view_wiki_edits",
            "edit_wiki_pages",
        ],
    )
    .await;
    let a = w.alice_ctx;
    let one = w.issue(w.web, "one").await;
    let two = w.issue(w.web, "two").await;
    let key = key_of(&pool, one).await;
    let attach = |issue: Uuid, name: &'static str, bytes: &'static [u8]| {
        let w = &w;
        let pool = pool.clone();
        async move {
            let u = up(w, a, "WEB", name, bytes).await.unwrap();
            attach_upload(w, a, &key_of(&pool, issue).await, u.upload_id).await
        }
    };
    let epoch = || {
        let pool = pool.clone();
        async move {
            count(
                &pool,
                "SELECT security_epoch FROM projects WHERE key = 'WEB'",
            )
            .await
        }
    };
    let name_of = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT filename FROM attachments WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };

    // Shared content: the twin is marked, keeps its name, and can still be
    // redacted by name (its name and events then go).
    let leak = attach(one, "leak.txt", b"same secret").await;
    let twin = attach(two, "twin-secret-name.txt", b"same secret").await;
    redact(&w, a, Target::Attachment { id: leak }, &why())
        .await
        .unwrap();
    assert_eq!(name_of(twin).await, "twin-secret-name.txt");
    let before = epoch().await;
    let r = redact(&w, a, Target::Attachment { id: twin }, &why())
        .await
        .unwrap();
    assert!(r.changed);
    assert_eq!(r.security_epoch, before + 1);
    assert_eq!(name_of(twin).await, "[已遮蔽]");
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'filename' FROM issue_events
         WHERE issue_id = $1 AND payload->>'attachment_id' = $2::text",
    )
    .bind(two)
    .bind(twin)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(names, ["[已遮蔽]"]);
    let used: i64 = count(
        &pool,
        &format!(
            "SELECT count(*) FROM uploads WHERE blob_id = (SELECT blob_id FROM attachments
             WHERE id = '{twin}') AND filename <> '[已遮蔽]'"
        ),
    )
    .await;
    assert_eq!(used, 0);
    let r = redact(&w, a, Target::Attachment { id: twin }, &why())
        .await
        .unwrap();
    assert!(!r.changed);

    // A redacted note takes no new attachments; again: nothing new.
    let u = up(&w, a, "WEB", "n.txt", b"note file").await.unwrap();
    run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "add_note",
                "target": {"type": "issue", "id": one}, "body": "see",
                "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let note: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_notes WHERE issue_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(one)
    .fetch_one(&pool)
    .await
    .unwrap();
    redact(
        &w,
        a,
        Target::Note {
            key: key.clone(),
            note,
        },
        &why(),
    )
    .await
    .unwrap();
    let late = up(&w, a, "WEB", "late.txt", b"late").await.unwrap();
    let e = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "attach", "target": {"note": note},
                "attachments": [{"upload_id": late.upload_id}]}]),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:#}");
    let r = redact(
        &w,
        a,
        Target::Note {
            key: key.clone(),
            note,
        },
        &why(),
    )
    .await
    .unwrap();
    assert!(!r.changed);

    // An issue event redacted first, attachments ticked later.
    let x = attach(one, "x.txt", b"x content").await;
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM issue_events WHERE issue_id = $1 AND note_id IS NULL ORDER BY seq LIMIT 1",
    )
    .bind(one)
    .fetch_one(&pool)
    .await
    .unwrap();
    let r = redact(
        &w,
        a,
        Target::IssueEvent {
            key: key.clone(),
            event,
        },
        &why(),
    )
    .await
    .unwrap();
    assert!(r.changed);
    assert!(downloadable(&w, a, x).await);
    let mut tick = why();
    tick.attachments = vec![x];
    let before = epoch().await;
    let r = redact(
        &w,
        a,
        Target::IssueEvent {
            key: key.clone(),
            event,
        },
        &tick,
    )
    .await
    .unwrap();
    assert!(r.changed);
    assert_eq!(r.security_epoch, before + 1);
    assert!(!downloadable(&w, a, x).await);
    let r = redact(
        &w,
        a,
        Target::IssueEvent {
            key: key.clone(),
            event,
        },
        &tick,
    )
    .await
    .unwrap();
    assert!(!r.changed);
    let mut many = why();
    many.attachments = (0..=oxledger::redaction::MAX_TICKED)
        .map(|_| Uuid::now_v7())
        .collect();
    let e = redact(&w, a, Target::IssueEvent { key, event }, &many)
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidValue, "{e:#}");

    // A wiki attachment: its page's events carry the marker.
    let u = up(&w, a, "WEB", "wiki-secret.png", PNG).await.unwrap();
    let r = run(
        &w,
        a,
        json!([{"op_id": Uuid::now_v7(), "kind": "wiki_put",
                "target": {"project": "WEB", "page": "Plan"},
                "body": "# Plan", "attachments": [{"upload_id": u.upload_id}]}]),
    )
    .await
    .unwrap();
    let page: Uuid = serde_json::from_value(r["items"][0]["wiki_page_id"].clone()).unwrap();
    let wa = ids_of(&r)[0];
    assert!(downloadable(&w, a, wa).await);
    let r = redact(&w, a, Target::Attachment { id: wa }, &why())
        .await
        .unwrap();
    assert!(r.changed);
    assert!(!downloadable(&w, a, wa).await);
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT payload->>'filename' FROM wiki_events
         WHERE page_id = $1 AND kind LIKE 'attachment%'",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(names, ["[已遮蔽]"]);
    let n = count(
        &pool,
        &format!("SELECT count(*) FROM outbox WHERE topic = 'wiki_page' AND entity_id = '{page}'"),
    )
    .await;
    assert!(n >= 1);
}
