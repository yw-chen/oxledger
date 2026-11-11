-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.9 (§2.11, §3.12, A3, K10): the AI team. Runtimes (where an
-- external agent actually runs, or oxledger's own built-in AI), one agent
-- principal per role, the work context (security context) bound to the
-- runtime instead of the principal, tokens bound to (context, principal),
-- principal- and project-level pauses, and the per-issue agent chain (U34).
--
-- No role, permission or membership changes and no principal is inserted,
-- deleted or re-statused: security_epoch does not move, and every existing
-- agent keeps its R, tokens, delegations and open proposals (B-38).
--
-- Differences from the §2.11 draft (docs/impl/decisions.tsv M1.9-*):
--   * agent_runtimes.lock_version (PATCH /me/agent-runtimes/{id} takes one,
--     §3.1; the draft table had no column for it);
--   * changesets.chain_review: a proposal sent for review by the chain
--     limit is not tickable in the queue and the queue names the reason
--     (§3.12.4) — the reason has to be stored somewhere;
--   * a BEFORE INSERT trigger fills delegations.runtime_id from the
--     principal when the caller leaves it out (the FKs still check it);
--   * the agent_credentials immutability trigger is new (0005 had none).

-- ---------------------------------------------------------------- runtimes

CREATE TABLE agent_runtimes (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    sponsor_user_id    uuid NOT NULL REFERENCES principals(id),
    kind               text NOT NULL CHECK (kind IN ('external', 'builtin')),
    label              text NOT NULL CHECK (char_length(label) BETWEEN 1 AND 60),
    -- "does not share files, memory or credentials with my other runtimes";
    -- builtin: guaranteed by the server.
    isolation_ack      boolean NOT NULL,
    created_by_session uuid REFERENCES browser_sessions(id),
    backfilled         boolean NOT NULL DEFAULT false,   -- appendix D V2
    lock_version       integer NOT NULL DEFAULT 1,
    created_at         timestamptz NOT NULL DEFAULT now(),
    retired_at         timestamptz,
    -- A site admin's pause of a role here (verification D3): roles created
    -- later for the same sponsor and client (in this runtime or a new one
    -- after retiring it) start paused by the site admin; only a site admin
    -- lifts it. Kept on a retired runtime on purpose.
    admin_paused_at     timestamptz,
    admin_paused_by     uuid REFERENCES principals(id),
    admin_paused_reason text CHECK (admin_paused_reason IS NULL OR char_length(admin_paused_reason) <= 500),
    CHECK ((admin_paused_at IS NULL) = (admin_paused_by IS NULL)),
    UNIQUE (sponsor_user_id, label),
    UNIQUE (id, sponsor_user_id),
    UNIQUE (id, kind),
    CHECK (kind <> 'builtin' OR isolation_ack),
    CHECK (kind = 'builtin' OR backfilled OR (isolation_ack AND created_by_session IS NOT NULL))
);
CREATE UNIQUE INDEX agent_runtimes_one_builtin ON agent_runtimes (sponsor_user_id)
    WHERE kind = 'builtin' AND retired_at IS NULL;
CREATE TRIGGER agent_runtimes_sponsor_guard BEFORE INSERT OR UPDATE OF sponsor_user_id ON agent_runtimes
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('sponsor_user_id');

-- Appendix D V11, V24: the built-in client is never offered to external
-- delegations and only goes with a builtin runtime (trigger below).
ALTER TABLE agent_clients ADD COLUMN builtin boolean NOT NULL DEFAULT false;

-- ---------------------------------------------------------------- roles

ALTER TABLE agent_principals
    ADD COLUMN runtime_id       uuid,
    ADD COLUMN role_key         text NOT NULL DEFAULT 'personal' CHECK (role_key ~ '^[a-z][a-z0-9_]{0,29}$'),
    ADD COLUMN role_template    text CHECK (role_template IN ('personal','triage','pm_assistant','design_review',
                                                              'qa','release','builtin_chat','custom')),
    ADD COLUMN template_version integer,
    -- Free text by the sponsor; only the sponsor and site administrators read it (V9).
    ADD COLUMN duties           text NOT NULL DEFAULT '' CHECK (char_length(duties) <= 500),
    ADD COLUMN created_at       timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN paused_at        timestamptz,
    ADD COLUMN paused_by        uuid REFERENCES principals(id),
    ADD COLUMN pause_scope      text CHECK (pause_scope IN ('sponsor','site_admin')),
    ADD COLUMN paused_reason    text CHECK (paused_reason IS NULL OR char_length(paused_reason) <= 500),
    -- POST /ai-team/{p}/retire, or the runtime's retirement (§3.12.5). The
    -- row stays for the audit; a retired role never comes back.
    ADD COLUMN retired_at       timestamptz,
    ADD CONSTRAINT agent_principals_pause_ck CHECK ((paused_at IS NULL) = (paused_by IS NULL)
                                                   AND (paused_at IS NULL) = (pause_scope IS NULL));

-- Backfill (appendix D V2): one external runtime per existing principal.
--   label: left(client name, 56), "-2", "-3", … on a clash within the sponsor;
--   created_by_session: the principal's latest context (any state), or NULL;
--   isolation_ack = true, backfilled = true; role personal, template v1.
-- Each principal had its own context, so every runtime's R is what the
-- principal's was (neither wider nor narrower).
DO $$
DECLARE
    r     record;
    base  text;
    cand  text;
    n     integer;
    rt    uuid;
BEGIN
    FOR r IN
        SELECT ap.agent_principal_id, ap.delegator_user_id, c.name AS client_name,
               (SELECT w.created_by_session FROM agent_work_contexts w
                 WHERE w.agent_principal_id = ap.agent_principal_id
                 ORDER BY w.created_at DESC, w.id DESC LIMIT 1) AS session
        FROM agent_principals ap JOIN agent_clients c ON c.id = ap.agent_client_id
        ORDER BY ap.delegator_user_id, ap.agent_principal_id
    LOOP
        base := left(btrim(r.client_name), 56);
        IF base = '' THEN
            base := 'agent';
        END IF;
        cand := base;
        n := 1;
        WHILE EXISTS (SELECT 1 FROM agent_runtimes
                      WHERE sponsor_user_id = r.delegator_user_id AND label = cand) LOOP
            n := n + 1;
            cand := base || '-' || n;
        END LOOP;
        INSERT INTO agent_runtimes (sponsor_user_id, kind, label, isolation_ack, created_by_session, backfilled)
        VALUES (r.delegator_user_id, 'external', cand, true, r.session, true)
        RETURNING id INTO rt;
        UPDATE agent_principals
           SET runtime_id = rt, role_key = 'personal', role_template = 'personal', template_version = 1
         WHERE agent_principal_id = r.agent_principal_id;
    END LOOP;
END $$;

-- Verification D5: 02 handles that look like the built-in AI's
-- (`oxledger-ai@…`, from a client named "oxledger-ai") are left as they are
-- (no user data is rewritten); the count is printed so the administrator
-- can find them (upgrade-notes.md has the query).
DO $$
DECLARE
    n integer;
BEGIN
    SELECT count(*) INTO n FROM agent_principals WHERE handle LIKE 'oxledger-ai@%';
    IF n > 0 THEN
        RAISE NOTICE '0029: % agent handle(s) start with oxledger-ai@ and look like the built-in AI; see upgrade-notes.md', n;
    END IF;
END $$;

ALTER TABLE agent_principals
    ALTER COLUMN runtime_id SET NOT NULL,
    ALTER COLUMN role_template SET NOT NULL,        -- appendix D V24
    ALTER COLUMN template_version SET NOT NULL,
    ADD CONSTRAINT agent_principals_runtime_fk FOREIGN KEY (runtime_id, delegator_user_id)
        REFERENCES agent_runtimes (id, sponsor_user_id),
    ADD CONSTRAINT agent_principals_runtime_role_uq UNIQUE (runtime_id, role_key),
    ADD CONSTRAINT agent_principals_id_runtime_uq UNIQUE (agent_principal_id, runtime_id);
ALTER TABLE agent_principals DROP CONSTRAINT agent_principals_delegator_user_id_agent_client_id_key;
CREATE INDEX agent_principals_delegator_idx ON agent_principals (delegator_user_id);

-- Q2 (user decision 2026-11-09): one sponsor × one client → one active
-- (not retired) runtime. Starting over with the same software is a new
-- context in that runtime or retiring it first. Also: the built-in client
-- goes with a builtin runtime and nothing else (V11), and an external
-- principal's handle never starts with oxledger-ai@ (it would look like the
-- built-in AI).
CREATE FUNCTION agent_principals_runtime_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rt_kind    text;
    cl_builtin boolean;
