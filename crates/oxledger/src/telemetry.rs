// SPDX-License-Identifier: GPL-2.0-or-later
//! Request ids, the per-request span, and the log format (T2; research 06
//! I-1, I-2; review 08 D9).
//!
//! - Every HTTP request gets a fresh UUIDv7 request id. It is the id in the
//!   `X-Request-Id` response header, in `meta.request_id` / `error.request_id`,
//!   and on every log line written while the request runs (the `http` span).
//! - The only client value taken is `traceparent` (W3C Trace Context): when
//!   it is well formed its trace-id goes into the span as `trace_id`. Any
//!   other id header (`X-Request-Id`, `X-Correlation-Id`, …) is ignored, so
//!   a client cannot write text of its choice into the logs.
//! - The span records the route template (`/api/v1/issues/{key}`), never the
//!   path or the query string, which can carry search text or keys.
//! - `OXLEDGER_LOG_FORMAT=json` writes one JSON object per line; unset or
//!   `text` keeps the plain text format.

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Map, Value};
use std::fmt;
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Instrument, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

/// The response header that carries the request id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// The only request header a client id is taken from.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// The route label of every request no route matched (404s of unknown
/// paths): one label, whatever the path was.
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// This request's id, in the request extensions (MCP reads it there: tool
/// calls run in tasks of their own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub Uuid);

tokio::task_local! {
    static CURRENT: Uuid;
}

/// The id of the request this task is serving; a fresh one outside a
/// request (stdio, background jobs), as before T2.
pub fn request_id() -> Uuid {
    CURRENT
        .try_with(|id| *id)
        .unwrap_or_else(|_| Uuid::now_v7())
}

/// Runs `f` with `id` as [`request_id`].
pub async fn with_request_id<F: std::future::Future>(id: Uuid, f: F) -> F::Output {
    CURRENT.scope(id, f).await
}

/// The trace-id of a `traceparent` header (W3C Trace Context level 1,
/// version `00`): `00-<32 hex>-<16 hex>-<2 hex>`, lower case, neither id all
/// zeros. Anything else is `None` and the header is ignored.
pub fn parse_traceparent(v: &str) -> Option<&str> {
    let b = v.as_bytes();
    if b.len() != 55 || b[2] != b'-' || b[35] != b'-' || b[52] != b'-' {
        return None;
    }
    let hex = |s: &[u8]| {
        s.iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    };
    let (version, trace, parent, flags) = (&b[0..2], &b[3..35], &b[36..52], &b[53..55]);
    if version != b"00" || !hex(trace) || !hex(parent) || !hex(flags) {
        return None;
    }
    if trace.iter().all(|c| *c == b'0') || parent.iter().all(|c| *c == b'0') {
        return None;
    }
    Some(&v[3..35])
}

/// The method label: the standard methods by name, everything else `OTHER`
/// (a client can send any token as the method).
pub fn method_label(m: &Method) -> &'static str {
    match *m {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

/// The route template axum matched, or [`UNMATCHED_ROUTE`].
pub fn route_label(req: &Request) -> MatchedOrNot {
    match req.extensions().get::<MatchedPath>() {
        Some(p) => MatchedOrNot(Some(p.clone())),
        None => MatchedOrNot(None),
    }
}

/// [`route_label`]'s result (keeps axum's shared string, no copy).
#[derive(Clone)]
pub struct MatchedOrNot(Option<MatchedPath>);

impl MatchedOrNot {
    pub fn as_str(&self) -> &str {
        self.0.as_ref().map_or(UNMATCHED_ROUTE, |p| p.as_str())
    }
}

/// The request middleware (`api::observed` puts it on every route): id,
/// span, `X-Request-Id`, and the HTTP metrics. No database access.
pub async fn observe(mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let id = Uuid::now_v7();
    let method = method_label(req.method());
    let route = route_label(&req);
    let span = tracing::info_span!(
        "http",
        request_id = %id,
        method,
        route = route.as_str(),
        trace_id = tracing::field::Empty,
        status = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    );
    if let Some(t) = req
        .headers()
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_traceparent)
    {
        span.record("trace_id", t);
    }
    req.extensions_mut().insert(RequestId(id));
    let mut resp = with_request_id(id, next.run(req).instrument(span.clone())).await;
    let took = started.elapsed();
    let status = resp.status().as_u16();
    span.record("status", status);
    span.record("latency_ms", took.as_millis() as u64);
    tracing::debug!(parent: &span, "request finished");
    if let Ok(v) = HeaderValue::from_str(&id.to_string()) {
        resp.headers_mut().insert(REQUEST_ID_HEADER, v);
    }
    crate::metrics::HTTP.observe(route.as_str(), method, status, took);
    resp
}

