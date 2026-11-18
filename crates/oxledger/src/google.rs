// SPDX-License-Identifier: GPL-2.0-or-later
//! Google sign-in (OpenID Connect, authorization code flow with PKCE) and
//! opening a new site with it (migration 0042).
//!
//! * Still no self sign-up (contract 02 §3.3, U4): a Google account signs in
//!   only as a user an administrator created with the same e-mail address.
//!   The first sign-in links the account (`sub`) to the user; later ones go
//!   by `sub`.
//! * A site with no user at all is opened by its first Google sign-in: that
//!   person names the organization and becomes its administrator
//!   ([`setup`]). `OXLEDGER_SETUP_EMAILS` limits who that may be.
//! * Password sign-in stays; a user may have both, either or (CLI
//!   `--no-password`) only Google.
//!
//! The ID token is read from Google's token endpoint over TLS, which is what
//! authenticates it (OpenID Connect Core §3.1.3.7, rule 6): its signature is
//! not checked again. Its issuer, audience, expiry, nonce and
//! `email_verified` are.
//!
//! Off unless `OXLEDGER_GOOGLE_CLIENT_ID` and `OXLEDGER_GOOGLE_CLIENT_SECRET`
//! are set and the build has the `google` feature.

use crate::auth::{self, NewSession, SessionPolicy};
use crate::error::{ApiError, ErrorCode, api};
use anyhow::{Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

#[cfg(feature = "google")]
pub mod http;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The callback, under the SPA origin (register it in the Google client).
pub const CALLBACK_PATH: &str = "/api/v1/auth/google/callback";
/// How long a round trip to Google may take.
const STATE_TTL_MINUTES: i64 = 10;
/// How long the setup form may stay open.
const TICKET_TTL_MINUTES: i64 = 15;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// What is sent to the token endpoint.
pub struct Exchange<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub code: &'a str,
    pub verifier: &'a str,
    pub redirect_uri: &'a str,
}

/// Google's token endpoint. The HTTPS implementation is [`http::HttpGoogle`]
/// (feature `google`); tests bring their own.
pub trait GoogleApi: Send + Sync {
    /// Trades the code for tokens and returns the claims of the ID token.
    fn exchange<'a>(&'a self, x: Exchange<'a>) -> Fut<'a, Json>;
}

#[derive(Clone)]
pub struct GoogleSettings {
    pub client_id: String,
    pub client_secret: String,
    /// When not empty, only addresses in these domains sign in (lower case).
    pub allowed_domains: Vec<String>,
    /// When not empty, only these addresses may open a new site (lower case).
    pub setup_emails: Vec<String>,
    pub api: Arc<dyn GoogleApi>,
}

impl std::fmt::Debug for GoogleSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleSettings")
            .field("client_id", &self.client_id)
            .field("allowed_domains", &self.allowed_domains)
            .field("setup_emails", &self.setup_emails)
            .finish_non_exhaustive()
    }
}

