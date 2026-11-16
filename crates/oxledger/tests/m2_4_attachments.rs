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
