// SPDX-License-Identifier: GPL-2.0-or-later
//! One streamed model request (design 05 §3.13.4): the deadlines (first
//! byte, gap between chunks, whole request), the 8 MiB cap, cancellation,
//! and the decoding into [`ModelEvent`]s. Without feature `ai` there is no
//! transport and every call is `NotConfigured`.

use super::config::{AiConfig, Endpoint};
#[cfg(feature = "ai")]
use super::protocol::ModelEvent;
use super::protocol::{self, Kind, ModelRequest, Stop};
#[cfg(feature = "ai")]
use super::sse::SseParser;
use serde_json::Value as Json;
use tokio::sync::watch;

/// Why a model request failed. Nothing here carries provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// P-3 refused the address.
    Blocked,
    /// No transport (feature `ai` off), no key where one is needed, or a
    /// TLS set-up problem.
    NotConfigured,
    /// The connection failed (no answer, refused, reset).
    Connect,
    Timeout,
    /// A non-2xx status (3xx included: redirects are not followed).
    Status(u16),
    Malformed,
    TooLarge,
    Canceled,
}

impl CallError {
    /// `ai_provider_error.details` (§5): `{kind, status?}`; `None` for
    /// errors that are `ai_unavailable` or a cancel.
    pub fn details(self) -> Option<Json> {
        Some(match self {
            CallError::Timeout | CallError::Connect => serde_json::json!({"kind": "timeout"}),
            CallError::Status(s) => serde_json::json!({"kind": "http_status", "status": s}),
            CallError::Malformed | CallError::TooLarge => serde_json::json!({"kind": "malformed"}),
            CallError::Blocked | CallError::NotConfigured | CallError::Canceled => return None,
        })
    }

    /// Counts against the circuit breaker (§3.13.4).
    pub fn is_failure(self) -> bool {
        !matches!(self, CallError::Canceled)
    }
}

/// What one model turn produced.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<(String, String, Json)>,
    pub stop: Option<Stop>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// The request headers for `kind` (the key only goes to the registered
/// endpoint; an OpenAI-compatible endpoint without a key gets no header).
pub fn headers(kind: Kind, ep: &Endpoint) -> Result<Vec<(&'static str, String)>, CallError> {
    Ok(match kind {
        Kind::Anthropic => {
            let key = ep.key.as_ref().ok_or(CallError::NotConfigured)?;
            vec![
                ("x-api-key", key.expose().to_string()),
                ("anthropic-version", protocol::ANTHROPIC_VERSION.to_string()),
            ]
        }
        Kind::OpenAiCompat => match &ep.key {
            Some(k) => vec![("authorization", format!("Bearer {}", k.expose()))],
            None => Vec::new(),
        },
    })
}

/// Sends `req` and streams the answer; `on_text` sees each text delta.
#[cfg(feature = "ai")]
pub async fn call(
    cfg: &AiConfig,
    ep: &Endpoint,
    kind: Kind,
    req: &ModelRequest,
    cancel: &mut watch::Receiver<bool>,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> Result<Turn, CallError> {
    let l = &cfg.limits;
    let hdrs = headers(kind, ep)?;
    let body = serde_json::to_vec(&protocol::body(kind, req)).map_err(|_| CallError::Malformed)?;
    let path = kind.path(&ep.base_path);
    let whole = tokio::time::Instant::now() + l.request;
    let work = async {
        let first = tokio::time::Instant::now() + l.first_byte;
        let mut opened =
            tokio::time::timeout_at(first, super::http::post(cfg, ep, &path, &hdrs, body))
                .await
                .map_err(|_| CallError::Timeout)??;
        if !(200..300).contains(&opened.status) {
            return Err(CallError::Status(opened.status));
        }
        let mut parser = SseParser::new();
        let mut dec = protocol::decoder(kind);
        let mut turn = Turn::default();
        let mut read = 0usize;
        let mut first_chunk = true;
        loop {
            let until = if first_chunk {
                first
            } else {
                tokio::time::Instant::now() + l.chunk_gap
            };
            let next = tokio::time::timeout_at(until, opened.next())
                .await
                .map_err(|_| CallError::Timeout)?;
            first_chunk = false;
            let events = match next {
                None => {
                    let mut evs = Vec::new();
                    if let Some(e) = parser.finish().map_err(|_| CallError::Malformed)? {
                        evs.extend(dec.on_event(&e).map_err(decode_err)?);
                    }
                    evs.extend(dec.finish().map_err(decode_err)?);
                    apply(&mut turn, evs, on_text);
                    return Ok(turn);
                }
                Some(chunk) => {
                    let chunk = chunk?;
                    read += chunk.len();
                    if read > l.max_response_bytes {
                        return Err(CallError::TooLarge);
                    }
                    let mut evs = Vec::new();
                    for e in parser.push(&chunk).map_err(|_| CallError::Malformed)? {
                        evs.extend(dec.on_event(&e).map_err(decode_err)?);
                    }
                    evs
                }
            };
            apply(&mut turn, events, on_text);
        }
    };
    tokio::select! {
        r = tokio::time::timeout_at(whole, work) => r.map_err(|_| CallError::Timeout)?,
        _ = cancel.wait_for(|c| *c) => Err(CallError::Canceled),
    }
}

#[cfg(not(feature = "ai"))]
pub async fn call(
    _: &AiConfig,
    ep: &Endpoint,
    kind: Kind,
    _: &ModelRequest,
    _: &mut watch::Receiver<bool>,
    _: &mut (dyn FnMut(&str) + Send),
) -> Result<Turn, CallError> {
    headers(kind, ep)?;
    Err(CallError::NotConfigured)
}

#[cfg(feature = "ai")]
fn decode_err(e: protocol::DecodeError) -> CallError {
    match e {
        protocol::DecodeError::Malformed => CallError::Malformed,
        protocol::DecodeError::Provider { status } => CallError::Status(status),
    }
}

#[cfg(feature = "ai")]
fn apply(turn: &mut Turn, evs: Vec<ModelEvent>, on_text: &mut (dyn FnMut(&str) + Send)) {
    for e in evs {
        match e {
            ModelEvent::Text(t) => {
                on_text(&t);
                turn.text.push_str(&t);
            }
            ModelEvent::ToolCall { id, name, input } => turn.calls.push((id, name, input)),
            ModelEvent::Usage { input, output } => {
                if input.is_some() {
                    turn.input_tokens = input;
                }
                if output.is_some() {
                    turn.output_tokens = output;
                }
            }
            ModelEvent::Stop(s) => turn.stop = Some(s),
        }
    }
}

/// Whether a build can reach providers at all.
pub const TRANSPORT: bool = cfg!(feature = "ai");
