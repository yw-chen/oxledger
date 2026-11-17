// SPDX-License-Identifier: GPL-2.0-or-later
//! `POST /api/v1/integrations/github/webhook` (§19.5). Outside `api_guard`
//! (B21): no session, no CSRF, only the signature. Bodies are kept only in
//! `github_event_jobs` until processed; nothing here logs them.

use crate::service::App;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value as Json;
use sha2::Sha256;
use std::sync::Arc;
use uuid::Uuid;

pub const PATH: &str = "/api/v1/integrations/github/webhook";
pub const MAX_BODY: usize = 25 * 1024 * 1024;
/// Design 06 §3.7: requests handled at once; one more is answered 503 at
/// once (GitHub's failed deliveries come back with the 10-minute redelivery).
pub const MAX_CONCURRENT: usize = 16;

struct Hook {
    app: Arc<App>,
    secret: Vec<u8>,
}

/// The webhook route, to merge into the main router.
pub fn routes(app: Arc<App>, secret: Vec<u8>) -> Router {
    // Load shed outside the concurrency limit: with the limit alone the
    // extra requests would wait in line (holding connections and memory).
    let shed = tower::ServiceBuilder::new()
        .layer(axum::error_handling::HandleErrorLayer::new(
            |_: tower::BoxError| async { StatusCode::SERVICE_UNAVAILABLE },
        ))
        .layer(tower::load_shed::LoadShedLayer::new())
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONCURRENT));
    Router::new()
        .route(PATH, post(receive))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(shed)
        .with_state(Arc::new(Hook { app, secret }))
}

/// `X-Hub-Signature-256: sha256=<hex>` over the raw body, compared in
/// constant time.
pub fn verify(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(sig) = header
        .and_then(|h| h.strip_prefix("sha256="))
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// The header value for `body` (tests and tools).
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn receive(State(h): State<Arc<Hook>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    if !verify(&h.secret, &body, header("x-hub-signature-256")) {
        return StatusCode::UNAUTHORIZED;
    }
    // B19: a delivery id that is not a UUID is refused, not stored.
    let Some(delivery) = header("x-github-delivery").and_then(|d| Uuid::parse_str(d).ok()) else {
        tracing::warn!("github webhook without a usable X-GitHub-Delivery");
        return StatusCode::BAD_REQUEST;
    };
    let Some(event) = header("x-github-event").map(str::to_string) else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(payload) = serde_json::from_slice::<Json>(&body) else {
        tracing::warn!(%delivery, "github webhook body is not JSON");
        return StatusCode::BAD_REQUEST;
    };
    match enqueue(&h.app, delivery, &event, &payload).await {
        Ok(true) => StatusCode::ACCEPTED,
        Ok(false) => StatusCode::OK,
        Err(e) => {
            crate::metrics::note_error(&e);
            tracing::warn!(%delivery, "github webhook not stored: {}", e.root_cause());
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// The delivery and its job in one transaction; false = seen before.
pub async fn enqueue(
    app: &App,
    delivery: Uuid,
    event: &str,
    payload: &Json,
) -> anyhow::Result<bool> {
    let mut tx = app.pool.begin().await?;
    let n = sqlx::query(
        "INSERT INTO github_deliveries (delivery_id, event) VALUES ($1, $2)
         ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(delivery)
    .bind(event)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query("INSERT INTO github_event_jobs (delivery_id, payload) VALUES ($1, $2)")
        .bind(delivery)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    super::fault("github.webhook.before_commit").await?;
    tx.commit().await?;
    Ok(true)
}
