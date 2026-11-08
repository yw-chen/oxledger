-- SPDX-License-Identifier: GPL-2.0-or-later
-- S5: ChangeSets (contract 02 §8.1–§8.3, §6.7 apply) and the issue_events FKs
-- S4 left out (§7.1, T-S7).
--
-- History lives in the append-only tables (versions, decisions, executions'
-- terminal rows, applications); changesets.state is an index (§8.2).

-- ---------------------------------------------------------------- content

-- Redactable content apart from the immutable commitment (D12, S7): the body
-- can be cleared without rewriting changeset_versions.
CREATE TABLE proposal_contents (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    body         jsonb,                              -- the normalized command document; NULL once redacted
    body_sha256  bytea NOT NULL CHECK (octet_length(body_sha256) = 32),
    state        text NOT NULL DEFAULT 'available' CHECK (state IN ('available', 'redacted', 'purged')),
    redacted_at  timestamptz,
    redacted_by  uuid REFERENCES principals(id),
    CHECK ((state = 'available') = (body IS NOT NULL))
);
-- Only redaction changes a row: available → redacted / purged, body cleared.
CREATE FUNCTION proposal_contents_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'proposal_contents rows are not deleted';
    END IF;
    IF OLD.state <> 'available' AND NEW.state = 'available' THEN
        RAISE EXCEPTION 'redacted proposal content does not come back';
    END IF;
    IF NEW.body_sha256 <> OLD.body_sha256 OR NEW.id <> OLD.id
       OR (NEW.body IS NOT NULL AND NEW.body IS DISTINCT FROM OLD.body) THEN
        RAISE EXCEPTION 'proposal content is immutable except for redaction';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER proposal_contents_guard BEFORE UPDATE OR DELETE ON proposal_contents
    FOR EACH ROW EXECUTE FUNCTION proposal_contents_guard();

-- ---------------------------------------------------------------- changesets

CREATE TABLE changesets (
    id                   uuid PRIMARY KEY DEFAULT uuidv7(),
    origin               text NOT NULL CHECK (origin IN ('external_agent', 'human', 'builtin_ai')),
    context_project_id   uuid NOT NULL REFERENCES projects(id),   -- for grouping, not an authorization boundary
    proposer_id          uuid NOT NULL REFERENCES principals(id),
    on_behalf_of_id      uuid NOT NULL REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    context_id           uuid REFERENCES agent_work_contexts(id),
    current_version      integer NOT NULL,
    state                text NOT NULL CHECK (state IN
                           ('proposed', 'approved', 'executing', 'succeeded', 'failed',
                            'rejected', 'canceled', 'expired')),
    created_at           timestamptz NOT NULL DEFAULT now(),
    expires_at           timestamptz NOT NULL,                     -- revisions do not extend it
    CHECK (origin <> 'external_agent' OR (delegation_id IS NOT NULL AND context_id IS NOT NULL)),
    CHECK (origin = 'external_agent')                              -- the only origin in the first slice
);

