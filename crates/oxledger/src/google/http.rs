// SPDX-License-Identifier: GPL-2.0-or-later
//! Google's token endpoint over HTTPS (feature `google`): the same hyper /
//! rustls stack as the GitHub and AI clients, no new crate. HTTPS only, no
//! redirects followed (hyper never follows them), ten seconds at most.

use super::{Exchange, Fut, GoogleApi, TOKEN_URL};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::Value as Json;
use std::sync::Arc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Google's answer is a few kilobytes.
const MAX_ANSWER: usize = 64 * 1024;

pub struct HttpGoogle {
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl HttpGoogle {
    pub fn new() -> Result<HttpGoogle> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .build();
        Ok(HttpGoogle {
            http: Client::builder(TokioExecutor::new()).build(conn),
        })
    }

    async fn post(&self, x: Exchange<'_>) -> Result<Json> {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", x.code)
            .append_pair("code_verifier", x.verifier)
            .append_pair("client_id", x.client_id)
            .append_pair("client_secret", x.client_secret)
            .append_pair("redirect_uri", x.redirect_uri)
            .finish();
        let req = Request::post(TOKEN_URL)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .header("user-agent", "oxledger")
            .body(Full::new(Bytes::from(form)))?;
        let res = self
            .http
            .request(req)
            .await
            .map_err(|e| anyhow!("token endpoint: {e}"))?;
        let status = res.status();
        let raw = res
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow!("token endpoint: {e}"))?
            .to_bytes();
        if raw.len() > MAX_ANSWER {
            bail!("token endpoint: the answer is too large");
        }
        let v: Json = serde_json::from_slice(&raw)
            .map_err(|_| anyhow!("token endpoint: HTTP {}, not JSON", status.as_u16()))?;
        if !status.is_success() {
            // Google's `error` is a fixed word (invalid_grant, …); the
            // description may quote the request and is left out.
            bail!(
                "token endpoint: HTTP {} ({})",
                status.as_u16(),
                v["error"].as_str().unwrap_or("no error code")
            );
        }
        claims(
            v["id_token"]
                .as_str()
                .context("no id_token in the answer")?,
        )
    }
}

/// The claims of a JWT (its middle part). The signature is not checked:
/// the token came straight from Google over TLS (see the module comment).
pub fn claims(jwt: &str) -> Result<Json> {
    let mut parts = jwt.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("the ID token is not a JWT");
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| anyhow!("the ID token is not base64url"))?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("the ID token's claims are not JSON"))
}

impl GoogleApi for HttpGoogle {
    fn exchange<'a>(&'a self, x: Exchange<'a>) -> Fut<'a, Json> {
        Box::pin(async move {
            tokio::time::timeout(TIMEOUT, self.post(x))
                .await
                .map_err(|_| anyhow!("token endpoint: no answer in time"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claims_are_the_middle_part() {
        let payload = URL_SAFE_NO_PAD.encode(json!({"sub": "108", "email": "a@b.c"}).to_string());
        let c = claims(&format!("e30.{payload}.sig")).unwrap();
        assert_eq!(c["sub"], "108");
        for bad in ["", "a.b", "a.b.c.d", "a.!!!.c"] {
            assert!(claims(bad).is_err(), "{bad:?}");
        }
    }
}