fn list(v: Option<String>) -> Vec<String> {
    v.unwrap_or_default()
        .split([',', ' '])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The settings without the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleVars {
    pub client_id: String,
    pub client_secret: String,
    pub allowed_domains: Vec<String>,
    pub setup_emails: Vec<String>,
}

impl GoogleVars {
    pub fn with(self, api: Arc<dyn GoogleApi>) -> GoogleSettings {
        GoogleSettings {
            client_id: self.client_id,
            client_secret: self.client_secret,
            allowed_domains: self.allowed_domains,
            setup_emails: self.setup_emails,
            api,
        }
    }
}

/// The settings from the environment: `None` when Google sign-in is not
/// configured.
pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<GoogleVars>> {
    let var = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let id = var("OXLEDGER_GOOGLE_CLIENT_ID");
    let secret = var("OXLEDGER_GOOGLE_CLIENT_SECRET");
    match (id, secret) {
        (None, None) => Ok(None),
        (Some(client_id), Some(client_secret)) => Ok(Some(GoogleVars {
            client_id,
            client_secret,
            allowed_domains: list(get("OXLEDGER_GOOGLE_ALLOWED_DOMAINS")),
            setup_emails: list(get("OXLEDGER_SETUP_EMAILS")),
        })),
        _ => bail!(
            "set both OXLEDGER_GOOGLE_CLIENT_ID and OXLEDGER_GOOGLE_CLIENT_SECRET, or neither"
        ),
    }
}

/// The HTTPS client, when this build has one.
#[cfg(feature = "google")]
pub fn client() -> Result<Option<Arc<dyn GoogleApi>>> {
    Ok(Some(Arc::new(http::HttpGoogle::new()?)))
}

#[cfg(not(feature = "google"))]
pub fn client() -> Result<Option<Arc<dyn GoogleApi>>> {
    Ok(None)
}

static CURRENT: RwLock<Option<Arc<GoogleSettings>>> = RwLock::new(None);

/// Installs the settings for this process (the server at start-up; tests
/// install their own). `None` turns Google sign-in off.
pub fn install(s: Option<GoogleSettings>) {
    if let Ok(mut g) = CURRENT.write() {
        *g = s.map(Arc::new);
    }
}

pub fn current() -> Option<Arc<GoogleSettings>> {
    CURRENT.read().ok().and_then(|g| g.clone())
}

pub fn enabled() -> bool {
    current().is_some()
}

// ---------------------------------------------------------------- refusals

/// Why a round trip ended without a session. The sign-in page shows a text
/// for each (`?google_error=<code>`); nothing else about the account leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Google sign-in is off on this server.
    NotConfigured,
    /// The person cancelled at Google, or Google reported an error.
    Cancelled,
    /// Unknown, expired or foreign `state` (another browser, a replay).
    State,
    /// The token endpoint failed or answered something unusable.
    Exchange,
    /// The address is not verified by Google.
    Unverified,
    /// The address is outside `OXLEDGER_GOOGLE_ALLOWED_DOMAINS`.
    Domain,
    /// The user is locked.
    Locked,
    /// The user is linked to another Google account, or (step-up) this is
    /// not the signed-in person's Google account.
    OtherAccount,
    /// A new site, but this address may not open it (`OXLEDGER_SETUP_EMAILS`).
    SetupNotAllowed,
    /// Step-up without a valid session.
    SignedOut,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::NotConfigured => "not_configured",
            Refusal::Cancelled => "cancelled",
            Refusal::State => "state",
            Refusal::Exchange => "exchange",
            Refusal::Unverified => "unverified",
            Refusal::Domain => "domain",
            Refusal::Locked => "locked",
            Refusal::OtherAccount => "other_account",
            Refusal::SetupNotAllowed => "setup_not_allowed",
            Refusal::SignedOut => "signed_out",
        }
    }
}

// ---------------------------------------------------------------- helpers

/// A lower-case address, or `None` when it does not look like one.
pub fn normalize_email(s: &str) -> Option<String> {
    let e = s.trim().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && e.len() <= 254
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then_some(e)
}

/// A path on this origin to go back to, `/` for anything else (never an
/// open redirect: no scheme, no `//host`, no backslash a browser reads as
/// a slash, no control characters).
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && n.len() <= 2000
                && !n.chars().any(|c| c == '\\' || c.is_control()) =>
        {
            n.to_string()
        }
        _ => "/".to_string(),
    }
}

fn redirect_uri(spa_origin: &str) -> String {
    format!("{}{CALLBACK_PATH}", spa_origin.trim_end_matches('/'))
}

fn secret() -> String {
    auth::encode(&auth::random_secret())
}

// ---------------------------------------------------------------- start

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Login,
    /// Step-up of this session.
    Reauth(Uuid),
}

pub struct Started {
    /// Where to send the browser.
    pub url: String,
    /// Goes into the state cookie.
    pub cookie: String,
    pub max_age_secs: i64,
}

