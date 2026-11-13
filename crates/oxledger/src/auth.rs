// SPDX-License-Identifier: GPL-2.0-or-later
//! Authentication (contract 02 §3): who is calling and how.
//!
//! * People sign in with a password and get a browser session (cookie). The
//!   SPA API (`/api/v1`) accepts only that cookie.
//! * MCP accepts only bearer tokens: legacy API keys (read-only, the user's
//!   own visibility) and agent tokens (an agent acting for a delegator, within
//!   its delegations).
//!
//! An [`AuthContext`] says what authentication established. It is not a pass:
//! every request turns it into an effective identity with `db::effective`,
//! which re-reads the session, key, token, context and delegations from the
//! database (§3.7: nothing that decides access is cached across requests).

use crate::error::{ApiError, ErrorCode, api};
use anyhow::{Context as _, Result};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use std::sync::OnceLock;
use subtle::ConstantTimeEq;
use uuid::Uuid;

// ---------------------------------------------------------------- identity

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    Anonymous,
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Anonymous,
    BrowserSession,
    LegacyApiKey,
    AgentToken,
    /// The server reading on a user's behalf to decide and render their
    /// notifications (design 05 §3.11.2 step 3) and to count watchers who can
    /// see an issue (§3.11.1). Built only by
    /// [`AuthContext::notification_recipient`], never from a request; no
    /// service that writes or needs a session accepts it.
    Recipient,
    /// A tool call of the built-in AI during one run (design 05 §3.13.1,
    /// appendix D V13). Built only by [`AuthContext::builtin_run`], which
    /// the run executor calls with a run it loaded from the database; never
    /// from an HTTP request or an MCP message. `db::effective` re-checks the
    /// run, the AI settings, the client, the user, the principal and the
    /// work context on every call; it has no token and no session.
    BuiltinRun,
    /// Tests and seeding only: a user with no credential row. Does not exist
    /// in builds without the `test-support` feature.
    #[cfg(feature = "test-support")]
    TestUser,
}

/// What authentication established (§3.2). Built only here (the fields are
/// private), never from request parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthContext {
    principal_id: Option<Uuid>,
    kind: PrincipalKind,
    method: AuthMethod,
    /// BrowserSession: the session.
    session_id: Option<Uuid>,
    /// LegacyApiKey: `api_keys.id`; AgentToken: `agent_credentials.id`.
    credential_id: Option<Uuid>,
    /// AgentToken: the work context the token belongs to.
    context_id: Option<Uuid>,
    /// AgentToken: the delegator.
    on_behalf_of: Option<Uuid>,
    /// BuiltinRun: the run (`ai_runs.id`).
    run_id: Option<Uuid>,
}

impl AuthContext {
    pub const fn anonymous() -> Self {
        Self {
            principal_id: None,
            kind: PrincipalKind::Anonymous,
            method: AuthMethod::Anonymous,
            session_id: None,
            credential_id: None,
            context_id: None,
            on_behalf_of: None,
            run_id: None,
        }
    }

    pub(crate) fn browser_session(user_id: Uuid, session_id: Uuid) -> Self {
        Self {
            principal_id: Some(user_id),
            kind: PrincipalKind::User,
            method: AuthMethod::BrowserSession,
            session_id: Some(session_id),
            ..Self::anonymous()
        }
    }

    pub(crate) fn legacy_api_key(user_id: Uuid, key_id: Uuid) -> Self {
        Self {
            principal_id: Some(user_id),
            kind: PrincipalKind::User,
            method: AuthMethod::LegacyApiKey,
            credential_id: Some(key_id),
            ..Self::anonymous()
        }
    }

    pub(crate) fn agent_token(
        agent_id: Uuid,
        credential_id: Uuid,
        context_id: Uuid,
        delegator_id: Uuid,
    ) -> Self {
        Self {
            principal_id: Some(agent_id),
            kind: PrincipalKind::Agent,
            method: AuthMethod::AgentToken,
            credential_id: Some(credential_id),
            context_id: Some(context_id),
            on_behalf_of: Some(delegator_id),
            ..Self::anonymous()
        }
    }