BEGIN
    -- Serializes the sponsor's role creation (two transactions creating the
    -- same client's role in two runtimes).
    PERFORM 1 FROM principals WHERE id = NEW.delegator_user_id FOR UPDATE;
    SELECT kind INTO rt_kind FROM agent_runtimes WHERE id = NEW.runtime_id;
    SELECT builtin INTO cl_builtin FROM agent_clients WHERE id = NEW.agent_client_id;
    IF (rt_kind = 'builtin') IS DISTINCT FROM cl_builtin THEN
        RAISE EXCEPTION 'builtin client and builtin runtime must go together' USING ERRCODE = 'check_violation';
    END IF;
    IF rt_kind = 'external' AND NEW.handle LIKE 'oxledger-ai@%' THEN
        RAISE EXCEPTION 'external agents cannot use an oxledger-ai@ handle' USING ERRCODE = 'check_violation';
    END IF;
    IF EXISTS (SELECT 1 FROM agent_principals p JOIN agent_runtimes r ON r.id = p.runtime_id
               WHERE p.delegator_user_id = NEW.delegator_user_id
                 AND p.agent_client_id = NEW.agent_client_id
                 AND p.runtime_id <> NEW.runtime_id AND r.retired_at IS NULL) THEN
        RAISE EXCEPTION 'runtime_client_taken' USING ERRCODE = 'unique_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_principals_runtime_guard
    BEFORE INSERT OR UPDATE OF runtime_id, agent_client_id, handle ON agent_principals
    FOR EACH ROW EXECUTE FUNCTION agent_principals_runtime_guard();
