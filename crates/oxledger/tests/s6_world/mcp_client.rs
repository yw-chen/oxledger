// SPDX-License-Identifier: GPL-2.0-or-later
//! A minimal MCP client over the router (Streamable HTTP; responses as JSON
//! or server-sent events).
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A minimal MCP client over the router: initialize, then calls.
pub struct Mcp {
    router: Router,
    token: String,
    session: Option<String>,
    next_id: u64,
    /// Tool calls made (`tools/call`), to count round trips (§14).
    pub calls: u32,
}

impl Mcp {
    pub async fn connect(router: Router, token: String) -> Mcp {
        let mut m = Mcp {
            router,
            token,
            session: None,
            next_id: 1,
            calls: 0,
        };
        let r = m
            .rpc(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "s6-test", "version": "0"}}),
            )
            .await;
        assert!(r["result"]["serverInfo"].is_object(), "{r}");
        m.notify("notifications/initialized").await;
        m
    }

    async fn post(&mut self, body: Value) -> (Option<String>, Vec<u8>) {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(s) = &self.session {
            b = b.header("mcp-session-id", s);
        }
        let resp = self
            .router
            .clone()
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (sid, bytes.to_vec())
    }

    async fn notify(&mut self, method: &str) {
        self.post(json!({"jsonrpc": "2.0", "method": method})).await;
    }

    pub async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let (sid, bytes) = self
            .post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        if self.session.is_none() {
            self.session = sid;
        }
        let text = String::from_utf8(bytes).unwrap();
        // JSON, or server-sent events with the answer in a data line.
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            return v;
        }
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v["id"] == json!(id))
            .unwrap_or_else(|| panic!("no answer in {text}"))
    }

    /// A tool call: `Ok(structured content)` or `Err(error object)`.
    pub async fn call(&mut self, name: &str, args: Value) -> Result<Value, Value> {
        self.calls += 1;
        let r = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await;
        let res = &r["result"];
        if res.is_null() {
            return Err(r["error"].clone());
        }
        let content = res["structuredContent"].clone();
        if res["isError"] == json!(true) {
            Err(content)
        } else {
            Ok(content)
        }
    }
}