/// Begins a round trip: stores the state and builds Google's URL.
pub async fn start(
    pool: &PgPool,
    spa_origin: &str,
    purpose: Purpose,
    next: Option<&str>,
) -> Result<Result<Started, Refusal>> {
    let Some(cfg) = current() else {
        return Ok(Err(Refusal::NotConfigured));
    };
    let (state, cookie, nonce, verifier) = (secret(), secret(), secret(), secret());
    let (purpose_s, session, hint) = match purpose {
        Purpose::Login => ("login", None, None),
        Purpose::Reauth(s) => {
            // The account the session's person is linked to, as a hint.
            let hint: Option<String> = sqlx::query_scalar(
                "SELECT i.email FROM browser_sessions s
                   JOIN user_identities i ON i.user_id = s.user_id AND i.provider = 'google'
                  WHERE s.id = $1",
            )
            .bind(s)
            .fetch_optional(pool)
            .await?;
            ("reauth", Some(s), hint)
        }
    };
    sqlx::query(
        "INSERT INTO oidc_states (state_sha256, cookie_sha256, nonce, pkce_verifier, purpose,
                                  session_id, next_path, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7, now() + make_interval(mins => $8))",
    )
    .bind(&auth::sha256(state.as_bytes())[..])
    .bind(&auth::sha256(cookie.as_bytes())[..])
    .bind(&nonce)
    .bind(&verifier)
    .bind(purpose_s)
    .bind(session)
    .bind(safe_next(next))
    .bind(STATE_TTL_MINUTES as i32)
    .execute(pool)
    .await?;
    let challenge = URL_SAFE_NO_PAD.encode(auth::sha256(verifier.as_bytes()));
    let redirect = redirect_uri(spa_origin);
    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &cfg.client_id),
        ("redirect_uri", &redirect),
        ("response_type", "code"),
        ("scope", "openid email profile"),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("prompt", "select_account"),
    ];
    if let Some(h) = &hint {
        params.push(("login_hint", h));
    }
    let url = url::Url::parse_with_params(AUTH_URL, &params)?;
    Ok(Ok(Started {
        url: url.into(),
        cookie,
        max_age_secs: STATE_TTL_MINUTES * 60,
    }))
}

// ---------------------------------------------------------------- claims

/// Who Google says this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: String,
    pub name: String,
}

/// Checks the ID token's claims (OpenID Connect Core §3.1.3.7).
pub fn check_claims(
    claims: &Json,
    cfg: &GoogleSettings,
    nonce: &str,
    now: DateTime<Utc>,
) -> Result<Identity, Refusal> {
    let text = |k: &str| claims[k].as_str().unwrap_or("");
    let iss_ok = matches!(
        text("iss"),
        "https://accounts.google.com" | "accounts.google.com"
    );
    // `aud` is one string for Google; an array must hold only this client.
    let aud_ok = match &claims["aud"] {
        Json::String(a) => *a == cfg.client_id,
        Json::Array(a) => a.len() == 1 && a[0].as_str() == Some(&cfg.client_id),
        _ => false,
    };
    let exp_ok = claims["exp"].as_i64().is_some_and(|e| e > now.timestamp());
    let nonce_ok =
        !nonce.is_empty() && auth::bytes_match(text("nonce").as_bytes(), nonce.as_bytes());
    let subject = text("sub");
    if !(iss_ok && aud_ok && exp_ok && nonce_ok) || subject.is_empty() || subject.len() > 255 {
        return Err(Refusal::Exchange);
    }
    // Google sends a boolean; some libraries have seen the string "true".
    let verified = match &claims["email_verified"] {
        Json::Bool(b) => *b,
        Json::String(s) => s == "true",
        _ => false,
    };
    let Some(email) = normalize_email(text("email")).filter(|_| verified) else {
        return Err(Refusal::Unverified);
    };
    if !cfg.allowed_domains.is_empty() {
        let domain = email.rsplit_once('@').map_or("", |(_, d)| d);
        if !cfg.allowed_domains.iter().any(|d| d == domain) {
            return Err(Refusal::Domain);
        }
    }
    let name = match text("name").trim() {
        "" => email.split('@').next().unwrap_or("").to_string(),
        n => n.chars().take(100).collect(),
    };
    Ok(Identity {
        subject: subject.to_string(),
        email,
        name,
    })
}

// ---------------------------------------------------------------- finish

pub enum Outcome {
    SignedIn {
        session: NewSession,
        next: String,
    },
    /// The step-up of the current session is done.
    Reauthed {
        next: String,
    },
    /// A new site: the ticket goes into the setup cookie, the browser to
    /// the setup page.
    Setup {
        ticket: String,
        max_age_secs: i64,
    },
}