    /// What `user_id` may read now, for notification dispatch and
    /// re-checks (see [`AuthMethod::Recipient`]). `db::effective` accepts it
    /// only for an active user.
    pub(crate) fn notification_recipient(user_id: Uuid) -> Self {
        Self {
            principal_id: Some(user_id),
            kind: PrincipalKind::User,
            method: AuthMethod::Recipient,
            ..Self::anonymous()
        }
    }

    /// The built-in AI acting in run `run_id` (see [`AuthMethod::BuiltinRun`]).
    /// Crate-private: only `ai::runner` calls it, with ids read from the
    /// run row; `db::effective` checks that they still belong together.
    pub(crate) fn builtin_run(
        agent_id: Uuid,
        context_id: Uuid,
        delegator_id: Uuid,
        run_id: Uuid,
    ) -> Self {
        Self {
            principal_id: Some(agent_id),
            kind: PrincipalKind::Agent,
            method: AuthMethod::BuiltinRun,
            context_id: Some(context_id),
            on_behalf_of: Some(delegator_id),
            run_id: Some(run_id),
            ..Self::anonymous()
        }
    }

    /// A user without credentials, for tests and `seed-demo`.
    #[cfg(feature = "test-support")]
    pub fn test_user(user_id: Uuid) -> Self {
        Self {
            principal_id: Some(user_id),
            kind: PrincipalKind::User,
            method: AuthMethod::TestUser,
            ..Self::anonymous()
        }
    }

    /// `Some(id)` → [`test_user`](Self::test_user), `None` → anonymous.
    #[cfg(feature = "test-support")]
    pub fn test_viewer(user: Option<Uuid>) -> Self {
        user.map_or(Self::anonymous(), Self::test_user)
    }

    pub fn principal_id(&self) -> Option<Uuid> {
        self.principal_id
    }
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }
    pub fn method(&self) -> AuthMethod {
        self.method
    }
    pub fn session_id(&self) -> Option<Uuid> {
        self.session_id
    }
    pub fn credential_id(&self) -> Option<Uuid> {
        self.credential_id
    }
    pub fn context_id(&self) -> Option<Uuid> {
        self.context_id
    }
    pub fn on_behalf_of(&self) -> Option<Uuid> {
        self.on_behalf_of
    }
    /// BuiltinRun: the run.
    pub fn run_id(&self) -> Option<Uuid> {
        self.run_id
    }

    /// The signed-in person, for "me" in filters. Agents have no "me": they
    /// cannot be assigned or be a user field value.
    pub fn user_id(&self) -> Option<Uuid> {
        match self.kind {
            PrincipalKind::User => self.principal_id,
            _ => None,
        }
    }

    /// Services that only a person in the browser may use (approve, apply,
    /// delegate, issue tokens) call this first, whatever route reached them
    /// (§4.2: a later route accepting another credential must not open them).
    pub fn require_human_session(&self) -> Result<(Uuid, Uuid)> {
        match (self.kind, self.method, self.principal_id, self.session_id) {
            (PrincipalKind::User, AuthMethod::BrowserSession, Some(u), Some(s)) => Ok((u, s)),
            _ => Err(api(
                ErrorCode::HumanSessionRequired,
                "only a person signed in to the web app can do this",
            )),
        }
    }
}

// ---------------------------------------------------------------- secrets

/// 32 random bytes from the thread CSPRNG (ChaCha, seeded from the OS).
pub fn random_secret() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    b
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn encode(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

pub fn decode_bytes(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).ok()
}

fn decode32(s: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(s).ok()?.try_into().ok()
}

/// Constant-time comparison of a presented token with the stored secret.
pub fn token_matches(presented: &str, secret: &[u8]) -> bool {
    match decode32(presented) {
        Some(p) => p.ct_eq(secret).into(),
        None => false,
    }
}

/// Prefix of agent tokens; anything else on the MCP bearer is a legacy key.
pub const AGENT_TOKEN_PREFIX: &str = "oxa_";

