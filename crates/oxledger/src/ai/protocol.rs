// SPDX-License-Identifier: GPL-2.0-or-later
//! The two provider protocols (design 05 §3.13.2; docs/impl/m1-10-provider-notes.md):
//! Anthropic Messages (`POST {base}/v1/messages`) and OpenAI-compatible
//! chat completions (`POST {base}/chat/completions`), both streamed.
//!
//! Requests are built from one neutral form; streams are decoded into the
//! same [`ModelEvent`]s, so the run sees one protocol (T-M1-117).

use super::sse::SseEvent;
use serde_json::{Value as Json, json};

/// The `anthropic-version` header: the latest version on the versioning
/// page on 2026-11-11 (docs/impl/m1-10-provider-notes.md).
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Anthropic,
    OpenAiCompat,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "anthropic" => Some(Kind::Anthropic),
            "openai_compat" => Some(Kind::OpenAiCompat),
            _ => None,
        }
    }

    /// The path under the registered base.
    pub fn path(self, base_path: &str) -> String {
        match self {
            Kind::Anthropic => format!("{base_path}/v1/messages"),
            Kind::OpenAiCompat => format!("{base_path}/chat/completions"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: Json,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Json,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Msg>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

impl ModelRequest {
    /// Characters of everything sent (the 1 token per character estimate).
    pub fn chars(&self) -> u64 {
        let mut n = self.system.chars().count();
        for m in &self.messages {
            for b in &m.blocks {
                n += match b {
                    Block::Text(t) => t.chars().count(),
                    Block::ToolCall { name, input, .. } => {
                        name.chars().count() + input.to_string().chars().count()
                    }
                    Block::ToolResult { content, .. } => content.chars().count(),
                };
            }
        }
        for t in &self.tools {
            n += t.name.chars().count()
                + t.description.chars().count()
                + t.schema.to_string().chars().count();
        }
        n as u64
    }
}

/// The JSON body of a streamed request.
pub fn body(kind: Kind, r: &ModelRequest) -> Json {
    match kind {
        Kind::Anthropic => {
            let messages: Vec<Json> = r
                .messages
                .iter()
                .map(|m| {
                    let content: Vec<Json> = m
                        .blocks
                        .iter()
                        .map(|b| match b {
                            Block::Text(t) => json!({"type": "text", "text": t}),
                            Block::ToolCall { id, name, input } => {
                                json!({"type": "tool_use", "id": id, "name": name, "input": input})
                            }
                            Block::ToolResult {
                                id,
                                content,
                                is_error,
                            } => json!({"type": "tool_result", "tool_use_id": id,
                                         "content": content, "is_error": is_error}),
                        })
                        .collect();
                    json!({"role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                           "content": content})
                })
                .collect();
            let mut b = json!({
                "model": r.model,
                "max_tokens": r.max_tokens,
                "system": r.system,
                "messages": messages,
                "stream": true,
            });
            if !r.tools.is_empty() {
                b["tools"] = r
                    .tools
                    .iter()
                    .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
                    .collect();
            }
            b
        }
        Kind::OpenAiCompat => {
            let mut messages = vec![json!({"role": "system", "content": r.system})];
            for m in &r.messages {
                match m.role {
                    Role::Assistant => {
                        let text: String = m
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                Block::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        let calls: Vec<Json> = m
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                Block::ToolCall { id, name, input } => Some(json!({
                                    "id": id, "type": "function",
                                    "function": {"name": name, "arguments": input.to_string()}
                                })),
                                _ => None,
                            })
                            .collect();
                        let mut a = json!({"role": "assistant", "content": text});
                        if !calls.is_empty() {
                            a["tool_calls"] = Json::Array(calls);
                        }
                        messages.push(a);
                    }
                    Role::User => {
                        let mut text = Vec::new();
                        for b in &m.blocks {
                            match b {
                                Block::ToolResult { id, content, .. } => messages.push(
                                    json!({"role": "tool", "tool_call_id": id, "content": content}),
                                ),
                                Block::Text(t) => text.push(t.as_str()),
                                Block::ToolCall { .. } => {}
                            }
                        }
                        if !text.is_empty() {
                            messages.push(json!({"role": "user", "content": text.join("\n\n")}));
                        }
                    }
                }
            }
            // `max_tokens`: self-hosted servers (vLLM, Ollama) take it;
            // OpenAI deprecated it for `max_completion_tokens` (provider notes).
            let mut b = json!({
                "model": r.model,
                "messages": messages,
                "max_tokens": r.max_tokens,
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if !r.tools.is_empty() {
                b["tools"] = r
                    .tools
                    .iter()
                    .map(|t| {
                        json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.schema}})
                    })
                    .collect();
            }
            b
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// refusal, content_filter, pause_turn, …: the turn is over.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: Json,
    },
    /// Cumulative counts as the provider reported them.
    Usage {
        input: Option<u64>,
        output: Option<u64>,
    },
    Stop(Stop),
}

/// The provider sent something we cannot use; or an error event inside the
/// stream (`status` is its HTTP-like class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Malformed,
    Provider { status: u16 },
}

pub trait Decoder: Send {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<ModelEvent>, DecodeError>;
    /// The byte stream ended; an unfinished stream is malformed.
    fn finish(&mut self) -> Result<Vec<ModelEvent>, DecodeError>;
}

pub fn decoder(kind: Kind) -> Box<dyn Decoder> {
    match kind {
        Kind::Anthropic => Box::new(AnthropicDecoder::default()),
        Kind::OpenAiCompat => Box::new(OpenAiDecoder::default()),
    }
}

fn parse(data: &str) -> Result<Json, DecodeError> {
    serde_json::from_str(data).map_err(|_| DecodeError::Malformed)
}

fn u(v: &Json) -> Option<u64> {
    v.as_u64()
}

#[derive(Default)]
struct AnthropicDecoder {
    /// index → (id, name, partial JSON) of an open tool_use block.
    tools: std::collections::BTreeMap<u64, (String, String, String)>,
    input: u64,
    done: bool,
}

fn anthropic_status(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "timeout_error" => 504,
        "overloaded_error" => 529,
        _ => 500,
    }
}

impl Decoder for AnthropicDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<ModelEvent>, DecodeError> {
        let v = parse(&ev.data)?;
        let ty = ev
            .event
            .as_deref()
            .or_else(|| v["type"].as_str())
            .unwrap_or_default()
            .to_string();
        let mut out = Vec::new();
        match ty.as_str() {
            "message_start" => {
                let us = &v["message"]["usage"];
                self.input = u(&us["input_tokens"]).unwrap_or(0)
                    + u(&us["cache_creation_input_tokens"]).unwrap_or(0)
                    + u(&us["cache_read_input_tokens"]).unwrap_or(0);
                out.push(ModelEvent::Usage {
                    input: Some(self.input),
                    output: u(&us["output_tokens"]),
                });
            }
            "content_block_start" => {
                let i = u(&v["index"]).ok_or(DecodeError::Malformed)?;
                let b = &v["content_block"];
                match b["type"].as_str() {
                    Some("tool_use") => {
                        let id = b["id"].as_str().ok_or(DecodeError::Malformed)?;
                        let name = b["name"].as_str().ok_or(DecodeError::Malformed)?;
                        self.tools
                            .insert(i, (id.to_string(), name.to_string(), String::new()));
                    }
                    Some("text") => {
                        if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(ModelEvent::Text(t.to_string()));
                        }
                    }
                    _ => {} // thinking, server tools: not used
                }
            }
            "content_block_delta" => {
                let i = u(&v["index"]).ok_or(DecodeError::Malformed)?;
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().ok_or(DecodeError::Malformed)?;
                        out.push(ModelEvent::Text(t.to_string()));
                    }
                    Some("input_json_delta") => {
                        let p = d["partial_json"].as_str().ok_or(DecodeError::Malformed)?;
                        match self.tools.get_mut(&i) {
                            Some(t) => t.2.push_str(p),
                            None => return Err(DecodeError::Malformed),
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = u(&v["index"]).ok_or(DecodeError::Malformed)?;
                if let Some((id, name, js)) = self.tools.remove(&i) {
                    let input = if js.trim().is_empty() {
                        json!({})
                    } else {
                        parse(&js)?
                    };
                    out.push(ModelEvent::ToolCall { id, name, input });
                }
            }
            "message_delta" => {
                if let Some(o) = u(&v["usage"]["output_tokens"]) {
                    out.push(ModelEvent::Usage {
                        input: Some(self.input),
                        output: Some(o),
                    });
                }
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    out.push(ModelEvent::Stop(match s {
                        "end_turn" | "stop_sequence" => Stop::EndTurn,
                        "tool_use" => Stop::ToolUse,
                        "max_tokens" | "model_context_window_exceeded" => Stop::MaxTokens,
                        _ => Stop::Other,
                    }));
                }
            }
            "message_stop" => self.done = true,
            "error" => {
                let k = v["error"]["type"].as_str().unwrap_or_default();
                return Err(DecodeError::Provider {
                    status: anthropic_status(k),
                });
            }
            _ => {} // ping and anything new
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<ModelEvent>, DecodeError> {
        if self.done && self.tools.is_empty() {
            Ok(Vec::new())
        } else {
            Err(DecodeError::Malformed)
        }
    }
}

#[derive(Default)]
struct OpenAiDecoder {
    /// index → (id, name, arguments).
    calls: std::collections::BTreeMap<u64, (String, String, String)>,
    stop: Option<Stop>,
    done: bool,
}

impl OpenAiDecoder {
    fn flush_calls(&mut self) -> Result<Vec<ModelEvent>, DecodeError> {
        let mut out = Vec::new();
        for (_, (id, name, args)) in std::mem::take(&mut self.calls) {
            if name.is_empty() {
                return Err(DecodeError::Malformed);
            }
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                parse(&args)?
            };
            out.push(ModelEvent::ToolCall { id, name, input });
        }
        Ok(out)
    }
}

