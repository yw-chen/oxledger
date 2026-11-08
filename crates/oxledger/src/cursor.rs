// SPDX-License-Identifier: GPL-2.0-or-later
//! Opaque page cursors (contract 02 §4.5).
//!
//! A cursor is `base64url(nonce ‖ XChaCha20-Poly1305(json))`: encrypted, not
//! just signed, so it reveals no internal positions or ids. It is bound to the
//! principal, the query (a hash of every parameter except `cursor` and
//! `limit`) and the sort, and expires after [`CURSOR_TTL`]. It carries no
//! authorization: every page reloads the caller's scope. Any mismatch,
//! tampering or expiry is `invalid_cursor`.

use crate::auth;
use crate::error::{ErrorCode, api};
use anyhow::Result;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use uuid::Uuid;

/// How long a cursor stays valid.
pub const CURSOR_TTL: chrono::Duration = chrono::Duration::hours(24);
const VERSION: u8 = 1;

/// Where the next page starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum Position {
    /// Newest first: the last item's `(updated_at, id)`.
    Keyset { t: DateTime<Utc>, i: Uuid },
    /// Ranked full text: how many ranked items were already returned.
    Rank { o: u32 },
}

#[derive(Serialize, Deserialize)]
struct Payload {
    v: u8,
    /// Principal the cursor was issued to (None: anonymous).
    p: Option<Uuid>,
    /// Query hash (hex, 32 chars).
    h: String,
    /// Expiry (unix seconds).
    e: i64,
    #[serde(flatten)]
    pos: Position,
}

static KEY: OnceCell<[u8; 32]> = OnceCell::const_new();

/// Loads (or creates) the key once at startup, so the first paged searches do
/// not each wait for a second pool connection while holding their read
/// transaction. The key is cached for the life of the process: after
/// replacing it in `server_secrets`, restart every server process.
pub async fn init(pool: &PgPool) -> Result<()> {
    key(pool).await.map(|_| ())
}

/// The cursor key, created on first use and shared through the database.
async fn key(pool: &PgPool) -> Result<&'static [u8; 32]> {
    KEY.get_or_try_init(|| async {
        let fresh = auth::random_secret();
        sqlx::query(
            "INSERT INTO server_secrets (name, secret) VALUES ('cursor_key', $1)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(&fresh[..])
        .execute(pool)
        .await?;
        let k: Vec<u8> =
            sqlx::query_scalar("SELECT secret FROM server_secrets WHERE name = 'cursor_key'")
                .fetch_one(pool)
                .await?;
        let k: [u8; 32] = k
            .try_into()
            .map_err(|_| anyhow::anyhow!("cursor_key is not 32 bytes"))?;
        anyhow::Ok(k)
    })
    .await
}

/// Hash of a query's identity: a canonical JSON value of every parameter that
/// changes the result set or its order (serde_json maps are sorted).
pub fn query_hash(query: &serde_json::Value) -> String {
    let digest = Sha256::digest(query.to_string().as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn encode(
    pool: &PgPool,
    principal: Option<Uuid>,
    hash: &str,
    pos: Position,
) -> Result<String> {
    let payload = Payload {
        v: VERSION,
        p: principal,
        h: hash.to_string(),
        e: (Utc::now() + CURSOR_TTL).timestamp(),
        pos,
    };
    let plain = serde_json::to_vec(&payload)?;
    let cipher = XChaCha20Poly1305::new(&Key::from(*key(pool).await?));
    let mut nonce = [0u8; 24];
    rand::rng().fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(&XNonce::from(nonce), plain.as_slice())
        .map_err(|_| anyhow::anyhow!("cursor encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend(sealed);
    Ok(auth::encode(&out))
}

fn invalid() -> anyhow::Error {
    api(
        ErrorCode::InvalidCursor,
        "invalid or expired cursor; search again without it",
    )
}

pub async fn decode(
    pool: &PgPool,
    raw: &str,
    principal: Option<Uuid>,
    hash: &str,
) -> Result<Position> {
    let bytes = auth::decode_bytes(raw.trim()).ok_or_else(invalid)?;
    if bytes.len() < 24 + 16 || bytes.len() > 1024 {
        return Err(invalid());
    }
    let (nonce, sealed) = bytes.split_at(24);
    let cipher = XChaCha20Poly1305::new(&Key::from(*key(pool).await?));
    let plain = cipher
        .decrypt(&XNonce::from(<[u8; 24]>::try_from(nonce)?), sealed)
        .map_err(|_| invalid())?;
    let p: Payload = serde_json::from_slice(&plain).map_err(|_| invalid())?;
    if p.v != VERSION || p.p != principal || p.h != hash || p.e < Utc::now().timestamp() {
        return Err(invalid());
    }
    Ok(p.pos)
}