// ---------------------------------------------------------------- passwords

/// Argon2id with the crate defaults (m = 19 MiB, t = 2, p = 1, the OWASP
/// minimum). The parameters are stored in each PHC string, so changing them
/// later only affects new hashes.
fn argon2() -> Argon2<'static> {
    Argon2::default()
}

pub fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Ok(argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))?
        .to_string())
}

fn verify_password(phc: &str, password: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(h) => argon2().verify_password(password.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

/// Hash checked for unknown logins so they cost the same as wrong passwords.
fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("not a real password").expect("hash"))
}

pub const MIN_PASSWORD_CHARS: usize = 12;

pub fn check_password_policy(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(ApiError::new(
            ErrorCode::InvalidValue,
            format!("the password needs at least {MIN_PASSWORD_CHARS} characters"),
        )
        .with_details(serde_json::json!({"path": "password"}))
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone)]
pub struct SessionPolicy {
    pub idle: Duration,
    pub absolute: Duration,
    /// Step-up window for delegating and issuing tokens (§3.3).
    pub step_up: Duration,
    /// `last_seen_at` is written at most this often.
    pub touch_every: Duration,
    pub login_challenge: Duration,
}

impl Default for SessionPolicy {
    /// Contract 02 §3.3 (U5 defaults): idle 12 h, absolute 7 days.
    fn default() -> Self {
        Self {
            idle: Duration::hours(12),
            absolute: Duration::days(7),
            step_up: Duration::minutes(15),
            touch_every: Duration::minutes(5),
            login_challenge: Duration::minutes(10),
        }
    }
}

// ---------------------------------------------------------------- audit

pub async fn audit(
    conn: &mut PgConnection,
    kind: &str,
    actor: Option<Uuid>,
    subject: Option<Uuid>,
    details: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO auth_events (kind, actor_id, subject_id, details) VALUES ($1,$2,$3,$4)",
    )
    .bind(kind)
    .bind(actor)
    .bind(subject)
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- login

/// A pre-session challenge for login CSRF: `cookie` goes into a cookie,
/// `csrf_token` to the page; `POST /auth/login` must present both.
pub struct LoginChallenge {
    pub cookie: String,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
}

pub async fn new_login_challenge(pool: &PgPool, policy: &SessionPolicy) -> Result<LoginChallenge> {
    let cookie = random_secret();
    let csrf = random_secret();
    let expires_at = Utc::now() + policy.login_challenge;
    sqlx::query(
        "INSERT INTO login_challenges (token_sha256, csrf_secret, expires_at) VALUES ($1,$2,$3)",
    )
    .bind(&sha256(&cookie)[..])
    .bind(&csrf[..])
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(LoginChallenge {
        cookie: encode(&cookie),
        csrf_token: encode(&csrf),
        expires_at,
    })
}

/// A new browser session; `token` goes into the cookie and is never stored.
pub struct NewSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub token: String,
    pub csrf_token: String,
    pub absolute_expires_at: DateTime<Utc>,
}

pub struct LoginInput<'a> {
    pub challenge_cookie: Option<&'a str>,
    pub challenge_csrf: &'a str,
    pub login: &'a str,
    pub password: &'a str,
}

