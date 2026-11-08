-- oxledger identity (contract 02 §3: S1). SPDX-License-Identifier: GPL-2.0-or-later
--
-- Browser sessions for people, read-only legacy API keys, and agent principals that act
-- through delegations grouped in one work context per agent (§3.5, §3.8). Also the database
-- guards that keep agents out of memberships, groups, assignments and user field values (§3.6).
--
-- Not here yet: project_catalog and the security_epoch triggers (S6), delegation policies
-- (§16, S6). projects.security_epoch is added now because delegations record it.

-- ---------------------------------------------------------------- principals

ALTER TABLE principals DROP CONSTRAINT principals_kind_check;
ALTER TABLE principals ADD CONSTRAINT principals_kind_check
    CHECK (kind IN ('user', 'group', 'agent'));
-- principals_check (kind = 'user' OR (login IS NULL AND NOT is_admin)) already keeps agents
-- without a login and never admin.

-- The kind of a principal never changes: every guard below checks the kind once, at insert
-- time, and relies on it staying put (a user turned into an agent would keep its memberships).
CREATE FUNCTION principals_freeze_kind() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind IS DISTINCT FROM OLD.kind THEN
        RAISE EXCEPTION 'principal % cannot change kind (% -> %)', OLD.id, OLD.kind, NEW.kind
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER principals_freeze_kind BEFORE UPDATE OF kind ON principals
    FOR EACH ROW EXECUTE FUNCTION principals_freeze_kind();

-- Shared check: the principal `id` exists and its kind is one of `allowed`.
CREATE FUNCTION principal_kind_is(id uuid, allowed text[]) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM principals p WHERE p.id = $1 AND p.kind = ANY ($2))
$$;

-- ---------------------------------------------------------------- guards (§3.6)

CREATE FUNCTION members_principal_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.principal_id, ARRAY['user', 'group']) THEN
        RAISE EXCEPTION 'only users and groups can be project members' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER members_principal_guard BEFORE INSERT OR UPDATE OF principal_id ON members
    FOR EACH ROW EXECUTE FUNCTION members_principal_guard();

CREATE FUNCTION group_members_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.group_id, ARRAY['group']) THEN
        RAISE EXCEPTION 'group_members.group_id must be a group' USING ERRCODE = 'check_violation';
    END IF;
    IF NOT principal_kind_is(NEW.user_id, ARRAY['user']) THEN
        RAISE EXCEPTION 'only users can join a group' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER group_members_guard BEFORE INSERT OR UPDATE ON group_members
    FOR EACH ROW EXECUTE FUNCTION group_members_guard();

CREATE FUNCTION issues_assignee_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.assigned_to_id IS NOT NULL
       AND NOT principal_kind_is(NEW.assigned_to_id, ARRAY['user', 'group']) THEN
        RAISE EXCEPTION 'issues can only be assigned to users or groups' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER issues_assignee_guard BEFORE INSERT OR UPDATE OF assigned_to_id ON issues
    FOR EACH ROW EXECUTE FUNCTION issues_assignee_guard();

-- API keys (legacy, read-only) belong to users only. They also get an id, so an
-- authenticated request can name its key and be re-checked on every call (§3.7).
ALTER TABLE api_keys
    ADD COLUMN id         uuid NOT NULL DEFAULT uuidv7() UNIQUE,
    ADD COLUMN revoked_at timestamptz,
    ADD COLUMN expires_at timestamptz;

CREATE FUNCTION api_keys_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.principal_id, ARRAY['user']) THEN
        RAISE EXCEPTION 'API keys belong to users only' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER api_keys_guard BEFORE INSERT OR UPDATE OF principal_id ON api_keys
    FOR EACH ROW EXECUTE FUNCTION api_keys_guard();

-- Custom field values of type user (P6): the 0004 check only asked whether the principal
-- exists, which an agent does. Users and groups only; whether groups are allowed for a given
-- field is an application rule (org settings, S3).
CREATE OR REPLACE FUNCTION issue_cf_check() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    t  text;
    st text;
    ok boolean;