/// What the callback was called with.
pub struct Callback<'a> {
    pub state: Option<&'a str>,
    pub code: Option<&'a str>,
    /// Google's `error` parameter.
    pub error: Option<&'a str>,
    /// The state cookie.
    pub cookie: Option<&'a str>,
    /// The valid browser session the request carried, if any.
    pub session: Option<Uuid>,
}

/// Ends a round trip. The state is used up whatever happens.
pub async fn finish(
    pool: &PgPool,
    policy: &SessionPolicy,
    spa_origin: &str,
    cb: Callback<'_>,
) -> Result<Result<Outcome, Refusal>> {
    let Some(cfg) = current() else {
        return Ok(Err(Refusal::NotConfigured));
    };
    let Some(state) = cb.state.filter(|s| !s.is_empty()) else {
        return Ok(Err(Refusal::State));
    };
    type Row = (
        Vec<u8>,
        String,
        String,
        String,
        Option<Uuid>,
        String,
        DateTime<Utc>,
    );
    let row: Option<Row> = sqlx::query_as(
        "DELETE FROM oidc_states WHERE state_sha256 = $1
         RETURNING cookie_sha256, nonce, pkce_verifier, purpose, session_id, next_path, expires_at",
    )
    .bind(&auth::sha256(state.as_bytes())[..])
    .fetch_optional(pool)
    .await?;
    let Some((cookie_hash, nonce, verifier, purpose, reauth_session, next, expires)) = row else {
        return Ok(Err(Refusal::State));
    };
    let cookie_ok = cb
        .cookie
        .is_some_and(|c| auth::bytes_match(&auth::sha256(c.as_bytes()), &cookie_hash));
    if !cookie_ok || expires <= Utc::now() {
        return Ok(Err(Refusal::State));
    }
    if cb.error.is_some() {
        return Ok(Err(Refusal::Cancelled));
    }
    let Some(code) = cb.code.filter(|c| !c.is_empty()) else {
        return Ok(Err(Refusal::Cancelled));
    };
    let redirect = redirect_uri(spa_origin);
    let claims = match cfg
        .api
        .exchange(Exchange {
            client_id: &cfg.client_id,
            client_secret: &cfg.client_secret,
            code,
            verifier: &verifier,
            redirect_uri: &redirect,
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Google token exchange failed: {e:#}");
            return Ok(Err(Refusal::Exchange));
        }
    };
    let who = match check_claims(&claims, &cfg, &nonce, Utc::now()) {
        Ok(w) => w,
        Err(r) => return Ok(Err(r)),
    };
    if purpose == "reauth" {
        return reauth(pool, cb.session, reauth_session, &who, next).await;
    }
    sign_in(pool, policy, &cfg, &who, next).await
}

async fn reauth(
    pool: &PgPool,
    current: Option<Uuid>,
    wanted: Option<Uuid>,
    who: &Identity,
    next: String,
) -> Result<Result<Outcome, Refusal>> {
    // The session that asked must be the one the browser still has.
    let (Some(session), true) = (current, current == wanted) else {
        return Ok(Err(Refusal::SignedOut));
    };
    let n = sqlx::query(
        "UPDATE browser_sessions s SET reauthenticated_at = now()
          WHERE s.id = $1 AND s.revoked_at IS NULL
            AND EXISTS (SELECT 1 FROM user_identities i
                         WHERE i.provider = 'google' AND i.subject = $2 AND i.user_id = s.user_id)",
    )
    .bind(session)
    .bind(&who.subject)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(Err(Refusal::OtherAccount));
    }
    Ok(Ok(Outcome::Reauthed { next }))
}

