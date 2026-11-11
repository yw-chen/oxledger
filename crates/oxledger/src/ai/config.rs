// SPDX-License-Identifier: GPL-2.0-or-later
//! Deployment registration of AI endpoints (design 05 §2.12, §3.13.2 P-1,
//! P-2, P-3, P-7; Q1).
//!
//! `OXLEDGER_AI_ENDPOINT_<NAME>=https://host[:port][/path]` registers an
//! endpoint; `OXLEDGER_AI_KEY_<NAME>` is its key (optional);
//! `OXLEDGER_AI_CA_FILE_<NAME>` a PEM file of extra trusted CA certificates;
//! `OXLEDGER_AI_ALLOW_HTTP=1` lets `http://` reach loopback and private
//! addresses (B-40: the switch only governs http). The web app picks a
//! registered name and a model; it can never give a URL or a variable name.
//!
//! Keys live in memory only: [`ApiKey`] prints as `<redacted>`, and no
//! warning, error or response carries a key or an endpoint URL.

use super::net::{Resolver, SystemResolver};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use url::Url;

/// D17 (§3.13.2 P-1): model names containing these are refused. The list is
/// not complete; the administrator's attestation is what counts.
pub const D17_MODEL_PATTERNS: [&str; 15] = [
    "qwen", "deepseek", "glm", "chatglm", "yi-", "baichuan", "internlm", "minimax", "moonshot",
    "kimi", "doubao", "ernie", "hunyuan", "step-", "abab",
];

/// D17: API hosts of known Chinese vendors (a host equal to one of these or
/// under it is not registered).
pub const D17_HOSTS: [&str; 20] = [
    "dashscope.aliyuncs.com",
    "aliyuncs.com",
    "deepseek.com",
    "bigmodel.cn",
    "zhipuai.cn",
    "moonshot.cn",
    "moonshot.ai",
    "baichuan-ai.com",
    "minimax.chat",
    "minimaxi.com",
    "volces.com",
    "volcengine.com",
    "baidubce.com",
    "tencentcloudapi.com",
    "stepfun.com",
    "lingyiwanwu.com",
    "01.ai",
    "sensenova.cn",
    "xf-yun.com",
    "siliconflow.cn",
];

/// An API key read from the environment. Never printed, serialized or stored.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// The key itself, for the request header only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A registered endpoint (P-3 (1)).
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub name: String,
    pub https: bool,
    /// Host name or IP literal (no brackets).
    pub host: String,
    pub port: u16,
    /// Path prefix without a trailing slash ("" or "/v1").
    pub base_path: String,
    pub key: Option<ApiKey>,
    /// Extra trusted CA certificates (DER), from `OXLEDGER_AI_CA_FILE_<NAME>`.
    pub ca_der: Vec<Vec<u8>>,
}

impl Endpoint {
    /// `scheme://host[:port]` (no path): what the settings page shows.
    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{}", self.port)
        }
    }

    /// The `Host` header value.
    pub fn host_header(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }
}

/// Limits of a run (§3.13.4; 01 §16.3 adjusted by B-34). Tests shorten them.
#[derive(Clone, Debug)]
pub struct Limits {
    pub first_byte: Duration,
    pub chunk_gap: Duration,
    pub request: Duration,
    pub run: Duration,
    pub max_tool_calls: u32,
    /// Estimated input tokens (1 per character) of one model request.
    pub request_input_tokens: u64,
    /// Estimated input tokens of all requests of a run.
    pub run_input_tokens: u64,
    /// A tool result sent to the model is cut to this many characters.
    pub tool_result_chars: usize,
    pub max_response_bytes: usize,
    pub lease: Duration,
    pub renew_every: Duration,
    pub sweep_every: Duration,
    pub ping_every: Duration,
    /// Events of a finished run are kept this long for reconnects.
    pub keep_events: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            first_byte: Duration::from_secs(30),
            chunk_gap: Duration::from_secs(30),
            request: Duration::from_secs(120),
            run: Duration::from_secs(180),
            max_tool_calls: 12,
            request_input_tokens: 16_000,
            run_input_tokens: 50_000,
            tool_result_chars: 8_000,
            max_response_bytes: 8 * 1024 * 1024,
            // A process that dies right after renewing leaves a lease of
            // `lease`; the sweeper notices within `sweep_every` more, so a
            // dead run ends within lease + sweep_every = 45 s ≤ 60 s
            // (§3.13.4). Renewing every 10 s survives two missed renewals.
            lease: Duration::from_secs(30),
            renew_every: Duration::from_secs(10),
            sweep_every: Duration::from_secs(15),
            ping_every: Duration::from_secs(15),
            keep_events: Duration::from_secs(300),
        }
    }
}