-- Verification D3: a new role inherits a site admin's pause of the same
-- sponsor's runtime (this one, or any runtime that had a role of the same
-- client, retired or not), so retiring a paused role or runtime and starting
-- over does not lift it. Every path that creates a role (POST /ai-team, the
-- 02 POST /delegations alias) goes through this trigger.
CREATE FUNCTION agent_principals_inherit_admin_pause() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    src record;
BEGIN
    SELECT r.id, r.admin_paused_at, r.admin_paused_by, r.admin_paused_reason INTO src
      FROM agent_runtimes r
     WHERE r.sponsor_user_id = NEW.delegator_user_id AND r.admin_paused_at IS NOT NULL
       AND (r.id = NEW.runtime_id
            OR EXISTS (SELECT 1 FROM agent_principals p
                       WHERE p.runtime_id = r.id AND p.agent_client_id = NEW.agent_client_id))
     ORDER BY r.admin_paused_at DESC LIMIT 1;
    IF FOUND THEN
        NEW.paused_at := clock_timestamp();
        NEW.paused_by := src.admin_paused_by;
        NEW.pause_scope := 'site_admin';
        NEW.paused_reason := src.admin_paused_reason;
        IF src.id <> NEW.runtime_id THEN
            UPDATE agent_runtimes
               SET admin_paused_at = src.admin_paused_at, admin_paused_by = src.admin_paused_by,
                   admin_paused_reason = src.admin_paused_reason
             WHERE id = NEW.runtime_id AND admin_paused_at IS NULL;
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_principals_inherit_admin_pause BEFORE INSERT ON agent_principals
    FOR EACH ROW EXECUTE FUNCTION agent_principals_inherit_admin_pause();