BEGIN
    SELECT ftype, index_state INTO t, st FROM field_defs WHERE id = NEW.field_id FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'unknown custom field %', NEW.field_id USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF st = 'dropping' THEN
        RAISE EXCEPTION 'custom field % is being deleted', NEW.field_id USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
    ok := CASE
        WHEN t = 'int'              THEN NEW.v_num IS NOT NULL AND NEW.v_num = trunc(NEW.v_num)
        WHEN t = 'float'            THEN NEW.v_num IS NOT NULL
        WHEN t = 'date'             THEN NEW.v_date IS NOT NULL
        WHEN t IN ('string','link') THEN NEW.v_text <> '' AND octet_length(NEW.v_text) <= 2000
        WHEN t = 'text'             THEN NEW.v_text <> ''
        WHEN t = 'list'             THEN NEW.v_ref IS NOT NULL AND EXISTS (
                                             SELECT 1 FROM field_options o WHERE o.id = NEW.v_ref AND o.field_id = NEW.field_id
                                             FOR KEY SHARE)
        WHEN t = 'user'             THEN NEW.v_ref IS NOT NULL AND principal_kind_is(NEW.v_ref, ARRAY['user', 'group'])
        WHEN t = 'bool'             THEN NEW.v_bool IS NOT NULL
        ELSE false
    END;
    -- NULL counts as a failure (0004).
    IF ok IS DISTINCT FROM TRUE THEN
        RAISE EXCEPTION 'value does not match type % of custom field %', t, NEW.field_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;

-- ---------------------------------------------------------------- people (§3.3)

-- Argon2id PHC strings. credential_version + 1 on every password change; sessions carry the
-- version they were created with and stop working when it differs.
CREATE TABLE user_passwords (
    user_id            uuid PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
    password_phc       text NOT NULL,
    credential_version bigint NOT NULL DEFAULT 1,
    changed_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE browser_sessions (
    id                  uuid PRIMARY KEY DEFAULT uuidv7(),
    user_id             uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    token_sha256        bytea NOT NULL UNIQUE CHECK (octet_length(token_sha256) = 32),
    csrf_secret         bytea NOT NULL CHECK (octet_length(csrf_secret) = 32),
    credential_version  bigint NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_seen_at        timestamptz NOT NULL DEFAULT now(),
    reauthenticated_at  timestamptz NOT NULL DEFAULT now(),
    idle_expires_at     timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    revoked_at          timestamptz,
    revoked_reason      text,
    CHECK (absolute_expires_at > created_at)
);
CREATE INDEX browser_sessions_user_idx ON browser_sessions (user_id) WHERE revoked_at IS NULL;

CREATE FUNCTION user_only_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.user_id, ARRAY['user']) THEN
        RAISE EXCEPTION '%.user_id must be a user', TG_TABLE_NAME USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER browser_sessions_user_guard BEFORE INSERT OR UPDATE OF user_id ON browser_sessions
    FOR EACH ROW EXECUTE FUNCTION user_only_guard();
CREATE TRIGGER user_passwords_user_guard BEFORE INSERT OR UPDATE OF user_id ON user_passwords
    FOR EACH ROW EXECUTE FUNCTION user_only_guard();

-- Login CSRF: a short-lived pre-session challenge (cookie + token). Deleted on use; expired
-- rows are swept by the server.
CREATE TABLE login_challenges (
    token_sha256 bytea PRIMARY KEY CHECK (octet_length(token_sha256) = 32),
    csrf_secret  bytea NOT NULL CHECK (octet_length(csrf_secret) = 32),
    expires_at   timestamptz NOT NULL
);

-- Who did what to credentials (password set by an admin, sessions revoked, delegations,
-- agent tokens). Append-only.
CREATE TABLE auth_events (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    kind       text NOT NULL,
    actor_id   uuid REFERENCES principals(id),   -- NULL: command line / system
    subject_id uuid,                             -- the principal, session, delegation, … acted on
    details    jsonb NOT NULL DEFAULT '{}',
    at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX auth_events_subject_idx ON auth_events (subject_id, at);

CREATE FUNCTION append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME USING ERRCODE = 'feature_not_supported';
END $$;
CREATE TRIGGER auth_events_append_only BEFORE UPDATE OR DELETE ON auth_events
    FOR EACH ROW EXECUTE FUNCTION append_only();

-- ---------------------------------------------------------------- agents (§3.5, §3.8)

-- Bumped by S6 triggers when anything that decides who can read the project changes.
ALTER TABLE projects ADD COLUMN security_epoch bigint NOT NULL DEFAULT 1;

-- Agent software an administrator allows (e.g. "Claude Code").
CREATE TABLE agent_clients (
    id                uuid PRIMARY KEY DEFAULT uuidv7(),
    name              text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 100),
    enabled           boolean NOT NULL DEFAULT false,
    egress_policy_ref text NOT NULL CHECK (egress_policy_ref <> ''),
    created_at        timestamptz NOT NULL DEFAULT now()
);