/// What the process registered at start-up.
#[derive(Clone)]
pub struct AiConfig {
    pub endpoints: BTreeMap<String, Endpoint>,
    pub allow_http: bool,
    pub resolver: Arc<dyn Resolver>,
    pub limits: Limits,
    pub blocked_hosts: Vec<String>,
    pub blocked_models: Vec<String>,
}

impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("endpoints", &self.endpoints.keys().collect::<Vec<_>>())
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            endpoints: BTreeMap::new(),
            allow_http: false,
            resolver: Arc::new(SystemResolver),
            limits: Limits::default(),
            blocked_hosts: D17_HOSTS.iter().map(|s| s.to_string()).collect(),
            blocked_models: D17_MODEL_PATTERNS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl AiConfig {
    /// The model name hits the D17 list (case-insensitive substring).
    pub fn model_disallowed(&self, model: &str) -> bool {
        let m = model.to_lowercase();
        self.blocked_models.iter().any(|p| m.contains(p.as_str()))
    }

    /// The host is a known Chinese vendor's API host or under one.
    pub fn host_disallowed(&self, host: &str) -> bool {
        let h = host.trim_end_matches('.').to_lowercase();
        self.blocked_hosts
            .iter()
            .any(|b| h == *b || h.ends_with(&format!(".{b}")))
    }
}

/// `^[A-Z][A-Z0-9_]{0,39}$`.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Parses a registered URL (P-3 (1)). The error says why, never the URL.
pub fn parse_endpoint_url(raw: &str) -> Result<(bool, String, u16, String), &'static str> {
    let u = Url::parse(raw.trim()).map_err(|_| "not a URL")?;
    let https = match u.scheme() {
        "https" => true,
        "http" => false,
        _ => return Err("only https:// (or http:// with OXLEDGER_AI_ALLOW_HTTP) is accepted"),
    };
    if !u.username().is_empty() || u.password().is_some() {
        return Err("the URL must not contain user information");
    }
    if u.query().is_some() {
        return Err("the URL must not contain a query");
    }
    if u.fragment().is_some() {
        return Err("the URL must not contain a fragment");
    }
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(a)) => a.to_string(),
        Some(url::Host::Ipv6(a)) => a.to_string(),
        None => return Err("the URL has no host"),
    };
    let port = u
        .port_or_known_default()
        .ok_or("the URL has no usable port")?;
    let path = u.path().trim_end_matches('/').to_string();
    Ok((https, host, port, path))
}

/// Reads PEM certificates (P-7: `rustls-pki-types`, already a dependency
/// of `rustls`; no new crate).
#[cfg(feature = "ai")]
fn pem_certs(bytes: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut out = Vec::new();
    for c in CertificateDer::pem_slice_iter(bytes) {
        out.push(c.map_err(|_| "the CA file is not valid PEM")?.to_vec());
    }
    if out.is_empty() {
        return Err("the CA file holds no certificate");
    }
    Ok(out)
}

#[cfg(not(feature = "ai"))]
fn pem_certs(_: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    Err("this build has no AI client (feature `ai`)")
}