-- A retired runtime keeps its principals (audit) and stops blocking the
-- client; a runtime is never un-retired.
CREATE FUNCTION agent_runtimes_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.retired_at IS NOT NULL AND NEW.retired_at IS DISTINCT FROM OLD.retired_at THEN
        RAISE EXCEPTION 'a retired runtime stays retired' USING ERRCODE = 'check_violation';
    END IF;
    IF (NEW.id, NEW.sponsor_user_id, NEW.kind, NEW.created_at, NEW.backfilled)
       IS DISTINCT FROM (OLD.id, OLD.sponsor_user_id, OLD.kind, OLD.created_at, OLD.backfilled) THEN
        RAISE EXCEPTION 'runtime identity columns are immutable' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_runtimes_guard BEFORE UPDATE ON agent_runtimes
    FOR EACH ROW EXECUTE FUNCTION agent_runtimes_guard();

-- ---------------------------------------------------------------- contexts bound to the runtime

ALTER TABLE agent_work_contexts ADD COLUMN runtime_id uuid REFERENCES agent_runtimes(id);
UPDATE agent_work_contexts c SET runtime_id = p.runtime_id
    FROM agent_principals p WHERE p.agent_principal_id = c.agent_principal_id;
ALTER TABLE delegations DROP CONSTRAINT delegations_context_id_agent_principal_id_fkey;
ALTER TABLE agent_work_contexts
    ALTER COLUMN runtime_id SET NOT NULL,
    ADD CONSTRAINT agent_work_contexts_id_runtime_uq UNIQUE (id, runtime_id);
ALTER TABLE agent_work_contexts RENAME COLUMN agent_principal_id TO created_for_principal_id;   -- audit only
ALTER TABLE agent_work_contexts ALTER COLUMN created_for_principal_id DROP NOT NULL;
DROP INDEX agent_work_contexts_one_active;
CREATE UNIQUE INDEX agent_work_contexts_one_active ON agent_work_contexts (runtime_id) WHERE state = 'active';

-- ---------------------------------------------------------------- delegations

ALTER TABLE delegations ADD COLUMN runtime_id uuid;
UPDATE delegations d SET runtime_id = p.runtime_id
    FROM agent_principals p WHERE p.agent_principal_id = d.agent_principal_id;
ALTER TABLE delegations
    ALTER COLUMN runtime_id SET NOT NULL,
    ADD CONSTRAINT delegations_principal_runtime_fk FOREIGN KEY (agent_principal_id, runtime_id)
        REFERENCES agent_principals (agent_principal_id, runtime_id),
    ADD CONSTRAINT delegations_context_runtime_fk FOREIGN KEY (context_id, runtime_id)
        REFERENCES agent_work_contexts (id, runtime_id),
    -- A project manager's "pause in this project" (§3.12.5); the reason is
    -- required by the API (V25).
    ADD COLUMN paused_at     timestamptz,
    ADD COLUMN paused_by     uuid REFERENCES principals(id),
    ADD COLUMN paused_reason text CHECK (paused_reason IS NULL OR char_length(paused_reason) BETWEEN 1 AND 500),
    ADD CONSTRAINT delegations_pause_ck CHECK ((paused_at IS NULL) = (paused_by IS NULL)
                                              AND (paused_at IS NULL) = (paused_reason IS NULL));
DROP INDEX delegations_one_active_per_project;
-- Two roles of one runtime may each be delegated the same project.
CREATE UNIQUE INDEX delegations_one_active_per_project ON delegations (context_id, agent_principal_id, project_id)
    WHERE revoked_at IS NULL;

CREATE FUNCTION delegations_fill_runtime() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.runtime_id IS NULL THEN
        SELECT runtime_id INTO NEW.runtime_id FROM agent_principals
         WHERE agent_principal_id = NEW.agent_principal_id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER delegations_fill_runtime BEFORE INSERT ON delegations
    FOR EACH ROW EXECUTE FUNCTION delegations_fill_runtime();

-- ---------------------------------------------------------------- tokens bound to (context, principal)

-- Appendix D V24: composite FKs instead of a trigger. The last FK keeps a
-- builtin runtime from ever holding a token (V11): the built-in AI runs as
-- AuthMethod::BuiltinRun (M1.10), not through agent_credentials.
ALTER TABLE agent_credentials ADD COLUMN agent_principal_id uuid, ADD COLUMN runtime_id uuid;
UPDATE agent_credentials t SET agent_principal_id = c.created_for_principal_id, runtime_id = c.runtime_id
    FROM agent_work_contexts c WHERE c.id = t.context_id;
