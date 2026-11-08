// SPDX-License-Identifier: GPL-2.0-or-later
//! `GithubApi` over GitHub's REST API (S6b.8, feature `github`).
//!
//! - App JWT: RS256 (`ring`), `iat = now − 60`, `exp = now + 540`.
//! - Installation tokens: memory only, renewed 5 minutes before expiry;
//!   never in the database, never in a log line or an error message.
//! - Pages follow `Link: rel="next"` only to the configured origin.
//! - 403/429 with the limit spent → [`RateLimited`].

use super::client::{Fut, GithubApi, HookDelivery, RateLimited};
use super::rollup::Review;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

pub const PER_PAGE: usize = 100;
pub const MAX_COMMITS: usize = 250;
/// A token is renewed when it has less than this left.
pub const TOKEN_MARGIN_SECS: i64 = 300;
const MAX_PAGES: usize = 1000;

/// The RSA key in PKCS#1 (`BEGIN RSA PRIVATE KEY`, what GitHub hands out)
/// or PKCS#8 (`BEGIN PRIVATE KEY`) PEM.
pub fn load_key(pem: &str) -> Result<RsaKeyPair> {
    let (label, der) = pem_der(pem)?;
    match label.as_str() {
        "RSA PRIVATE KEY" => {
            RsaKeyPair::from_der(&der).map_err(|_| anyhow!("not an RSA PKCS#1 key"))
        }
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der).map_err(|_| anyhow!("not an RSA PKCS#8 key")),
        _ => bail!("unsupported PEM block (want RSA PRIVATE KEY or PRIVATE KEY)"),
    }
}

fn pem_der(pem: &str) -> Result<(String, Vec<u8>)> {
    let begin = pem.find("-----BEGIN ").context("no PEM block")?;
    let rest = &pem[begin + 11..];
    let end_label = rest.find("-----").context("bad PEM header")?;
    let label = rest[..end_label].to_string();
    let body = &rest[end_label + 5..];
    let stop = body
        .find(&format!("-----END {label}-----"))
        .context("no PEM footer")?;
    let b64: String = body[..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Ok((label, STANDARD.decode(b64).context("bad PEM base64")?))
}

/// The App JWT for `app_id` at `now`.
pub fn app_jwt(key: &RsaKeyPair, app_id: i64, now: DateTime<Utc>) -> Result<String> {
    let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let iat = now.timestamp() - 60;
    let claims = json!({"iat": iat, "exp": iat + 600, "iss": app_id.to_string()});
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let msg = format!("{head}.{body}");
    let mut sig = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        msg.as_bytes(),
        &mut sig,
    )
    .map_err(|_| anyhow!("RS256 signing failed"))?;
    Ok(format!("{msg}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// Test helpers: the public key of a PEM private key (PKCS#1 DER) and
/// a JWT check, so the mock GitHub can verify what the client sends.
#[cfg(feature = "test-support")]
pub mod testing {
    use super::*;
    use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    pub fn public_key(pem: &str) -> Result<Vec<u8>> {
        Ok(load_key(pem)?.public_key().as_ref().to_vec())
    }

    /// The claims when the RS256 signature checks out.
    pub fn verify_jwt(public: &[u8], jwt: &str) -> Option<Json> {
        let (msg, sig) = jwt.rsplit_once('.')?;
        let (head, body) = msg.split_once('.')?;
        let h: Json = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(head).ok()?).ok()?;
        if h["alg"] != "RS256" {
            return None;
        }
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
            .verify(msg.as_bytes(), &URL_SAFE_NO_PAD.decode(sig).ok()?)
            .ok()?;
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()
    }
}

/// GitHub answered 401 (the cached token is dropped).
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unauthorized")
    }
}

impl std::error::Error for Unauthorized {}

/// An installation token and when it expires.
type Token = (Arc<str>, DateTime<Utc>);

enum Auth {
    App,
    Installation(i64),
}

pub struct HttpGithub {
    base: Url,
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    app_id: i64,
    key: RsaKeyPair,
    tokens: Mutex<HashMap<i64, Token>>,
}

impl std::fmt::Debug for HttpGithub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HttpGithub {{ base: {}, app_id: {}, key: <redacted> }}",
            self.base, self.app_id
        )
    }
}