/// Builds the registration from environment variables. Returns warnings for
/// names that were not registered (name and reason only).
pub fn from_vars<I: IntoIterator<Item = (String, String)>>(vars: I) -> (AiConfig, Vec<String>) {
    let vars: BTreeMap<String, String> = vars
        .into_iter()
        .filter(|(k, _)| k.starts_with("OXLEDGER_AI_"))
        .collect();
    let mut cfg = AiConfig {
        allow_http: matches!(
            vars.get("OXLEDGER_AI_ALLOW_HTTP").map(|s| s.trim()),
            Some("1" | "true" | "yes")
        ),
        ..AiConfig::default()
    };
    for (var, extra) in [
        ("OXLEDGER_AI_D17_EXTRA_HOSTS", &mut cfg.blocked_hosts),
        ("OXLEDGER_AI_D17_EXTRA_MODELS", &mut cfg.blocked_models),
    ] {
        if let Some(v) = vars.get(var) {
            extra.extend(
                v.split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    let mut warnings = Vec::new();
    for (k, raw) in &vars {
        let Some(name) = k.strip_prefix("OXLEDGER_AI_ENDPOINT_") else {
            continue;
        };
        if !valid_name(name) {
            warnings.push(format!(
                "AI endpoint {name:?} not registered: the name must match ^[A-Z][A-Z0-9_]{{0,39}}$"
            ));
            continue;
        }
        let (https, host, port, base_path) = match parse_endpoint_url(raw) {
            Ok(p) => p,
            Err(why) => {
                warnings.push(format!("AI endpoint {name} not registered: {why}"));
                continue;
            }
        };
        if cfg.host_disallowed(&host) {
            warnings.push(format!(
                "AI endpoint {name} not registered: the host is on the D17 list (Chinese vendors)"
            ));
            continue;
        }
        let ca_der = match vars.get(&format!("OXLEDGER_AI_CA_FILE_{name}")) {
            None => Vec::new(),
            Some(path) => match std::fs::read(path.trim()) {
                Err(_) => {
                    warnings.push(format!(
                        "AI endpoint {name} not registered: its CA file cannot be read"
                    ));
                    continue;
                }
                Ok(bytes) => match pem_certs(&bytes) {
                    Ok(c) => c,
                    Err(why) => {
                        warnings.push(format!("AI endpoint {name} not registered: {why}"));
                        continue;
                    }
                },
            },
        };
        let key = vars
            .get(&format!("OXLEDGER_AI_KEY_{name}"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(ApiKey);
        cfg.endpoints.insert(
            name.to_string(),
            Endpoint {
                name: name.to_string(),
                https,
                host,
                port,
                base_path,
                key,
                ca_der,
            },
        );
    }
    (cfg, warnings)
}

static CURRENT: RwLock<Option<Arc<AiConfig>>> = RwLock::new(None);

/// Installs the registration for this process (the server at start-up;
/// tests install their own).
pub fn install(cfg: AiConfig) {
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(Arc::new(cfg));
    }
}

/// The installed registration (empty when none was installed).
pub fn current() -> Arc<AiConfig> {
    CURRENT
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| Arc::new(AiConfig::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// A dead run ends within 60 s (§3.13.4, independent verification D6):
    /// worst case is a death right after a renewal, the lease running out
    /// and the sweeper coming round just after it ran.
    #[test]
    fn a_dead_run_ends_within_a_minute() {
        let l = Limits::default();
        assert!(l.lease + l.sweep_every <= Duration::from_secs(60));
        // Two missed renewals still keep a live run's lease.
        assert!(l.renew_every * 3 <= l.lease);
    }

    #[test]
    fn registers_valid_urls_only() {
        let (c, w) = from_vars(vars(&[
            (
                "OXLEDGER_AI_ENDPOINT_ANTHROPIC",
                "https://api.anthropic.com",
            ),
            ("OXLEDGER_AI_KEY_ANTHROPIC", "sk-secret-123"),
            ("OXLEDGER_AI_ENDPOINT_VLLM", "https://llm.intra.example/v1/"),
            ("OXLEDGER_AI_ENDPOINT_USERINFO", "https://u:p@llm.example"),
            ("OXLEDGER_AI_ENDPOINT_QUERY", "https://llm.example/?a=1"),
            ("OXLEDGER_AI_ENDPOINT_FRAG", "https://llm.example/#x"),
            ("OXLEDGER_AI_ENDPOINT_FTP", "ftp://llm.example"),
            ("OXLEDGER_AI_ENDPOINT_bad", "https://llm.example"),
            (
                "OXLEDGER_AI_ENDPOINT_CN",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
            ),
            ("OXLEDGER_AI_ENDPOINT_OLLAMA", "http://10.0.0.5:11434/v1"),
        ]));
        assert_eq!(
            c.endpoints.keys().cloned().collect::<Vec<_>>(),
            vec!["ANTHROPIC", "OLLAMA", "VLLM"]
        );
        assert_eq!(c.endpoints["VLLM"].base_path, "/v1");
        assert_eq!(c.endpoints["OLLAMA"].port, 11434);
        assert!(!c.endpoints["OLLAMA"].https);
        assert!(c.endpoints["ANTHROPIC"].has_key());
        assert!(!c.allow_http);
        assert_eq!(w.len(), 6, "{w:?}");
        // Warnings name the variable, never a URL or a key.
        for x in &w {
            assert!(!x.contains("u:p@") && !x.contains("sk-secret") && !x.contains("aliyuncs"));
        }
        assert_eq!(
            format!("{:?}", c.endpoints["ANTHROPIC"].key),
            "Some(<redacted>)"
        );
        assert!(!format!("{c:?}").contains("sk-secret"));
    }

    #[test]
    fn d17_lists() {
        let c = AiConfig::default();
        assert!(c.model_disallowed("Qwen2.5-72B-Instruct"));
        assert!(c.model_disallowed("deepseek-chat"));
        assert!(!c.model_disallowed("claude-sonnet-5-5"));
        assert!(c.host_disallowed("api.deepseek.com"));
        assert!(!c.host_disallowed("api.anthropic.com"));
    }

    #[test]
    fn allow_http_switch() {
        let (c, _) = from_vars(vars(&[("OXLEDGER_AI_ALLOW_HTTP", "1")]));
        assert!(c.allow_http);
    }
}