ALTER TABLE agent_credentials
    ALTER COLUMN agent_principal_id SET NOT NULL,
    ALTER COLUMN runtime_id SET NOT NULL,
    ADD CONSTRAINT agent_credentials_principal_runtime_fk FOREIGN KEY (agent_principal_id, runtime_id)
        REFERENCES agent_principals (agent_principal_id, runtime_id),
    ADD CONSTRAINT agent_credentials_context_runtime_fk FOREIGN KEY (context_id, runtime_id)
        REFERENCES agent_work_contexts (id, runtime_id),
    ADD COLUMN runtime_kind text NOT NULL DEFAULT 'external' CHECK (runtime_kind = 'external'),
    ADD CONSTRAINT agent_credentials_runtime_kind_fk FOREIGN KEY (runtime_id, runtime_kind)
        REFERENCES agent_runtimes (id, kind);
CREATE INDEX agent_credentials_principal_idx ON agent_credentials (agent_principal_id) WHERE revoked_at IS NULL;

CREATE FUNCTION agent_credentials_freeze() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.context_id, NEW.agent_principal_id, NEW.runtime_id, NEW.runtime_kind, NEW.token_sha256)
       IS DISTINCT FROM
       (OLD.context_id, OLD.agent_principal_id, OLD.runtime_id, OLD.runtime_kind, OLD.token_sha256) THEN
        RAISE EXCEPTION 'a token''s context, principal and runtime never change' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_credentials_freeze BEFORE UPDATE ON agent_credentials
    FOR EACH ROW EXECUTE FUNCTION agent_credentials_freeze();

-- ---------------------------------------------------------------- U34: the agent chain

-- Agent writes to one issue with no person in between (§3.12.4). The row is
-- deleted when a person intervenes: a human_direct write (an undo too) or
-- the apply of an approved proposal (external_agent events, P4); the next
-- agent write starts a new chain.
CREATE TABLE issue_agent_chains (
    issue_id                uuid PRIMARY KEY REFERENCES issues(id),
    started_at              timestamptz NOT NULL,
    started_by_execution_id uuid NOT NULL REFERENCES execution_refs(id),
    writes                  integer NOT NULL CHECK (writes >= 1),
    depth                   integer NOT NULL CHECK (depth >= 0),
    last_agent_principal_id uuid NOT NULL REFERENCES principals(id),
    agents                  uuid[] NOT NULL,          -- display only, at most 20
    updated_at              timestamptz NOT NULL
);

CREATE FUNCTION issue_agent_chains_reset() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM issue_agent_chains WHERE issue_id = NEW.issue_id;
    RETURN NULL;
END $$;
CREATE TRIGGER issue_agent_chains_reset AFTER INSERT ON issue_events
    FOR EACH ROW WHEN (NEW.origin IN ('human_direct', 'external_agent'))
    EXECUTE FUNCTION issue_agent_chains_reset();

ALTER TABLE command_executions
    ADD COLUMN agent_chain_writes integer,     -- the chain after the write (audit; NULL for people)
    ADD COLUMN agent_chain_depth  integer;
ALTER TABLE changeset_executions
    ADD COLUMN agent_chain_writes integer,
    ADD COLUMN agent_chain_depth  integer;
-- 'depth' or 'writes' when the chain limit sent the document for review.
ALTER TABLE changesets ADD COLUMN chain_review text CHECK (chain_review IN ('depth', 'writes'));

-- ---------------------------------------------------------------- agent_write_log kinds

-- The union of every value so far (0012, 0013, 0023) plus chain_review and,
-- for M1.10's built-in AI, time_taint_review (§2.1 rule; T-M1-124).
ALTER TABLE agent_write_log DROP CONSTRAINT agent_write_log_kind_check;
ALTER TABLE agent_write_log ADD CONSTRAINT agent_write_log_kind_check
    CHECK (kind IN ('direct_write', 'issue_create', 'issue_write', 'propose', 'audience_blocked',
                    'version_create', 'version_write', 'validate', 'time_write',
                    'chain_review', 'time_taint_review'));
-- agent_context_write_controls (0012) is unchanged: keyed by context, so a
-- breaker on the shared context stops every role of the runtime.
