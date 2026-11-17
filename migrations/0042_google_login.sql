-- oxledger: Google sign-in and opening a new site with it.
-- SPDX-License-Identifier: GPL-2.0-or-later
--
-- Still one organization per site (D5) and no self sign-up (U4): a Google
-- account signs in only as a user an administrator created with the same
-- e-mail address. The one exception is a site with no user at all, where the
-- first Google sign-in names the organization and becomes its administrator.

-- A user may have no password (Google only). The row stays: it carries
-- credential_version, which every session check joins.
ALTER TABLE user_passwords ALTER COLUMN password_phc DROP NOT NULL;

-- The address an administrator gave the user; a verified Google address that
-- equals it signs in as this user. Stored lower case.
ALTER TABLE principals ADD COLUMN email text
    CHECK (email IS NULL OR (kind = 'user' AND email = lower(email) AND email LIKE '_%@_%'));
CREATE UNIQUE INDEX principals_email_key ON principals (email) WHERE email IS NOT NULL;

-- Which Google account (the `sub` claim, which never changes) is which user.
-- Written at the first sign-in; later sign-ins go by `subject`, so a changed
-- address on either side does not move the link.
CREATE TABLE user_identities (
    provider   text NOT NULL CHECK (provider IN ('google')),
    subject    text NOT NULL CHECK (subject <> ''),
    user_id    uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    email      text NOT NULL,
    linked_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, subject),
    UNIQUE (provider, user_id)
);
CREATE TRIGGER user_identities_user_guard BEFORE INSERT OR UPDATE OF user_id ON user_identities
    FOR EACH ROW EXECUTE FUNCTION user_only_guard();

-- One round trip to Google: `state` names the row, the cookie ties it to the
-- browser that started it. Deleted on use; expired rows are swept.
CREATE TABLE oidc_states (
    state_sha256  bytea PRIMARY KEY CHECK (octet_length(state_sha256) = 32),
    cookie_sha256 bytea NOT NULL CHECK (octet_length(cookie_sha256) = 32),
    nonce         text NOT NULL,
    pkce_verifier text NOT NULL,
    -- 'login', or 'reauth' for the step-up of `session_id`.
    purpose       text NOT NULL CHECK (purpose IN ('login', 'reauth')),
    session_id    uuid REFERENCES browser_sessions(id) ON DELETE CASCADE,
    next_path     text NOT NULL DEFAULT '/',
    expires_at    timestamptz NOT NULL,
    CHECK ((purpose = 'reauth') = (session_id IS NOT NULL))
);

-- A Google account that reached a site with no user: it may fill in the
-- setup form for a few minutes. Deleted on use; expired rows are swept.
CREATE TABLE setup_tickets (
    token_sha256 bytea PRIMARY KEY CHECK (octet_length(token_sha256) = 32),
    subject      text NOT NULL,
    email        text NOT NULL,
    name         text NOT NULL,
    expires_at   timestamptz NOT NULL
);

-- The organization's name, shown on the sign-in page. Empty until set.
ALTER TABLE org_settings ADD COLUMN name text NOT NULL DEFAULT ''
    CHECK (char_length(name) <= 100);