/// Checks the login challenge, then the password. The challenge is used up
/// whatever happens. Unknown login and wrong password both cost one Argon2
/// verification and return `invalid_credentials`.
pub async fn login(
    pool: &PgPool,
    policy: &SessionPolicy,
    input: LoginInput<'_>,
) -> Result<NewSession> {
    let csrf_failed = || api(ErrorCode::CsrfFailed, "the sign-in page expired; reload it");
    let cookie = input
        .challenge_cookie
        .and_then(decode32)
        .ok_or_else(csrf_failed)?;
    let challenge: Option<(Vec<u8>, DateTime<Utc>)> = sqlx::query_as(
        "DELETE FROM login_challenges WHERE token_sha256 = $1 RETURNING csrf_secret, expires_at",
    )
    .bind(&sha256(&cookie)[..])
    .fetch_optional(pool)
    .await?;
    match challenge {
        Some((secret, expires))
            if expires > Utc::now() && token_matches(input.challenge_csrf, &secret) => {}
        _ => return Err(csrf_failed()),
    }

    let login = input.login.trim();
    let row: Option<(Uuid, String, String, i64)> = sqlx::query_as(
        "SELECT p.id, p.status, up.password_phc, up.credential_version
         FROM principals p JOIN user_passwords up ON up.user_id = p.id
         WHERE p.kind = 'user' AND p.login = $1",
    )
    .bind(login)
    .fetch_optional(pool)
    .await?;
    let bad = || api(ErrorCode::InvalidCredentials, "wrong login or password");
    let Some((user_id, status, phc, version)) = row else {
        verify_password(dummy_phc(), input.password);
        return Err(bad());
    };
    // Verify first so a locked account costs the same as an active one.
    if !verify_password(&phc, input.password) || status != "active" {
        return Err(bad());
    }
    let mut tx = pool.begin().await?;
    let session = create_session(&mut tx, policy, user_id, version).await?;
    audit(
        &mut tx,
        "login",
        Some(user_id),
        Some(session.session_id),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(session)
}

async fn create_session(
    conn: &mut PgConnection,
    policy: &SessionPolicy,
    user_id: Uuid,
    credential_version: i64,
) -> Result<NewSession> {
    let token = random_secret();
    let csrf = random_secret();
    let now = Utc::now();
    let absolute = now + policy.absolute;
    let idle = (now + policy.idle).min(absolute);
    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO browser_sessions (user_id, token_sha256, csrf_secret, credential_version,
                                       created_at, last_seen_at, reauthenticated_at,
                                       idle_expires_at, absolute_expires_at)
         VALUES ($1,$2,$3,$4,$5,$5,$5,$6,$7) RETURNING id",
    )
    .bind(user_id)
    .bind(&sha256(&token)[..])
    .bind(&csrf[..])
    .bind(credential_version)
    .bind(now)
    .bind(idle)
    .bind(absolute)
    .fetch_one(conn)
    .await?;
    Ok(NewSession {
        session_id,
        user_id,
        token: encode(&token),
        csrf_token: encode(&csrf),
        absolute_expires_at: absolute,
    })
}

/// A valid session found from its cookie.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub ctx: AuthContext,
    pub csrf_secret: Vec<u8>,
    pub reauthenticated_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
}

/// SQL condition (alias `s` = browser_sessions, `p` = principals,
/// `up` = user_passwords) for a session that may be used right now.
pub(crate) const SESSION_VALID: &str = "s.revoked_at IS NULL AND s.idle_expires_at > now()
    AND s.absolute_expires_at > now() AND p.kind = 'user' AND p.status = 'active'
    AND up.credential_version = s.credential_version";

/// Resolves a session cookie. Slides the idle expiry, writing at most once
/// per `touch_every`. `None` for anything invalid (unknown, expired, revoked,
/// locked user, password changed since).
pub async fn session_from_cookie(
    pool: &PgPool,
    policy: &SessionPolicy,
    cookie: &str,
) -> Result<Option<SessionInfo>> {
    let Some(token) = decode32(cookie) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT s.id, s.user_id, s.csrf_secret, s.reauthenticated_at, s.absolute_expires_at, s.last_seen_at
         FROM browser_sessions s JOIN principals p ON p.id = s.user_id
         JOIN user_passwords up ON up.user_id = s.user_id
         WHERE s.token_sha256 = $1 AND {SESSION_VALID}"
    );
    type Row = (
        Uuid,
        Uuid,
        Vec<u8>,
        DateTime<Utc>,
        DateTime<Utc>,
        DateTime<Utc>,
    );
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&sha256(&token)[..])
        .fetch_optional(pool)
        .await?;
    let Some((id, user_id, csrf_secret, reauth, absolute, last_seen)) = row else {
        return Ok(None);
    };
    let now = Utc::now();
    if now - last_seen >= policy.touch_every {
        sqlx::query(
            "UPDATE browser_sessions SET last_seen_at = $2, idle_expires_at = least($3, absolute_expires_at)
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .bind(now + policy.idle)
        .execute(pool)
        .await?;
    }
    Ok(Some(SessionInfo {
        ctx: AuthContext::browser_session(user_id, id),
        csrf_secret,
        reauthenticated_at: reauth,
        absolute_expires_at: absolute,
    }))
}