// ---------------------------------------------------------------- log format

/// `OXLEDGER_LOG_FORMAT`: `text` (default) or `json`.
pub const LOG_FORMAT_ENV: &str = "OXLEDGER_LOG_FORMAT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Parses [`LOG_FORMAT_ENV`]: unset or empty is [`LogFormat::Text`] (the
/// format before T2); anything but `text` / `json` is an error, so a typo
/// stops the process instead of being ignored.
pub fn parse_log_format(v: Option<&str>) -> anyhow::Result<LogFormat> {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("text") => Ok(LogFormat::Text),
        Some("json") => Ok(LogFormat::Json),
        Some(other) => anyhow::bail!("{LOG_FORMAT_ENV} must be text or json, got {other:?}"),
    }
}

/// The log filter of `main`: `RUST_LOG` when it parses, `info` otherwise
/// (the behaviour before T2).
pub fn env_filter(rust_log: Option<&str>) -> EnvFilter {
    rust_log
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new("info"))
}

/// `OXLEDGER_ALLOW_RMCP_DEBUG=1` lets `RUST_LOG` turn on rmcp's debug and
/// trace logs. They contain whole MCP requests and tool results (issue
/// content), so without it rmcp is capped at `info` whatever `RUST_LOG` says.
pub const ALLOW_RMCP_DEBUG_ENV: &str = "OXLEDGER_ALLOW_RMCP_DEBUG";

/// Parses [`ALLOW_RMCP_DEBUG_ENV`]: only exactly `1` allows.
pub fn allow_rmcp_debug(v: Option<&str>) -> bool {
    v.map(str::trim) == Some("1")
}

/// The cap on top of `RUST_LOG`: rmcp at most `info` unless
/// `allow_rmcp_debug`, everything else untouched. A second filter can
/// only drop more, so no `RUST_LOG` value can lift it.
pub fn rmcp_cap(allow_rmcp_debug: bool) -> tracing_subscriber::filter::Targets {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    let t = Targets::new().with_default(LevelFilter::TRACE);
    if allow_rmcp_debug {
        t
    } else {
        t.with_target("rmcp", LevelFilter::INFO)
    }
}

/// The subscriber of `main` (and of the log tests, with their own writer
/// and filter), with [`rmcp_cap`] on top of `filter`.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    allow_rmcp_debug: bool,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt;
    let cap = rmcp_cap(allow_rmcp_debug);
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match format {
        LogFormat::Text => Box::new(b.finish().with(cap)),
        LogFormat::Json => Box::new(
            b.with_ansi(false)
                .fmt_fields(JsonFields)
                .event_format(JsonFormat)
                .finish()
                .with(cap),
        ),
    }
}

/// JSON text safe to paste into JavaScript and to split on any Unicode line
/// break: serde_json leaves U+2028, U+2029 and U+0085 as raw characters.
fn escape_line_breaks(s: String) -> String {
    if !s.contains(['\u{2028}', '\u{2029}', '\u{0085}']) {
        return s;
    }
    s.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{0085}', "\\u0085")
}

/// Collects fields into a JSON object.
struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl Visit for JsonVisitor<'_> {
    fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
        self.0
            .insert(f.name().to_string(), Value::String(format!("{v:?}")));
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.0
            .insert(f.name().to_string(), Value::String(v.to_string()));
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.0.insert(f.name().to_string(), Value::from(v));
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.0.insert(f.name().to_string(), Value::from(v));
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.0.insert(f.name().to_string(), Value::from(v));
    }
    fn record_f64(&mut self, f: &Field, v: f64) {
        let n = serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(v.to_string()));
        self.0.insert(f.name().to_string(), n);
    }
    fn record_error(&mut self, f: &Field, v: &(dyn std::error::Error + 'static)) {
        self.0
            .insert(f.name().to_string(), Value::String(v.to_string()));
    }
}

