// SPDX-License-Identifier: GPL-2.0-or-later
//! HTTPS to a registered AI endpoint (feature `ai`; design 05 §3.13.2 P-3,
//! P-7). The same hyper / rustls stack as the GitHub client, no new crate.
//!
//! Each request resolves the host, checks every address (P-3) and connects
//! to the checked IP literal: the request URI names the IP, TLS verifies
//! the registered host name (SNI via a fixed server-name resolver) and the
//! `Host` header carries it. One client per request, no connection pool,
//! no redirects followed (hyper never follows them; a 3xx is an error).

use super::client::CallError;
use super::config::{AiConfig, Endpoint};
use super::net;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::{Bytes, Incoming};
use hyper_rustls::FixedServerNameResolver;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::{CertificateDer, ServerName};
use std::net::IpAddr;
use std::sync::Arc;

pub(super) struct Opened {
    pub status: u16,
    body: Incoming,
}

impl Opened {
    /// The next data chunk, `None` at the end.
    pub async fn next(&mut self) -> Option<Result<Bytes, CallError>> {
        loop {
            match self.body.frame().await? {
                Ok(f) => {
                    if let Ok(d) = f.into_data() {
                        return Some(Ok(d));
                    }
                }
                Err(_) => return Some(Err(CallError::Malformed)),
            }
        }
    }
}

fn tls(ep: &Endpoint) -> Result<rustls::ClientConfig, CallError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for der in &ep.ca_der {
        roots
            .add(CertificateDer::from(der.clone()))
            .map_err(|_| CallError::NotConfigured)?;
    }
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| CallError::NotConfigured)?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// POSTs `body` to `path` of `ep`, pinned to a checked address.
pub(super) async fn post(
    cfg: &AiConfig,
    ep: &Endpoint,
    path: &str,
    headers: &[(&'static str, String)],
    body: Vec<u8>,
) -> Result<Opened, CallError> {
    let ip = net::pick(
        cfg.resolver.as_ref(),
        &ep.host,
        ep.port,
        ep.https,
        cfg.allow_http,
    )
    .await
    .map_err(|r| match r {
        net::Refused::Blocked => CallError::Blocked,
        net::Refused::Unresolved => CallError::Connect,
    })?;
    let literal = match ip {
        IpAddr::V4(a) => a.to_string(),
        IpAddr::V6(a) => format!("[{a}]"),
    };
    let scheme = if ep.https { "https" } else { "http" };
    let uri = format!("{scheme}://{literal}:{}{path}", ep.port);
    let b = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls(ep)?);
    let conn = if ep.https {
        let name = ServerName::try_from(ep.host.clone()).map_err(|_| CallError::NotConfigured)?;
        b.https_only()
            .with_server_name_resolver(FixedServerNameResolver::new(name))
            .enable_http1()
            .build()
    } else {
        b.https_or_http().enable_http1().build()
    };
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(conn);
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", ep.host_header())
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("user-agent", "oxledger");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let req = req
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| CallError::Malformed)?;
    // The error is not shown anywhere: it can name addresses.
    let res = client.request(req).await.map_err(|_| CallError::Connect)?;
    Ok(Opened {
        status: res.status().as_u16(),
        body: res.into_body(),
    })
}