impl HttpGithub {
    /// `allow_http` is for the mock GitHub in tests only.
    pub fn new(base: &str, app_id: i64, key_pem: &str, allow_http: bool) -> Result<HttpGithub> {
        let base = Url::parse(base).context("OXLEDGER_GITHUB_API_URL is not a URL")?;
        match base.scheme() {
            "https" => {}
            "http" if allow_http => {}
            _ => bail!("OXLEDGER_GITHUB_API_URL must be https"),
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let b = hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls);
        let conn = if allow_http {
            b.https_or_http().enable_http1().build()
        } else {
            b.https_only().enable_http1().build()
        };
        Ok(HttpGithub {
            base,
            http: Client::builder(TokioExecutor::new()).build(conn),
            app_id,
            key: load_key(key_pem)?,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        let mut u = self.base.clone();
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        let prefix = u.path().trim_end_matches('/').to_string();
        u.set_path(&format!("{prefix}{p}"));
        u.set_query(if q.is_empty() { None } else { Some(q) });
        Ok(u)
    }

    async fn token(&self, inst: i64) -> Result<Arc<str>> {
        let now = Utc::now();
        if let Some((t, exp)) = self.tokens.lock().unwrap().get(&inst)
            && *exp - now > Duration::seconds(TOKEN_MARGIN_SECS)
        {
            return Ok(t.clone());
        }
        let url = self.url(&format!("/app/installations/{inst}/access_tokens"))?;
        let jwt = format!("Bearer {}", app_jwt(&self.key, self.app_id, now)?);
        let (_, _, v) = self
            .send_raw(Method::POST, url, jwt, Some(json!({})))
            .await?;
        let t: Arc<str> = v["token"]
            .as_str()
            .context("no token in the answer")?
            .into();
        let exp = v["expires_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now + Duration::minutes(55));
        self.tokens.lock().unwrap().insert(inst, (t.clone(), exp));
        Ok(t)
    }

    /// One request; 404 → `Ok((404, …, Null))`; other errors carry the
    /// method, path and status only.
    async fn send(
        &self,
        method: Method,
        url: Url,
        auth: Auth,
        body: Option<Json>,
    ) -> Result<(StatusCode, HeaderMap, Json)> {
        let authz = match &auth {
            Auth::App => format!("Bearer {}", app_jwt(&self.key, self.app_id, Utc::now())?),
            Auth::Installation(i) => format!("token {}", self.token(*i).await?),
        };
        let r = self.send_raw(method, url, authz, body).await;
        // A revoked token is not reused until it would have expired.
        if let (Auth::Installation(i), Err(e)) = (&auth, &r)
            && e.downcast_ref::<Unauthorized>().is_some()
        {
            self.tokens.lock().unwrap().remove(i);
        }
        r
    }

    async fn send_raw(
        &self,
        method: Method,
        url: Url,
        authz: String,
        body: Option<Json>,
    ) -> Result<(StatusCode, HeaderMap, Json)> {
        let bytes = match &body {
            Some(b) => serde_json::to_vec(b)?,
            None => Vec::new(),
        };
        let req = Request::builder()
            .method(method.clone())
            .uri(url.as_str())
            .header("authorization", authz)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "oxledger")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(bytes)))?;
        let what = format!("GitHub {method} {}", url.path());
        let res = self
            .http
            .request(req)
            .await
            .map_err(|e| anyhow!("{what}: {e}"))?;
        let status = res.status();
        let headers = res.headers().clone();
        let raw = res
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow!("{what}: {e}"))?
            .to_bytes();
        if let Some(r) = rate_limited(status, &headers) {
            return Err(r.into());
        }
        if status == StatusCode::NOT_FOUND {
            return Ok((status, headers, Json::Null));
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow::Error::new(Unauthorized).context(format!("{what}: HTTP 401")));
        }
        if !status.is_success() {
            bail!("{what}: HTTP {}", status.as_u16());
        }
        let v = if raw.is_empty() {
            Json::Null
        } else {
            serde_json::from_slice(&raw).map_err(|_| anyhow!("{what}: the answer is not JSON"))?
        };
        Ok((status, headers, v))
    }

    async fn get_one(&self, auth: Auth, path: &str) -> Result<Option<Json>> {
        let (st, _, v) = self.send(Method::GET, self.url(path)?, auth, None).await?;
        Ok((st != StatusCode::NOT_FOUND).then_some(v))
    }

    /// Every page of a list; `key` names the array in an object answer.
    /// `stop` ends early once a page's item says so (sorted lists).
    async fn pages(
        &self,
        inst: Option<i64>,
        path: &str,
        key: Option<&str>,
        limit: usize,
        stop: &(dyn Fn(&Json) -> bool + Sync),
    ) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut url = self.url(path)?;
        for _ in 0..MAX_PAGES {
            let auth = match inst {
                Some(i) => Auth::Installation(i),
                None => Auth::App,
            };
            let (st, h, v) = self.send(Method::GET, url.clone(), auth, None).await?;
            if st == StatusCode::NOT_FOUND {
                bail!("GitHub GET {}: HTTP 404", url.path());
            }
            let items = match key {
                Some(k) => v[k].as_array().cloned().unwrap_or_default(),
                None => v.as_array().cloned().unwrap_or_default(),
            };
            let mut done = false;
            for it in items {
                if stop(&it) {
                    done = true;
                    break;
                }
                out.push(it);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            match next_link(&h) {
                Some(n) if !done => {
                    let n = Url::parse(&n).context("bad Link header")?;
                    if n.origin() != self.base.origin() {
                        bail!("GitHub Link header points to another origin");
                    }
                    url = n;
                }
                _ => return Ok(out),
            }
        }
        bail!("GitHub list {path}: too many pages")
    }
}