/// Revokes one session. A person may revoke only their own.
pub async fn revoke_session(
    pool: &PgPool,
    ctx: &AuthContext,
    session_id: Uuid,
    reason: &str,
) -> Result<()> {
    let (user, _) = ctx.require_human_session()?;
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "UPDATE browser_sessions SET revoked_at = now(), revoked_reason = $3
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .bind(user)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(api(ErrorCode::NotFound, "no such session"));
    }
    audit(
        &mut tx,
        "session_revoked",
        Some(user),
        Some(session_id),
        serde_json::json!({"reason": reason}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Revokes every session of a user (lock, password change, admin request).
pub async fn revoke_all_sessions(
    conn: &mut PgConnection,
    user_id: Uuid,
    reason: &str,
) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE browser_sessions SET revoked_at = now(), revoked_reason = $2
         WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(reason)
    .execute(conn)
    .await?
    .rows_affected())
}

/// Administrator action (CLI): ends every session of a user, audited.
pub async fn admin_revoke_sessions(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    let mut tx = pool.begin().await?;
    lock_user_row(&mut tx, user_id).await?;
    let n = revoke_all_sessions(&mut tx, user_id, "admin").await?;
    audit(
        &mut tx,
        "sessions_revoked_by_admin",
        None,
        Some(user_id),
        serde_json::json!({"count": n}),
    )
    .await?;
    tx.commit().await?;
    Ok(n)
}