-- Proposal groups (§14.3): navigation only, never an approval unit, not in proposal_hash.
CREATE TABLE changeset_groups (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    delegation_id   uuid NOT NULL REFERENCES delegations(id),
    project_id      uuid NOT NULL REFERENCES projects(id),
    security_epoch  bigint NOT NULL,
    label           text CHECK (label IS NULL OR (char_length(label) <= 80 AND octet_length(label) <= 320)),
    label_state     text NOT NULL DEFAULT 'available' CHECK (label_state IN ('available', 'redacted')),
    created_at      timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE changesets ADD COLUMN group_id uuid REFERENCES changeset_groups(id);
CREATE INDEX changesets_owner_idx ON changesets (on_behalf_of_id, state, created_at DESC, id);
CREATE INDEX changesets_delegation_idx ON changesets (delegation_id, state);

CREATE TABLE changeset_versions (
    changeset_id           uuid NOT NULL REFERENCES changesets(id),
    version                integer NOT NULL CHECK (version >= 1),
    content_id             uuid NOT NULL UNIQUE REFERENCES proposal_contents(id),
    proposal_hash          bytea NOT NULL CHECK (octet_length(proposal_hash) = 32),
    command_schema_version integer NOT NULL,
    exec_mode              text NOT NULL CHECK (exec_mode = 'atomic'),
    expires_at             timestamptz NOT NULL,
    authority_ceiling      jsonb NOT NULL,
    security_epoch         bigint NOT NULL,
    editor_id              uuid NOT NULL REFERENCES principals(id),
    edit_method            text NOT NULL CHECK (edit_method IN ('agent_propose', 'agent_revise', 'owner_revise')),
    based_on_version       integer,
    validation             jsonb,              -- codes and statuses only, never values (§7.5)
    audience_contract_ver  integer NOT NULL DEFAULT 1,
    created_at             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (changeset_id, version),
    UNIQUE (changeset_id, version, proposal_hash),
    CHECK ((version = 1) = (based_on_version IS NULL)),
    CHECK (based_on_version IS NULL OR based_on_version < version)
);

-- The source snapshot of each version (§3.8 M5); insert only.
CREATE TABLE changeset_version_source_projects (
    changeset_id   uuid NOT NULL,
    version        integer NOT NULL,
    project_id     uuid NOT NULL REFERENCES projects(id),
    security_epoch bigint NOT NULL,
    PRIMARY KEY (changeset_id, version, project_id),
    FOREIGN KEY (changeset_id, version) REFERENCES changeset_versions (changeset_id, version)
);

ALTER TABLE changesets ADD CONSTRAINT changesets_current_version_fk
    FOREIGN KEY (id, current_version) REFERENCES changeset_versions (changeset_id, version)
    DEFERRABLE INITIALLY DEFERRED;

-- Lets events reference an op by FK; the content is in proposal_contents.
CREATE TABLE changeset_version_ops (
    changeset_id uuid NOT NULL,
    version      integer NOT NULL,
    op_id        uuid NOT NULL,
    position     integer NOT NULL CHECK (position >= 0),
    kind         text NOT NULL CHECK (kind IN ('create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of')),
    PRIMARY KEY (changeset_id, version, op_id),
    UNIQUE (changeset_id, version, position),
    FOREIGN KEY (changeset_id, version) REFERENCES changeset_versions (changeset_id, version)
);

-- Append-only: approve, reject. No free-text comment (T-S12, D12).
CREATE TABLE changeset_decisions (
    id            uuid PRIMARY KEY DEFAULT uuidv7(),
    changeset_id  uuid NOT NULL,
    version       integer NOT NULL,
    proposal_hash bytea NOT NULL,
    decider_id    uuid NOT NULL REFERENCES principals(id),
    decision      text NOT NULL CHECK (decision IN ('approve', 'reject')),
    session_id    uuid NOT NULL REFERENCES browser_sessions(id),
    review_mode   text NOT NULL DEFAULT 'detail' CHECK (review_mode IN ('detail', 'queue', 'revise_and_apply')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (changeset_id, version, proposal_hash)
        REFERENCES changeset_versions (changeset_id, version, proposal_hash),
    UNIQUE (id, changeset_id, version, proposal_hash, decider_id, decision)
);

CREATE TABLE changeset_executions (
    id                   uuid PRIMARY KEY,
    ref_kind             text NOT NULL DEFAULT 'changeset' CHECK (ref_kind = 'changeset'),
    changeset_id         uuid NOT NULL,
    version              integer NOT NULL,
    proposal_hash        bytea NOT NULL,
    attempt_no           integer NOT NULL CHECK (attempt_no >= 1),
    operation_request_id uuid NOT NULL UNIQUE REFERENCES operation_requests(id),
    decision_id          uuid NOT NULL,
    decision             text NOT NULL DEFAULT 'approve' CHECK (decision = 'approve'),
    executor_id          uuid NOT NULL REFERENCES principals(id),
    executor_session_id  uuid NOT NULL REFERENCES browser_sessions(id),
    status               text NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    error_code           text,
    item_results         jsonb,          -- per-op status and code, never values (§7.5)
    consistent_after     text,           -- the full token from the domain transaction; never rebuilt
    created_at           timestamptz NOT NULL DEFAULT now(),
    started_at           timestamptz,
    finished_at          timestamptz,
    UNIQUE (changeset_id, version, attempt_no),
    FOREIGN KEY (id, ref_kind) REFERENCES execution_refs (id, kind),
    -- Only an approval can be executed, and only by the person who approved.
    FOREIGN KEY (decision_id, changeset_id, version, proposal_hash, executor_id, decision)
        REFERENCES changeset_decisions (id, changeset_id, version, proposal_hash, decider_id, decision),
    CHECK ((status = 'succeeded') = (consistent_after IS NOT NULL)),
    CHECK ((status = 'failed') = (error_code IS NOT NULL)),
    CHECK ((status IN ('succeeded', 'failed')) = (finished_at IS NOT NULL))
);
-- At most one unfinished execution per version.
CREATE UNIQUE INDEX changeset_executions_active_uniq
    ON changeset_executions (changeset_id, version) WHERE status IN ('queued', 'running');

-- Execution status only moves forward: queued → running → succeeded | failed,
-- queued → failed (cancel, expiry, a failure before the domain transaction),
-- running → queued (the domain transaction rolled back for a technical reason);
-- terminal rows never change. Identity columns never change.
CREATE FUNCTION changeset_executions_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'changeset_executions rows are not deleted';
    END IF;
    IF OLD.status IN ('succeeded', 'failed') THEN
        RAISE EXCEPTION 'a finished execution does not change';
    END IF;
    IF NOT ((OLD.status = 'queued' AND NEW.status IN ('queued', 'running', 'failed'))
         OR (OLD.status = 'running' AND NEW.status IN ('running', 'succeeded', 'failed', 'queued'))) THEN
        RAISE EXCEPTION 'execution status % → % is not allowed', OLD.status, NEW.status;
    END IF;
    IF (NEW.id, NEW.changeset_id, NEW.version, NEW.proposal_hash, NEW.attempt_no,
        NEW.operation_request_id, NEW.decision_id, NEW.executor_id, NEW.executor_session_id, NEW.created_at)
       IS DISTINCT FROM
       (OLD.id, OLD.changeset_id, OLD.version, OLD.proposal_hash, OLD.attempt_no,
        OLD.operation_request_id, OLD.decision_id, OLD.executor_id, OLD.executor_session_id, OLD.created_at) THEN
        RAISE EXCEPTION 'execution identity columns are immutable';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER changeset_executions_guard BEFORE UPDATE OR DELETE ON changeset_executions
    FOR EACH ROW EXECUTE FUNCTION changeset_executions_guard();

-- Created in the same transaction as the domain effects.
CREATE TABLE changeset_applications (
    changeset_id uuid NOT NULL,
    version      integer NOT NULL,
    execution_id uuid NOT NULL UNIQUE REFERENCES changeset_executions(id),
    effect_count integer NOT NULL CHECK (effect_count >= 0),
    applied_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (changeset_id, version),       -- one success per version
    FOREIGN KEY (changeset_id, version) REFERENCES changeset_versions (changeset_id, version)
);

CREATE TRIGGER changeset_versions_append_only BEFORE UPDATE OR DELETE ON changeset_versions
    FOR EACH ROW EXECUTE FUNCTION append_only();
CREATE TRIGGER changeset_version_source_projects_append_only BEFORE UPDATE OR DELETE ON changeset_version_source_projects
    FOR EACH ROW EXECUTE FUNCTION append_only();
CREATE TRIGGER changeset_version_ops_append_only BEFORE UPDATE OR DELETE ON changeset_version_ops
    FOR EACH ROW EXECUTE FUNCTION append_only();
CREATE TRIGGER changeset_decisions_append_only BEFORE UPDATE OR DELETE ON changeset_decisions
    FOR EACH ROW EXECUTE FUNCTION append_only();
CREATE TRIGGER changeset_applications_append_only BEFORE UPDATE OR DELETE ON changeset_applications
    FOR EACH ROW EXECUTE FUNCTION append_only();

-- §8.2 transitions. A new version (current_version + 1) may come only from
-- proposed or failed and lands in proposed; succeeded, rejected, canceled and
-- expired are terminal. Identity columns never change.
CREATE FUNCTION changesets_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'changesets rows are not deleted';
    END IF;
    IF (NEW.id, NEW.origin, NEW.context_project_id, NEW.proposer_id, NEW.on_behalf_of_id,
        NEW.delegation_id, NEW.context_id, NEW.created_at, NEW.expires_at, NEW.group_id)
       IS DISTINCT FROM
       (OLD.id, OLD.origin, OLD.context_project_id, OLD.proposer_id, OLD.on_behalf_of_id,
        OLD.delegation_id, OLD.context_id, OLD.created_at, OLD.expires_at, OLD.group_id) THEN
        RAISE EXCEPTION 'changeset identity columns are immutable';
    END IF;
    IF OLD.state IN ('succeeded', 'rejected', 'canceled', 'expired') THEN
        RAISE EXCEPTION 'changeset state % is terminal', OLD.state;
    END IF;
    IF NEW.current_version <> OLD.current_version THEN
        IF NEW.current_version <> OLD.current_version + 1
           OR OLD.state NOT IN ('proposed', 'failed') OR NEW.state <> 'proposed' THEN
            RAISE EXCEPTION 'a revision goes from proposed or failed to proposed, one version up';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.state = OLD.state THEN
        RETURN NEW;
    END IF;
    IF NOT (
        (OLD.state = 'proposed'  AND NEW.state IN ('approved', 'rejected', 'canceled', 'expired'))
     OR (OLD.state = 'approved'  AND NEW.state IN ('executing', 'succeeded', 'failed', 'canceled', 'expired'))
     OR (OLD.state = 'executing' AND NEW.state IN ('succeeded', 'failed', 'approved'))
     OR (OLD.state = 'failed'    AND NEW.state IN ('approved', 'canceled', 'expired'))
    ) THEN
        RAISE EXCEPTION 'changeset state % → % is not allowed', OLD.state, NEW.state;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER changesets_guard BEFORE UPDATE OR DELETE ON changesets
    FOR EACH ROW EXECUTE FUNCTION changesets_guard();

-- ---------------------------------------------------------------- issue_events FKs (S4 left these out)

ALTER TABLE issue_events
    ADD CONSTRAINT issue_events_execution_fk
        FOREIGN KEY (execution_id) REFERENCES changeset_executions(id),
    ADD CONSTRAINT issue_events_changeset_version_fk
        FOREIGN KEY (changeset_id, changeset_version) REFERENCES changeset_versions (changeset_id, version),
    ADD CONSTRAINT issue_events_changeset_op_fk
        FOREIGN KEY (changeset_id, changeset_version, op_id)
        REFERENCES changeset_version_ops (changeset_id, version, op_id);