async fn sign_in(
    pool: &PgPool,
    policy: &SessionPolicy,
    cfg: &GoogleSettings,
    who: &Identity,
    next: String,
) -> Result<Result<Outcome, Refusal>> {
    let mut tx = pool.begin().await?;
    let linked: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT p.id, p.status FROM user_identities i JOIN principals p ON p.id = i.user_id
          WHERE i.provider = 'google' AND i.subject = $1 AND p.kind = 'user'",
    )
    .bind(&who.subject)
    .fetch_optional(&mut *tx)
    .await?;
    let (user, status) = match linked {
        Some(u) => u,
        None => {
            // First sign-in: the user an administrator made with this address.
            let by_email: Option<(Uuid, String)> = sqlx::query_as(
                "SELECT id, status FROM principals WHERE kind = 'user' AND email = $1 FOR UPDATE",
            )
            .bind(&who.email)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((user, status)) = by_email else {
                let any_user: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM principals WHERE kind = 'user')",
                )
                .fetch_one(&mut *tx)
                .await?;
                if any_user {
                    // Self sign-up: an ordinary user without a password,
                    // linked to this Google account from the start.
                    let user = sign_up(&mut tx, who).await?;
                    let session = auth::session_for(&mut tx, policy, user, "google").await?;
                    tx.commit().await?;
                    return Ok(Ok(Outcome::SignedIn { session, next }));
                }
                if !cfg.setup_emails.is_empty() && !cfg.setup_emails.contains(&who.email) {
                    return Ok(Err(Refusal::SetupNotAllowed));
                }
                let ticket = secret();
                sqlx::query(
                    "INSERT INTO setup_tickets (token_sha256, subject, email, name, expires_at)
                     VALUES ($1,$2,$3,$4, now() + make_interval(mins => $5))",
                )
                .bind(&auth::sha256(ticket.as_bytes())[..])
                .bind(&who.subject)
                .bind(&who.email)
                .bind(&who.name)
                .bind(TICKET_TTL_MINUTES as i32)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(Ok(Outcome::Setup {
                    ticket,
                    max_age_secs: TICKET_TTL_MINUTES * 60,
                }));
            };
            // A user already linked to a Google account keeps that one.
            let n = sqlx::query(
                "INSERT INTO user_identities (provider, subject, user_id, email)
                 VALUES ('google', $1, $2, $3) ON CONFLICT (provider, user_id) DO NOTHING",
            )
            .bind(&who.subject)
            .bind(user)
            .bind(&who.email)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n == 0 {
                return Ok(Err(Refusal::OtherAccount));
            }
            auth::audit(
                &mut tx,
                "google_linked",
                Some(user),
                Some(user),
                json!({"email": who.email}),
            )
            .await?;
            (user, status)
        }
    };
    if status != "active" {
        return Ok(Err(Refusal::Locked));
    }
    let session = auth::session_for(&mut tx, policy, user, "google").await?;
    tx.commit().await?;
    Ok(Ok(Outcome::SignedIn { session, next }))
}

/// Makes the user for a Google account nobody has seen: not an
/// administrator, no password, no memberships. The login is the part of the
/// address before `@`, with a number after it when that is taken.
async fn sign_up(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, who: &Identity) -> Result<Uuid> {
    let base: String = who
        .email
        .split('@')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .take(60)
        .collect();
    let base = if base.is_empty() {
        "user".to_string()
    } else {
        base
    };
    let mut made: Option<(Uuid, String)> = None;
    for n in 1..=100 {
        let login = if n == 1 {
            base.clone()
        } else {
            format!("{base}{n}")
        };
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO principals (kind, login, name, is_admin, email)
             VALUES ('user', $1, $2, false, $3) ON CONFLICT (login) DO NOTHING RETURNING id",
        )
        .bind(&login)
        .bind(&who.name)
        .bind(&who.email)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(id) = id {
            made = Some((id, login));
            break;
        }
    }
    let (user, login) =
        made.ok_or_else(|| anyhow::anyhow!("no free login for a new Google user"))?;
    sqlx::query("INSERT INTO user_passwords (user_id, password_phc) VALUES ($1, NULL)")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO user_identities (provider, subject, user_id, email) VALUES ('google',$1,$2,$3)",
    )
    .bind(&who.subject)
    .bind(user)
    .bind(&who.email)
    .execute(&mut **tx)
    .await?;
    auth::audit(
        tx,
        "google_signed_up",
        Some(user),
        Some(user),
        json!({"login": login, "email": who.email}),
    )
    .await?;
    Ok(user)
}

// ---------------------------------------------------------------- setup

/// What the setup form starts with.
pub struct SetupContext {
    pub email: String,
    pub name: String,
    /// The part of the address before `@`.
    pub login: String,
    pub timezone: String,
}

type Ticket = (String, String, String);