/// Step-up: the person re-enters the password; the session's
/// `reauthenticated_at` becomes now. Grants no permission by itself.
pub async fn reauthenticate(pool: &PgPool, ctx: &AuthContext, password: &str) -> Result<()> {
    let (user, session) = ctx.require_human_session()?;
    let phc: Option<String> =
        sqlx::query_scalar("SELECT password_phc FROM user_passwords WHERE user_id = $1")
            .bind(user)
            .fetch_optional(pool)
            .await?;
    let ok = match &phc {
        Some(p) => verify_password(p, password),
        None => {
            verify_password(dummy_phc(), password);
            false
        }
    };
    if !ok {
        return Err(api(ErrorCode::InvalidCredentials, "wrong password"));
    }
    sqlx::query("UPDATE browser_sessions SET reauthenticated_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(session)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fails with `reauthentication_required` unless the session stepped up
/// within the window. Locks the session row (§3.7 order: sessions after
/// principals), so the caller's transaction sees a session that is still valid.
pub async fn require_step_up(
    conn: &mut PgConnection,
    policy: &SessionPolicy,
    ctx: &AuthContext,
) -> Result<(Uuid, Uuid)> {
    let (user, session) = ctx.require_human_session()?;
    let sql = format!(
        "SELECT s.reauthenticated_at FROM browser_sessions s JOIN principals p ON p.id = s.user_id
         JOIN user_passwords up ON up.user_id = s.user_id
         WHERE s.id = $1 AND s.user_id = $2 AND {SESSION_VALID} FOR SHARE OF s"
    );
    let reauth: Option<DateTime<Utc>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(session)
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(reauth) = reauth else {
        return Err(api(
            ErrorCode::Unauthenticated,
            "the session is no longer valid",
        ));
    };
    if Utc::now() - reauth > policy.step_up {
        return Err(api(
            ErrorCode::ReauthenticationRequired,
            "enter your password again to continue",
        ));
    }
    Ok((user, session))
}

// ---------------------------------------------------------------- users (admin CLI)

/// Creates a local user with a password (§3.3: no self sign-up).
pub async fn create_user(
    pool: &PgPool,
    login: &str,
    name: &str,
    password: &str,
    is_admin: bool,
) -> Result<Uuid> {
    check_password_policy(password)?;
    let phc = hash_password(password)?;
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO principals (kind, login, name, is_admin) VALUES ('user', $1, $2, $3) RETURNING id",
    )
    .bind(login.trim())
    .bind(name)
    .bind(is_admin)
    .fetch_one(&mut *tx)
    .await
    .context("create user")?;
    sqlx::query("INSERT INTO user_passwords (user_id, password_phc) VALUES ($1, $2)")
        .bind(id)
        .bind(&phc)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        "user_created",
        None,
        Some(id),
        serde_json::json!({"login": login.trim(), "is_admin": is_admin}),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Sets a password from the admin CLI: bumps `credential_version` (all
/// sessions stop working), revokes the sessions, writes an audit event.
pub async fn set_password(pool: &PgPool, user_id: Uuid, password: &str) -> Result<()> {
    check_password_policy(password)?;
    let phc = hash_password(password)?;
    let mut tx = pool.begin().await?;
    lock_user_row(&mut tx, user_id).await?;
    sqlx::query(
        "INSERT INTO user_passwords (user_id, password_phc) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET password_phc = EXCLUDED.password_phc,
             credential_version = user_passwords.credential_version + 1, changed_at = now()",
    )
    .bind(user_id)
    .bind(&phc)
    .execute(&mut *tx)
    .await?;
    let n = revoke_all_sessions(&mut tx, user_id, "password_changed").await?;
    audit(
        &mut tx,
        "password_set",
        None,
        Some(user_id),
        serde_json::json!({"sessions_revoked": n}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Locks or unlocks a user. Locking revokes their sessions; legacy keys and
/// their agents stop working because every request checks the status.
pub async fn set_user_status(pool: &PgPool, user_id: Uuid, active: bool) -> Result<()> {
    let mut tx = pool.begin().await?;
    lock_user_row(&mut tx, user_id).await?;
    sqlx::query("UPDATE principals SET status = $2 WHERE id = $1")
        .bind(user_id)
        .bind(if active { "active" } else { "locked" })
        .execute(&mut *tx)
        .await?;
    let n = if active {
        0
    } else {
        revoke_all_sessions(&mut tx, user_id, "user_locked").await?
    };
    audit(
        &mut tx,
        if active {
            "user_unlocked"
        } else {
            "user_locked"
        },
        None,
        Some(user_id),
        serde_json::json!({"sessions_revoked": n}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn lock_user_row(conn: &mut PgConnection, user_id: Uuid) -> Result<()> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM principals WHERE id = $1 AND kind = 'user' FOR UPDATE")
            .bind(user_id)
            .fetch_optional(conn)
            .await?;
    found
        .map(|_| ())
        .ok_or_else(|| api(ErrorCode::NotFound, "no such user"))
}

pub async fn user_id_by_login(pool: &PgPool, login: &str) -> Result<Uuid> {
    sqlx::query_scalar("SELECT id FROM principals WHERE kind = 'user' AND login = $1")
        .bind(login.trim())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| api(ErrorCode::NotFound, format!("no user {login:?}")))
}

// ---------------------------------------------------------------- bearer tokens (MCP)

pub fn hash_api_key(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// MCP bearer: an agent token (`oxa_…`) or a legacy API key. `None` when it
/// is neither valid nor usable right now.
pub async fn authenticate_bearer(pool: &PgPool, raw: &str) -> Result<Option<AuthContext>> {
    if let Some(rest) = raw.strip_prefix(AGENT_TOKEN_PREFIX) {
        let Some(token) = decode32(rest) else {
            return Ok(None);
        };
        let row: Option<(Uuid, Uuid, Uuid, Uuid, bool)> = sqlx::query_as(
            "SELECT c.id, w.id, ap.agent_principal_id, ap.delegator_user_id,
                    c.last_used_at IS NULL OR c.last_used_at < now() - interval '1 minute'
             FROM agent_credentials c
             JOIN agent_work_contexts w ON w.id = c.context_id
             JOIN agent_principals ap ON ap.agent_principal_id = c.agent_principal_id
             WHERE c.token_sha256 = $1 AND c.revoked_at IS NULL AND c.expires_at > now()
               AND w.state = 'active'",
        )
        .bind(&sha256(&token)[..])
        .fetch_optional(pool)
        .await?;
        if row.is_none() {
            // A token of a closed context (T-X20): say so, unless the token
            // itself was revoked before the context closed.
            let sql = format!(
                "SELECT EXISTS (SELECT 1 FROM agent_credentials c
                                JOIN agent_work_contexts w ON w.id = c.context_id
                                WHERE c.token_sha256 = $1 AND {})",
                crate::agents::CLOSED_WITH_CONTEXT
            );
            let closed: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(&sha256(&token)[..])
                .fetch_one(pool)
                .await?;
            if closed {
                return Err(crate::agents::context_closed());
            }
        }
        if let Some((cred, .., true)) = row {
            // G14: "last used", written at most once a minute per token.
            // Best effort: a locked row is skipped (never waited for) and a
            // failed write is logged; neither fails the authentication.
            if let Err(e) = sqlx::query(
                "UPDATE agent_credentials SET last_used_at = now()
                 WHERE id = (SELECT id FROM agent_credentials
                             WHERE id = $1
                               AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')
                             FOR UPDATE SKIP LOCKED)",
            )
            .bind(cred)
            .execute(pool)
            .await
            {
                tracing::warn!("recording agent token {cred} last use: {e}");
            }
        }
        return Ok(row.map(|(cred, ctx, agent, delegator, _)| {
            AuthContext::agent_token(agent, cred, ctx, delegator)
        }));
    }
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT k.id, p.id FROM api_keys k JOIN principals p ON p.id = k.principal_id
         WHERE k.token_sha256 = $1 AND k.revoked_at IS NULL
           AND (k.expires_at IS NULL OR k.expires_at > now())
           AND p.kind = 'user' AND p.status = 'active'",
    )
    .bind(hash_api_key(raw))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(key, user)| AuthContext::legacy_api_key(user, key)))
}

/// A fresh agent token: (raw token shown once, sha256 to store).
pub fn new_agent_token() -> (String, [u8; 32]) {
    let t = random_secret();
    (format!("{AGENT_TOKEN_PREFIX}{}", encode(&t)), sha256(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip_and_policy() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(verify_password(&phc, "correct horse battery"));
        assert!(!verify_password(&phc, "correct horse batterY"));
        assert!(!verify_password("garbage", "x"));
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy("twelve chars").is_ok());
    }

    #[test]
    fn tokens_compare_in_constant_time_and_reject_garbage() {
        let s = random_secret();
        assert!(token_matches(&encode(&s), &s));
        assert!(!token_matches(&encode(&random_secret()), &s));
        assert!(!token_matches("not base64 !", &s));
        assert!(!token_matches(&encode(&s[..31]), &s));
    }

    #[test]
    fn only_browser_sessions_are_human_sessions() {
        let u = Uuid::now_v7();
        assert!(
            AuthContext::browser_session(u, Uuid::now_v7())
                .require_human_session()
                .is_ok()
        );
        for ctx in [
            AuthContext::anonymous(),
            AuthContext::legacy_api_key(u, Uuid::now_v7()),
            AuthContext::agent_token(u, Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()),
        ] {
            let e = ctx.require_human_session().unwrap_err();
            assert_eq!(
                crate::error::as_api_error(&e).unwrap().code,
                ErrorCode::HumanSessionRequired
            );
        }
    }

    #[test]
    fn agent_tokens_have_the_prefix_and_256_bits() {
        let (raw, hash) = new_agent_token();
        let body = raw.strip_prefix(AGENT_TOKEN_PREFIX).unwrap();
        assert_eq!(sha256(&decode32(body).unwrap()), hash);
    }
}