fn rate_limited(status: StatusCode, h: &HeaderMap) -> Option<RateLimited> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
    if let Some(s) = get("retry-after").and_then(|v| v.trim().parse::<i64>().ok()) {
        return Some(RateLimited {
            reset_at: Utc::now() + Duration::seconds(s),
        });
    }
    if get("x-ratelimit-remaining") == Some("0") {
        let reset = get("x-ratelimit-reset")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .unwrap_or_else(|| Utc::now() + Duration::minutes(1));
        return Some(RateLimited { reset_at: reset });
    }
    None
}

/// `<url>; rel="next"` from a `Link` header.
pub fn next_link(h: &HeaderMap) -> Option<String> {
    let v = h.get("link")?.to_str().ok()?;
    v.split(',').find_map(|part| {
        let (u, rest) = part.split_once(';')?;
        rest.split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| {
                u.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

fn updated_before(since: Option<DateTime<Utc>>) -> impl Fn(&Json) -> bool + Sync {
    move |it: &Json| match (since, it["updated_at"].as_str()) {
        (Some(s), Some(u)) => DateTime::parse_from_rfc3339(u).is_ok_and(|u| u < s),
        _ => false,
    }
}

fn never(_: &Json) -> bool {
    false
}

impl GithubApi for HttpGithub {
    fn get_pull<'a>(&'a self, inst: i64, repo: &'a str, n: i32) -> Fut<'a, Option<Json>> {
        Box::pin(async move {
            self.get_one(
                Auth::Installation(inst),
                &format!("/repos/{repo}/pulls/{n}"),
            )
            .await
        })
    }

    fn list_pulls<'a>(
        &'a self,
        inst: i64,
        repo: &'a str,
        state: &'a str,
        since: Option<DateTime<Utc>>,
    ) -> Fut<'a, Vec<Json>> {
        Box::pin(async move {
            let path = format!(
                "/repos/{repo}/pulls?state={state}&sort=updated&direction=desc&per_page={PER_PAGE}"
            );
            self.pages(Some(inst), &path, None, usize::MAX, &updated_before(since))
                .await
        })
    }

    fn list_pull_commits<'a>(&'a self, inst: i64, repo: &'a str, n: i32) -> Fut<'a, Vec<String>> {
        Box::pin(async move {
            let path = format!("/repos/{repo}/pulls/{n}/commits?per_page={PER_PAGE}");
            let v = self
                .pages(Some(inst), &path, None, MAX_COMMITS, &never)
                .await?;
            Ok(v.iter()
                .filter_map(|c| c["commit"]["message"].as_str().map(str::to_string))
                .collect())
        })
    }

    fn list_reviews<'a>(&'a self, inst: i64, repo: &'a str, n: i32) -> Fut<'a, Vec<Review>> {
        Box::pin(async move {
            let path = format!("/repos/{repo}/pulls/{n}/reviews?per_page={PER_PAGE}");
            let v = self
                .pages(Some(inst), &path, None, usize::MAX, &never)
                .await?;
            Ok(v.iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    Some(Review {
                        reviewer: r["user"]["id"].as_i64()?,
                        state: r["state"].as_str()?.to_string(),
                        order: i as i64,
                    })
                })
                .collect())
        })
    }

    fn get_check_suite<'a>(&'a self, inst: i64, repo: &'a str, id: i64) -> Fut<'a, Option<Json>> {
        Box::pin(async move {
            self.get_one(
                Auth::Installation(inst),
                &format!("/repos/{repo}/check-suites/{id}"),
            )
            .await
        })
    }

    fn get_check_run<'a>(&'a self, inst: i64, repo: &'a str, id: i64) -> Fut<'a, Option<Json>> {
        Box::pin(async move {
            self.get_one(
                Auth::Installation(inst),
                &format!("/repos/{repo}/check-runs/{id}"),
            )
            .await
        })
    }

    fn checks_for_ref<'a>(
        &'a self,
        inst: i64,
        repo: &'a str,
        sha: &'a str,
    ) -> Fut<'a, (Vec<Json>, Vec<Json>, Vec<Json>)> {
        Box::pin(async move {
            let base = format!("/repos/{repo}/commits/{sha}");
            let suites = self
                .pages(
                    Some(inst),
                    &format!("{base}/check-suites?per_page={PER_PAGE}"),
                    Some("check_suites"),
                    usize::MAX,
                    &never,
                )
                .await?;
            let runs = self
                .pages(
                    Some(inst),
                    &format!("{base}/check-runs?per_page={PER_PAGE}"),
                    Some("check_runs"),
                    usize::MAX,
                    &never,
                )
                .await?;
            let mut statuses = self
                .pages(
                    Some(inst),
                    &format!("{base}/statuses?per_page={PER_PAGE}"),
                    None,
                    usize::MAX,
                    &never,
                )
                .await?;
            // Items of this list do not repeat the commit.
            for s in &mut statuses {
                s["sha"] = json!(sha);
            }
            Ok((suites, runs, statuses))
        })
    }

    fn list_installations(&self) -> Fut<'_, Vec<(i64, String, bool)>> {
        Box::pin(async move {
            let path = format!("/app/installations?per_page={PER_PAGE}");
            let v = self.pages(None, &path, None, usize::MAX, &never).await?;
            Ok(v.iter()
                .filter_map(|i| {
                    Some((
                        i["id"].as_i64()?,
                        i["account"]["login"].as_str().unwrap_or("").to_string(),
                        !i["suspended_at"].is_null(),
                    ))
                })
                .collect())
        })
    }

    fn list_installation_repos(&self, inst: i64) -> Fut<'_, Vec<Json>> {
        Box::pin(async move {
            let path = format!("/installation/repositories?per_page={PER_PAGE}");
            self.pages(Some(inst), &path, Some("repositories"), usize::MAX, &never)
                .await
        })
    }

    fn list_hook_deliveries(&self, since: DateTime<Utc>) -> Fut<'_, Vec<HookDelivery>> {
        Box::pin(async move {
            let path = format!("/app/hook/deliveries?per_page={PER_PAGE}");
            let stop = move |it: &Json| {
                it["delivered_at"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|d| d < since)
            };
            let v = self.pages(None, &path, None, usize::MAX, &stop).await?;
            Ok(v.iter()
                .filter_map(|d| {
                    Some(HookDelivery {
                        id: d["id"].as_i64()?,
                        guid: d["guid"].as_str()?.to_string(),
                        status_code: d["status_code"].as_u64().unwrap_or(0) as u16,
                        delivered_at: DateTime::parse_from_rfc3339(d["delivered_at"].as_str()?)
                            .ok()?
                            .with_timezone(&Utc),
                    })
                })
                .collect())
        })
    }

    fn redeliver(&self, id: i64) -> Fut<'_, ()> {
        Box::pin(async move {
            let url = self.url(&format!("/app/hook/deliveries/{id}/attempts"))?;
            let (st, _, _) = self.send(Method::POST, url, Auth::App, None).await?;
            if st == StatusCode::NOT_FOUND {
                bail!("GitHub redeliver: HTTP 404");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header() {
        let mut h = HeaderMap::new();
        h.insert(
            "link",
            r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        h.insert(
            "link",
            r#"<https://api.github.com/x?page=1>; rel="prev""#.parse().unwrap(),
        );
        assert_eq!(next_link(&h), None);
    }

    #[test]
    fn rate_limit_headers() {
        let mut h = HeaderMap::new();
        assert!(
            rate_limited(StatusCode::FORBIDDEN, &h).is_none(),
            "plain 403"
        );
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        h.insert("x-ratelimit-reset", "2000000000".parse().unwrap());
        assert_eq!(
            rate_limited(StatusCode::FORBIDDEN, &h)
                .unwrap()
                .reset_at
                .timestamp(),
            2_000_000_000
        );
        assert!(rate_limited(StatusCode::OK, &h).is_none());
        let mut h = HeaderMap::new();
        h.insert("retry-after", "30".parse().unwrap());
        assert!(rate_limited(StatusCode::TOO_MANY_REQUESTS, &h).is_some());
    }
}