async fn ticket(conn: &mut sqlx::PgConnection, cookie: Option<&str>) -> Result<Option<Ticket>> {
    let Some(c) = cookie.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    Ok(sqlx::query_as(
        "SELECT subject, email, name FROM setup_tickets
          WHERE token_sha256 = $1 AND expires_at > now()
            AND NOT EXISTS (SELECT 1 FROM principals WHERE kind = 'user')",
    )
    .bind(&auth::sha256(c.as_bytes())[..])
    .fetch_optional(conn)
    .await?)
}

/// `None`: no ticket, an expired one, or the site has been opened since.
pub async fn setup_context(pool: &PgPool, cookie: Option<&str>) -> Result<Option<SetupContext>> {
    let mut conn = pool.acquire().await?;
    let Some((_, email, name)) = ticket(&mut conn, cookie).await? else {
        return Ok(None);
    };
    let timezone: String = sqlx::query_scalar("SELECT timezone FROM org_settings")
        .fetch_one(&mut *conn)
        .await?;
    Ok(Some(SetupContext {
        login: email.split('@').next().unwrap_or("").to_string(),
        email,
        name,
        timezone,
    }))
}

pub struct SetupInput<'a> {
    pub org_name: &'a str,
    pub timezone: &'a str,
    pub login: &'a str,
    pub name: &'a str,
    /// Load the default roles, trackers, statuses and workflow
    /// ([`crate::setup::init_defaults`]) when the site has none.
    pub load_defaults: bool,
}

fn bad(field: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidValue, msg)
        .with_details(json!({"path": field}))
        .into()
}

/// Opens the site: names the organization and makes the ticket's Google
/// account its first user, an administrator. Works once: with any user in
/// the database it answers `not_found`, like a missing ticket.
pub async fn setup(
    pool: &PgPool,
    policy: &SessionPolicy,
    cookie: Option<&str>,
    input: SetupInput<'_>,
) -> Result<NewSession> {
    let gone = || api(ErrorCode::NotFound, "this site is already set up");
    let org_name = input.org_name.trim();
    if org_name.is_empty() || org_name.chars().count() > 100 {
        return Err(bad(
            "org_name",
            "the organization needs a name of at most 100 characters",
        ));
    }
    let login = input.login.trim();
    if login.is_empty()
        || login.chars().count() > 64
        || login.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad(
            "login",
            "the login is 1 to 64 characters without spaces",
        ));
    }
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad("name", "your name is 1 to 100 characters"));
    }
    let tz_ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name = $1)")
            .bind(input.timezone)
            .fetch_one(pool)
            .await?;
    if !tz_ok {
        return Err(bad(
            "timezone",
            "unknown time zone (use an IANA name such as Asia/Taipei)",
        ));
    }
    {
        let mut conn = pool.acquire().await?;
        if ticket(&mut conn, cookie).await?.is_none() {
            return Err(gone());
        }
    }
    if input.load_defaults {
        let existing: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM roles) + (SELECT count(*) FROM trackers)
                  + (SELECT count(*) FROM issue_statuses)",
        )
        .fetch_one(pool)
        .await?;
        if existing == 0 {
            crate::setup::init_defaults(pool).await?;
        }
    }
    let mut tx = pool.begin().await?;
    // One setup at a time: two forms side by side would both see no user.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('oxledger.site_setup'))")
        .execute(&mut *tx)
        .await?;
    let Some((subject, email, _)) = ticket(&mut tx, cookie).await? else {
        return Err(gone());
    };
    sqlx::query("DELETE FROM setup_tickets")
        .execute(&mut *tx)
        .await?;
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO principals (kind, login, name, is_admin, email)
         VALUES ('user', $1, $2, true, $3) RETURNING id",
    )
    .bind(login)
    .bind(name)
    .bind(&email)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO user_passwords (user_id, password_phc) VALUES ($1, NULL)")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO user_identities (provider, subject, user_id, email) VALUES ('google',$1,$2,$3)",
    )
    .bind(&subject)
    .bind(user)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE org_settings SET name = $1, timezone = $2")
        .bind(org_name)
        .bind(input.timezone)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut tx,
        "site_setup",
        Some(user),
        Some(user),
        json!({"login": login, "email": email, "org_name": org_name}),
    )
    .await?;
    let session = auth::session_for(&mut tx, policy, user, "google").await?;
    tx.commit().await?;
    Ok(session)
}