-- One agent principal per (delegator, client).
CREATE TABLE agent_principals (
    agent_principal_id uuid PRIMARY KEY REFERENCES principals(id),
    delegator_user_id  uuid NOT NULL REFERENCES principals(id),
    agent_client_id    uuid NOT NULL REFERENCES agent_clients(id),
    UNIQUE (delegator_user_id, agent_client_id),
    UNIQUE (agent_principal_id, delegator_user_id)
);

CREATE FUNCTION agent_principals_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.agent_principal_id, ARRAY['agent']) THEN
        RAISE EXCEPTION 'agent_principal_id must be an agent' USING ERRCODE = 'check_violation';
    END IF;
    IF NOT principal_kind_is(NEW.delegator_user_id, ARRAY['user']) THEN
        RAISE EXCEPTION 'the delegator must be a user' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_principals_guard BEFORE INSERT OR UPDATE ON agent_principals
    FOR EACH ROW EXECUTE FUNCTION agent_principals_guard();

-- The work context: everything one agent may remember. One active context per agent.
-- "Quarantined" is not a state: a context whose source projects changed epoch is detected
-- when used (S6). closed only by a person (close, or replacing it with a new context).
CREATE TABLE agent_work_contexts (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    agent_principal_id uuid NOT NULL REFERENCES agent_principals(agent_principal_id),
    state              text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'closed')),
    fresh_context_ack  boolean NOT NULL,
    created_by_session uuid NOT NULL REFERENCES browser_sessions(id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    closed_at          timestamptz,
    CHECK ((state = 'closed') = (closed_at IS NOT NULL)),
    UNIQUE (id, agent_principal_id)
);
CREATE UNIQUE INDEX agent_work_contexts_one_active ON agent_work_contexts (agent_principal_id)
    WHERE state = 'active';

CREATE TABLE delegations (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    agent_principal_id uuid NOT NULL,
    delegator_user_id  uuid NOT NULL,
    context_id         uuid NOT NULL,
    project_id         uuid NOT NULL REFERENCES projects(id),
    security_epoch     bigint NOT NULL,
    proposal_ops       text[] NOT NULL
        CHECK (proposal_ops <@ ARRAY['create_issue','update_fields','transition','add_note','duplicate_of']),
    writable_fields    text[] NOT NULL,
    max_ops            integer NOT NULL DEFAULT 10 CHECK (max_ops BETWEEN 1 AND 10),
    created_at         timestamptz NOT NULL DEFAULT now(),
    expires_at         timestamptz NOT NULL,
    revoked_at         timestamptz,
    revoked_reason     text,
    CHECK (expires_at > created_at AND expires_at <= created_at + interval '30 days'),
    -- The delegator is the agent's delegator, and the context is the agent's.
    FOREIGN KEY (agent_principal_id, delegator_user_id)
        REFERENCES agent_principals (agent_principal_id, delegator_user_id),
    FOREIGN KEY (context_id, agent_principal_id)
        REFERENCES agent_work_contexts (id, agent_principal_id)
);
CREATE UNIQUE INDEX delegations_one_active_per_project ON delegations (context_id, project_id)
    WHERE revoked_at IS NULL;

-- R: projects ever granted to the context. Monotonic: insert only.
CREATE TABLE agent_context_source_projects (
    context_id          uuid NOT NULL REFERENCES agent_work_contexts(id),
    project_id          uuid NOT NULL REFERENCES projects(id),
    security_epoch      bigint NOT NULL,
    first_delegation_id uuid NOT NULL REFERENCES delegations(id),
    granted_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (context_id, project_id)
);
CREATE TRIGGER agent_context_source_projects_insert_only BEFORE UPDATE OR DELETE ON agent_context_source_projects
    FOR EACH ROW EXECUTE FUNCTION append_only();

-- Tokens belong to the context, not to one delegation.
CREATE TABLE agent_credentials (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    context_id   uuid NOT NULL REFERENCES agent_work_contexts(id),
    token_sha256 bytea NOT NULL UNIQUE CHECK (octet_length(token_sha256) = 32),
    label        text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    revoked_at   timestamptz,
    CHECK (expires_at > created_at)
);
CREATE INDEX agent_credentials_context_idx ON agent_credentials (context_id) WHERE revoked_at IS NULL;