impl Decoder for OpenAiDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<ModelEvent>, DecodeError> {
        if ev.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let v = parse(&ev.data)?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            let status = v["error"]["code"]
                .as_u64()
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(500);
            return Err(DecodeError::Provider { status });
        }
        let mut out = Vec::new();
        if let Some(c) = v["choices"].as_array().and_then(|a| a.first()) {
            let d = &c["delta"];
            if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                out.push(ModelEvent::Text(t.to_string()));
            }
            if let Some(calls) = d["tool_calls"].as_array() {
                for (n, tc) in calls.iter().enumerate() {
                    let i = u(&tc["index"]).unwrap_or(n as u64);
                    let e = self.calls.entry(i).or_default();
                    if let Some(id) = tc["id"].as_str() {
                        e.0 = id.to_string();
                    }
                    if let Some(name) = tc["function"]["name"].as_str() {
                        e.1.push_str(name);
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        e.2.push_str(a);
                    }
                }
            }
            if let Some(f) = c["finish_reason"].as_str() {
                let s = match f {
                    "stop" => Stop::EndTurn,
                    "tool_calls" | "function_call" => Stop::ToolUse,
                    "length" => Stop::MaxTokens,
                    _ => Stop::Other,
                };
                self.stop = Some(s);
                out.extend(self.flush_calls()?);
                out.push(ModelEvent::Stop(s));
            }
        }
        if let Some(us) = v.get("usage").filter(|x| !x.is_null()) {
            out.push(ModelEvent::Usage {
                input: u(&us["prompt_tokens"]),
                output: u(&us["completion_tokens"]),
            });
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<ModelEvent>, DecodeError> {
        if !self.done || self.stop.is_none() {
            return Err(DecodeError::Malformed);
        }
        self.flush_calls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sse::SseParser;

    fn run(kind: Kind, raw: &str) -> Result<Vec<ModelEvent>, DecodeError> {
        let mut p = SseParser::new();
        let mut d = decoder(kind);
        let mut out = Vec::new();
        for e in p.push(raw.as_bytes()).unwrap() {
            out.extend(d.on_event(&e)?);
        }
        if let Some(e) = p.finish().unwrap() {
            out.extend(d.on_event(&e)?);
        }
        out.extend(d.finish()?);
        Ok(out)
    }

    const ANTHROPIC: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: ping\n\
data: {\"type\":\"ping\"}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"key\\\": \\\"AU\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"TH-1\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    #[test]
    fn anthropic_text_and_tool() {
        let v = run(Kind::Anthropic, ANTHROPIC).unwrap();
        assert!(v.contains(&ModelEvent::Text("Hi".into())));
        assert!(v.contains(&ModelEvent::ToolCall {
            id: "toolu_1".into(),
            name: "get".into(),
            input: json!({"key": "AUTH-1"})
        }));
        assert!(v.contains(&ModelEvent::Usage {
            input: Some(12),
            output: Some(30)
        }));
        assert_eq!(v.last(), Some(&ModelEvent::Stop(Stop::ToolUse)));
    }

    #[test]
    fn anthropic_cut_short_is_malformed() {
        let cut = &ANTHROPIC[..ANTHROPIC.find("event: message_delta").unwrap()];
        assert_eq!(run(Kind::Anthropic, cut), Err(DecodeError::Malformed));
    }

    #[test]
    fn anthropic_error_event() {
        let e = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"secret detail\"}}\n\n";
        assert_eq!(
            run(Kind::Anthropic, e),
            Err(DecodeError::Provider { status: 529 })
        );
    }

    const OPENAI: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get\",\"arguments\":\"{\\\"key\\\":\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"AUTH-1\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16}}\n\n\
data: [DONE]\n\n";

    #[test]
    fn openai_text_and_tool() {
        let v = run(Kind::OpenAiCompat, OPENAI).unwrap();
        assert_eq!(
            v,
            vec![
                ModelEvent::Text("Hi".into()),
                ModelEvent::ToolCall {
                    id: "call_1".into(),
                    name: "get".into(),
                    input: json!({"key": "AUTH-1"})
                },
                ModelEvent::Stop(Stop::ToolUse),
                ModelEvent::Usage {
                    input: Some(11),
                    output: Some(5)
                },
            ]
        );
    }

    #[test]
    fn openai_without_done_is_malformed() {
        let cut = &OPENAI[..OPENAI
            .find("data: {\"choices\":[{\"index\":0,\"delta\":{},")
            .unwrap()];
        assert_eq!(run(Kind::OpenAiCompat, cut), Err(DecodeError::Malformed));
    }

    #[test]
    fn bodies() {
        let r = ModelRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![
                Msg {
                    role: Role::User,
                    blocks: vec![Block::Text("hello".into())],
                },
                Msg {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::Text("ok".into()),
                        Block::ToolCall {
                            id: "t1".into(),
                            name: "get".into(),
                            input: json!({"key": "A-1"}),
                        },
                    ],
                },
                Msg {
                    role: Role::User,
                    blocks: vec![Block::ToolResult {
                        id: "t1".into(),
                        content: "{}".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "get".into(),
                description: "d".into(),
                schema: json!({"type": "object"}),
            }],
            max_tokens: 100,
        };
        let a = body(Kind::Anthropic, &r);
        assert_eq!(a["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(a["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(a["tools"][0]["input_schema"]["type"], "object");
        let o = body(Kind::OpenAiCompat, &r);
        assert_eq!(o["messages"][0]["role"], "system");
        assert_eq!(
            o["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"key\":\"A-1\"}"
        );
        assert_eq!(o["messages"][3]["role"], "tool");
        assert_eq!(o["tools"][0]["function"]["parameters"]["type"], "object");
        let mut r2 = r.clone();
        r2.tools.clear();
        assert!(body(Kind::Anthropic, &r2).get("tools").is_none());
        assert!(body(Kind::OpenAiCompat, &r2).get("tools").is_none());
    }
}
