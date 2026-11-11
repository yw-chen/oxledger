// SPDX-License-Identifier: GPL-2.0-or-later
//! A fake model provider for the M1.11 suites: an HTTP server in the test
//! that speaks the Anthropic and the OpenAI-compatible streaming protocols
//! with scripted answers and records every request (the same server as in
//! `m1_10_ai_run.rs`, shared here so the gate and the E5 harness can use
//! it). Needs feature `ai`.
#![allow(dead_code)]

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub enum Reply {
    Sse(String),
    /// A status with a body the provider would write (never shown).
    Status(u16),
    /// Waits before answering.
    Slow(Duration, String),
    Redirect(String),
}

#[derive(Clone, Debug)]
pub struct Req {
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
    pub raw: String,
}

/// Scripted replies, or a `responder` that answers from the request.
#[derive(Default)]
pub struct Fake {
    pub replies: Mutex<VecDeque<Reply>>,
    pub requests: Mutex<Vec<Req>>,
    #[allow(clippy::type_complexity)]
    pub responder: Mutex<Option<Box<dyn Fn(&Req) -> Reply + Send + Sync>>>,
}

impl Fake {
    pub fn push(&self, r: Reply) {
        self.replies.lock().unwrap().push_back(r);
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    pub fn raw(&self) -> String {
        self.requests()
            .iter()
            .map(|r| r.raw.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
        self.replies.lock().unwrap().clear();
    }
}

pub const RAW_ERROR: &str = "PROVIDER-RAW-ERROR-TEXT";

async fn handle(State(f): State<Arc<Fake>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let raw = String::from_utf8_lossy(&body).to_string();
    let path = uri.path().to_string();
    let req = Req {
        path: path.clone(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: serde_json::from_str(&raw).unwrap_or(Value::Null),
        raw,
    };
    f.requests.lock().unwrap().push(req.clone());
    let p = if path.ends_with("/chat/completions") {
        P::Oai
    } else {
        P::Anth
    };
    let scripted = f.replies.lock().unwrap().pop_front();
    let reply = match scripted {
        Some(r) => r,
        None => match f.responder.lock().unwrap().as_ref() {
            Some(answer) => answer(&req),
            None => turn(p, &[text("ok")], Some((10, 2))),
        },
    };
    let sse = |s: String| ([(header::CONTENT_TYPE, "text/event-stream")], s).into_response();
    match reply {
        Reply::Sse(s) => sse(s),
        Reply::Status(code) => (
            StatusCode::from_u16(code).unwrap(),
            format!("{{\"error\": {{\"message\": \"{RAW_ERROR} {code}\"}}}}"),
        )
            .into_response(),
        Reply::Slow(d, s) => {
            tokio::time::sleep(d).await;
            sse(s)
        }
        Reply::Redirect(to) => (StatusCode::FOUND, [(header::LOCATION, to)], "").into_response(),
    }
}

/// Serves `f` on 127.0.0.1; returns the port.
pub async fn serve(f: Arc<Fake>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = Router::new().fallback(handle).with_state(f);
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    port
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum P {
    Anth,
    Oai,
}

pub enum Step {
    Text(String),
    Call(String, String, Value),
}

pub fn text(s: &str) -> Step {
    Step::Text(s.into())
}

pub fn call(id: &str, name: &str, input: Value) -> Step {
    Step::Call(id.into(), name.into(), input)
}

fn ev(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// One model turn as the provider streams it; `usage` = (input, output).
pub fn turn(p: P, steps: &[Step], usage: Option<(u64, u64)>) -> Reply {
    let calls = steps.iter().any(|s| matches!(s, Step::Call(..)));
    let mut s = String::new();
    match p {
        P::Anth => {
            let (i, o) = usage.unwrap_or((0, 0));
            s += &ev(
                "message_start",
                json!({"type": "message_start", "message": {"usage": {"input_tokens": i, "output_tokens": 1}}}),
            );
            for (n, st) in steps.iter().enumerate() {
                match st {
                    Step::Text(t) => {
                        s += &ev(
                            "content_block_start",
                            json!({"type": "content_block_start", "index": n,
                            "content_block": {"type": "text", "text": ""}}),
                        );
                        s += &ev(
                            "content_block_delta",
                            json!({"type": "content_block_delta", "index": n,
                            "delta": {"type": "text_delta", "text": t}}),
                        );
                    }
                    Step::Call(id, name, input) => {
                        s += &ev(
                            "content_block_start",
                            json!({"type": "content_block_start", "index": n,
                            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                        );
                        s += &ev(
                            "content_block_delta",
                            json!({"type": "content_block_delta", "index": n,
                            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
                        );
                    }
                }
                s += &ev(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": n}),
                );
            }
            s += &ev(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": if calls { "tool_use" } else { "end_turn" }},
                       "usage": {"output_tokens": o}}),
            );
            s += &ev("message_stop", json!({"type": "message_stop"}));
        }
        P::Oai => {
            let data = |v: Value| format!("data: {v}\n\n");
            let mut k = 0;
            for st in steps {
                match st {
                    Step::Text(t) => {
                        s += &data(json!({"choices": [{"index": 0, "delta": {"content": t}}]}));
                    }
                    Step::Call(id, name, input) => {
                        s += &data(
                            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": k, "id": id,
                            "type": "function", "function": {"name": name, "arguments": input.to_string()}}]}}]}),
                        );
                        k += 1;
                    }
                }
            }
            s += &data(json!({"choices": [{"index": 0, "delta": {},
                "finish_reason": if calls { "tool_calls" } else { "stop" }}]}));
            if let Some((i, o)) = usage {
                s += &data(
                    json!({"choices": [], "usage": {"prompt_tokens": i, "completion_tokens": o}}),
                );
            }
            s += "data: [DONE]\n\n";
        }
    }
    Reply::Sse(s)
}

/// The SSE frames of a body: (id, event, data).
pub fn parse_sse(s: &str) -> Vec<(u64, String, Value)> {
    let mut out = Vec::new();
    for frame in s.split("\n\n") {
        let (mut id, mut name, mut data) = (0, None, Value::Null);
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("id: ") {
                id = v.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("event: ") {
                name = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("data: ") {
                data = serde_json::from_str(v).unwrap_or(Value::String(v.into()));
            }
        }
        if let Some(n) = name {
            out.push((id, n, data));
        }
    }
    out
}