/// Span fields as a JSON object (stored per span by the fmt layer).
pub struct JsonFields;

impl<'w> FormatFields<'w> for JsonFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut m = Map::new();
        fields.record(&mut JsonVisitor(&mut m));
        write!(writer, "{}", Value::Object(m))
    }

    fn add_fields(
        &self,
        current: &'w mut FormattedFields<Self>,
        fields: &tracing::span::Record<'_>,
    ) -> fmt::Result {
        let mut m = match serde_json::from_str::<Value>(&current.fields) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        fields.record(&mut JsonVisitor(&mut m));
        current.fields = Value::Object(m).to_string();
        Ok(())
    }
}

/// One JSON object per event: `timestamp`, `level`, `target`, `fields`
/// (with `message`), and `spans` from the root (each with its `name` and
/// fields, e.g. the `http` span's `request_id` and `route`).
pub struct JsonFormat;

impl<S> FormatEvent<S, JsonFields> for JsonFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut fields = Map::new();
        event.record(&mut JsonVisitor(&mut fields));
        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let mut obj = Map::new();
                obj.insert("name".into(), Value::from(span.name()));
                if let Some(f) = span.extensions().get::<FormattedFields<JsonFields>>()
                    && let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&f.fields)
                {
                    obj.extend(m);
                }
                spans.push(Value::Object(obj));
            }
        }
        let mut out = Map::new();
        out.insert(
            "timestamp".into(),
            Value::from(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
        );
        out.insert("level".into(), Value::from(meta.level().as_str()));
        out.insert("target".into(), Value::from(meta.target()));
        out.insert("fields".into(), Value::Object(fields));
        if !spans.is_empty() {
            out.insert("spans".into(), Value::Array(spans));
        }
        writeln!(
            writer,
            "{}",
            escape_line_breaks(Value::Object(out).to_string())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparent_accepts_the_w3c_example() {
        assert_eq!(
            parse_traceparent(GOOD),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[test]
    fn traceparent_rejects_malformed_values() {
        for bad in [
            "",
            "garbage",
            // upper case hex
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            // all-zero trace id / parent id
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // other versions, wrong separators, extra text, short
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\nINJECTED",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            // non-hex inside
            "00-4bf92f3577b34da6a3ce929d0e0e47zz-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0g",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn method_label_is_bounded() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(method_label(&Method::DELETE), "DELETE");
        let odd = Method::from_bytes(b"HIDDENMETHOD").unwrap();
        assert_eq!(method_label(&odd), "OTHER");
    }

    #[test]
    fn log_format_parses() {
        assert_eq!(parse_log_format(None).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some("")).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some("text")).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some(" JSON ")).unwrap(), LogFormat::Json);
        assert!(parse_log_format(Some("yaml")).is_err());
    }

    #[test]
    fn json_lines_escape_unicode_line_breaks() {
        let raw = Value::from("a\u{2028}b\u{2029}c\u{0085}d").to_string();
        let out = escape_line_breaks(raw);
        // Built from pieces: the escapes as text, not as characters.
        let want = ["\"a", "u2028b", "u2029c", "u0085d\""].join("\\");
        assert_eq!(out, want);
        assert_eq!(
            serde_json::from_str::<String>(&out).unwrap(),
            "a\u{2028}b\u{2029}c\u{0085}d"
        );
    }

    #[test]
    fn rmcp_debug_is_allowed_only_by_exactly_one() {
        assert!(allow_rmcp_debug(Some("1")));
        for v in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert!(!allow_rmcp_debug(v), "{v:?}");
        }
    }

    #[test]
    fn env_filter_falls_back_to_info() {
        assert_eq!(env_filter(None).to_string(), "info");
        assert_eq!(env_filter(Some("[[[")).to_string(), "info");
        assert_eq!(env_filter(Some("debug")).to_string(), "debug");
    }

    #[test]
    fn request_id_outside_a_request_is_fresh() {
        assert_ne!(request_id(), request_id());
    }

    #[tokio::test]
    async fn request_id_inside_a_scope_is_the_scope_id() {
        let id = Uuid::now_v7();
        let (a, b) = with_request_id(id, async { (request_id(), request_id()) }).await;
        assert_eq!((a, b), (id, id));
    }
}