/// Deletes expired states and tickets (the login sweeper calls it).
pub async fn sweep(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM oidc_states WHERE expires_at < now()")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM setup_tickets WHERE expires_at < now()")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Never;
    impl GoogleApi for Never {
        fn exchange<'a>(&'a self, _: Exchange<'a>) -> Fut<'a, Json> {
            Box::pin(async { bail!("not used") })
        }
    }

    fn cfg(domains: &[&str]) -> GoogleSettings {
        GoogleSettings {
            client_id: "client-1".into(),
            client_secret: "s".into(),
            allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
            setup_emails: vec![],
            api: Arc::new(Never),
        }
    }

    fn claims() -> Json {
        json!({
            "iss": "https://accounts.google.com", "aud": "client-1", "sub": "108",
            "exp": Utc::now().timestamp() + 600, "nonce": "n-1",
            "email": "Alice@Example.com", "email_verified": true, "name": "Alice Chen",
        })
    }

    #[test]
    fn claims_are_checked() {
        let now = Utc::now();
        let who = check_claims(&claims(), &cfg(&[]), "n-1", now).unwrap();
        assert_eq!(who.email, "alice@example.com");
        assert_eq!(who.subject, "108");
        assert_eq!(who.name, "Alice Chen");

        let with = |k: &str, v: Json| {
            let mut c = claims();
            c[k] = v;
            c
        };
        for c in [
            with("iss", json!("https://accounts.example.com")),
            with("aud", json!("client-2")),
            with("aud", json!(["client-1", "client-2"])),
            with("exp", json!(now.timestamp() - 1)),
            with("nonce", json!("n-2")),
            with("sub", json!("")),
        ] {
            assert_eq!(
                check_claims(&c, &cfg(&[]), "n-1", now),
                Err(Refusal::Exchange),
                "{c}"
            );
        }
        assert_eq!(
            check_claims(&with("email_verified", json!(false)), &cfg(&[]), "n-1", now),
            Err(Refusal::Unverified)
        );
        assert_eq!(
            check_claims(&claims(), &cfg(&["corp.example"]), "n-1", now),
            Err(Refusal::Domain)
        );
        assert!(check_claims(&claims(), &cfg(&["example.com"]), "n-1", now).is_ok());
        // A look-alike domain is not the allowed one.
        assert_eq!(
            check_claims(
                &with("email", json!("a@example.com.evil.test")),
                &cfg(&["example.com"]),
                "n-1",
                now
            ),
            Err(Refusal::Domain)
        );
    }

    #[test]
    fn next_stays_on_this_origin() {
        assert_eq!(
            safe_next(Some("/issue/AUTH-1?tab=wiki")),
            "/issue/AUTH-1?tab=wiki"
        );
        for n in [
            "//evil.test",
            "https://evil.test",
            "/\\evil.test",
            "evil",
            "/a\nb",
            "",
        ] {
            assert_eq!(safe_next(Some(n)), "/", "{n:?}");
        }
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn emails_are_normalized() {
        assert_eq!(
            normalize_email(" Bob@Example.COM ").as_deref(),
            Some("bob@example.com")
        );
        for e in ["", "bob", "@x", "bob@", "a b@x.com", "a@b@c"] {
            assert_eq!(normalize_email(e), None, "{e:?}");
        }
    }

    #[test]
    fn settings_need_both_values() {
        let vars = |id: Option<&str>, secret: Option<&str>| {
            let (id, secret) = (id.map(String::from), secret.map(String::from));
            from_vars(move |k| match k {
                "OXLEDGER_GOOGLE_CLIENT_ID" => id.clone(),
                "OXLEDGER_GOOGLE_CLIENT_SECRET" => secret.clone(),
                "OXLEDGER_GOOGLE_ALLOWED_DOMAINS" => Some("Example.com, corp.example".into()),
                _ => None,
            })
        };
        assert!(vars(None, None).unwrap().is_none());
        assert!(vars(Some("id"), None).is_err());
        let v = vars(Some("id"), Some("s")).unwrap().unwrap();
        assert_eq!(v.allowed_domains, ["example.com", "corp.example"]);
        assert!(v.setup_emails.is_empty());
    }
}
