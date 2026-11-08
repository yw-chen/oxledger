-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S4: command engine and human_direct (§6.6, §6.8, §6.9, §7, §8.1
-- operation_requests, §16.2, §16.5, §16.7 tables, §17.2 version history).

-- ---------------------------------------------------------------- system principal

-- seed / import events need an actor (§7.2: actor_kind = 'system'). One
-- fixed principal; it can never log in, be a member or be assigned (the
-- principal_kind_is guards accept only users and groups there).
ALTER TABLE principals DROP CONSTRAINT principals_kind_check;
ALTER TABLE principals ADD CONSTRAINT principals_kind_check
    CHECK (kind IN ('user', 'group', 'agent', 'system'));
INSERT INTO principals (id, kind, name)
    VALUES ('00000000-0000-7000-8000-000000000001', 'system', 'oxledger');

-- ---------------------------------------------------------------- search epoch (design 03 §4.3)

-- The data epoch in consistent_after tokens. Writers read it FOR SHARE (§6.7
-- step 8b); a rotation is a one-row UPDATE that waits for them.
CREATE TABLE search_settings (
    singleton  boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    data_epoch uuid NOT NULL DEFAULT uuidv7(),
    rotated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO search_settings DEFAULT VALUES;

-- ---------------------------------------------------------------- priorities (U3)

CREATE TABLE issue_priorities (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    name       text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 60),
    position   integer NOT NULL DEFAULT 0,
    active     boolean NOT NULL DEFAULT true,
    is_default boolean NOT NULL DEFAULT false,
    CHECK (active OR NOT is_default)
);
CREATE UNIQUE INDEX issue_priorities_one_default ON issue_priorities (is_default) WHERE is_default;
INSERT INTO issue_priorities (name, position, is_default) VALUES
    ('Low', 1, false), ('Normal', 2, true), ('High', 3, false), ('Urgent', 4, false), ('Immediate', 5, false);

ALTER TABLE issues ADD COLUMN priority_id uuid REFERENCES issue_priorities(id);
UPDATE issues SET priority_id = (SELECT id FROM issue_priorities WHERE is_default);
ALTER TABLE issues ALTER COLUMN priority_id SET NOT NULL;
-- Rows written without a priority (seed and test helpers) get the default.
-- The command engine always sets it explicitly from the is_default row.
CREATE FUNCTION issues_default_priority() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.priority_id IS NULL THEN
        SELECT id INTO NEW.priority_id FROM issue_priorities WHERE is_default;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER issues_default_priority BEFORE INSERT ON issues
    FOR EACH ROW EXECUTE FUNCTION issues_default_priority();

-- ---------------------------------------------------------------- idempotency (§8.1, §8.7)

CREATE TABLE operation_requests (
    id                uuid PRIMARY KEY DEFAULT uuidv7(),
    caller_id         uuid NOT NULL REFERENCES principals(id),
    operation_id      uuid NOT NULL,
    action            text NOT NULL,   -- command / version_create / … / changeset_propose (S5)
    fingerprint       bytea NOT NULL CHECK (octet_length(fingerprint) = 32),
    status            text NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed')),
    result_ref        jsonb,           -- ids, lock versions and the token; never values (§7.5)
    result_expires_at timestamptz,     -- afterwards only a tombstone: operation_result_expired
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (caller_id, operation_id)
);

-- ---------------------------------------------------------------- executions (§16.5)

CREATE TABLE execution_refs (
    id   uuid PRIMARY KEY DEFAULT uuidv7(),
    kind text NOT NULL CHECK (kind IN ('command', 'changeset')),
    UNIQUE (id, kind)
);

CREATE TABLE command_executions (
    id                   uuid PRIMARY KEY,
    ref_kind             text NOT NULL DEFAULT 'command' CHECK (ref_kind = 'command'),
    operation_request_id uuid NOT NULL REFERENCES operation_requests(id),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    on_behalf_of_id      uuid REFERENCES principals(id),
    origin               text NOT NULL CHECK (origin IN ('agent_direct', 'human_direct')),
    delegation_id        uuid REFERENCES delegations(id),
    context_id           uuid REFERENCES agent_work_contexts(id),
    policy_version       bigint,
    command_hash         bytea NOT NULL CHECK (octet_length(command_hash) = 32),
    reverts_execution_id uuid REFERENCES execution_refs(id),
    consistent_after     text NOT NULL,   -- the full token, never rebuilt later (§6.9)
    created_at           timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (id, ref_kind) REFERENCES execution_refs (id, kind),
    CHECK (origin <> 'agent_direct' OR (on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
                                        AND context_id IS NOT NULL AND policy_version IS NOT NULL)),
    CHECK (origin <> 'human_direct' OR (on_behalf_of_id IS NULL AND delegation_id IS NULL
                                        AND context_id IS NULL))
);
CREATE TRIGGER command_executions_append_only BEFORE UPDATE OR DELETE ON command_executions
    FOR EACH ROW EXECUTE FUNCTION append_only();

-- The project snapshot R of a direct agent write (§16.7, D12 derived copies).
CREATE TABLE command_execution_source_projects (
    execution_id   uuid NOT NULL REFERENCES command_executions(id),
    project_id     uuid NOT NULL REFERENCES projects(id),
    security_epoch bigint NOT NULL,
    PRIMARY KEY (execution_id, project_id)
);

-- ---------------------------------------------------------------- issues: authorship (§16.2)

ALTER TABLE issues
    ADD COLUMN last_event_seq bigint NOT NULL DEFAULT 0,
    ADD COLUMN created_on_behalf_of_id uuid REFERENCES principals(id);

-- author is an agent ⇔ created_on_behalf_of_id is set and is that agent's delegator.
CREATE FUNCTION on_behalf_of_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    delegator uuid;
BEGIN
    SELECT ap.delegator_user_id INTO delegator
    FROM agent_principals ap WHERE ap.agent_principal_id = NEW.author_id;
    IF FOUND THEN
        IF NEW.created_on_behalf_of_id IS DISTINCT FROM delegator THEN
            RAISE EXCEPTION '%: an agent author needs created_on_behalf_of_id = its delegator', TG_TABLE_NAME
                USING ERRCODE = 'check_violation';
        END IF;
    ELSIF NEW.created_on_behalf_of_id IS NOT NULL THEN
        RAISE EXCEPTION '%: created_on_behalf_of_id is only for agent authors', TG_TABLE_NAME
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER issues_on_behalf_of BEFORE INSERT OR UPDATE OF author_id, created_on_behalf_of_id ON issues
    FOR EACH ROW EXECUTE FUNCTION on_behalf_of_guard();

-- ---------------------------------------------------------------- notes and duplicates

CREATE TABLE issue_notes (
    id                      uuid PRIMARY KEY DEFAULT uuidv7(),
    issue_id                uuid NOT NULL REFERENCES issues(id),
    author_id               uuid NOT NULL REFERENCES principals(id),
    created_on_behalf_of_id uuid REFERENCES principals(id),
    body                    text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 65536),
    is_private              boolean NOT NULL DEFAULT false,
    created_at              timestamptz NOT NULL DEFAULT now(),
    redacted_at             timestamptz,      -- D12: body becomes '[已遮蔽]'
    UNIQUE (issue_id, id)
);
CREATE INDEX issue_notes_issue_idx ON issue_notes (issue_id, created_at, id);
CREATE TRIGGER issue_notes_on_behalf_of BEFORE INSERT OR UPDATE OF author_id, created_on_behalf_of_id ON issue_notes
    FOR EACH ROW EXECUTE FUNCTION on_behalf_of_guard();

-- source = the duplicate, target = the original. Each creation is a new
-- instance (undo removes only the instance it created, §16.7).
CREATE TABLE issue_duplicates (
    id                      uuid PRIMARY KEY DEFAULT uuidv7(),
    source_issue_id         uuid NOT NULL REFERENCES issues(id),
    target_issue_id         uuid NOT NULL REFERENCES issues(id),
    created_by_execution_id uuid REFERENCES execution_refs(id),
    created_at              timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT issue_duplicates_pair UNIQUE (source_issue_id, target_issue_id),
    CHECK (source_issue_id <> target_issue_id)
);
CREATE INDEX issue_duplicates_target_idx ON issue_duplicates (target_issue_id);

-- ---------------------------------------------------------------- issue events (§7.1, §16.5)

CREATE TABLE issue_events (
    id                   uuid PRIMARY KEY DEFAULT uuidv7(),
    issue_id             uuid NOT NULL REFERENCES issues(id),
    seq                  bigint NOT NULL CHECK (seq >= 1),   -- internal only
    at                   timestamptz NOT NULL DEFAULT now(),
    kind                 text NOT NULL CHECK (kind IN
                           ('created', 'fields_changed', 'status_changed', 'note_added', 'duplicate_added')),
    schema_ver           smallint NOT NULL DEFAULT 1,
    origin               text NOT NULL CHECK (origin IN
                           ('human_direct', 'agent_direct', 'external_agent', 'seed', 'import')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    actor_kind           text NOT NULL CHECK (actor_kind IN ('user', 'agent', 'system')),
    proposer_id          uuid REFERENCES principals(id),
    approver_id          uuid REFERENCES principals(id),
    on_behalf_of_id      uuid REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    agent_client_id      uuid REFERENCES agent_clients(id),
    operation_request_id uuid REFERENCES operation_requests(id),
    command_execution_id uuid REFERENCES command_executions(id),
    changeset_id         uuid,
    changeset_version    integer,
    execution_id         uuid,     -- FK added in S5 with changeset_executions
    op_id                uuid,
    system_operation     text,
    note_id              uuid,
    payload              jsonb NOT NULL,   -- replaced as a whole by a D12 redaction
    redaction_id         uuid,
    UNIQUE (issue_id, seq),
    FOREIGN KEY (issue_id, note_id) REFERENCES issue_notes (issue_id, id),
    CHECK (kind <> 'note_added' OR note_id IS NOT NULL),
    CHECK (
      (origin = 'human_direct' AND actor_kind = 'user'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND proposer_id IS NULL
         AND approver_id IS NULL AND on_behalf_of_id IS NULL AND delegation_id IS NULL
         AND system_operation IS NULL)
   OR (origin = 'agent_direct' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND approver_id IS NULL
         AND system_operation IS NULL)
   OR (origin = 'external_agent' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND changeset_id IS NOT NULL
         AND changeset_version IS NOT NULL AND execution_id IS NOT NULL AND op_id IS NOT NULL
         AND proposer_id IS NOT NULL AND approver_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND command_execution_id IS NULL AND system_operation IS NULL)
   OR (origin IN ('seed', 'import') AND actor_kind = 'system'
         AND system_operation IS NOT NULL AND changeset_id IS NULL AND execution_id IS NULL
         AND operation_request_id IS NULL AND command_execution_id IS NULL
         AND proposer_id IS NULL AND approver_id IS NULL AND on_behalf_of_id IS NULL
         AND delegation_id IS NULL)
    )
);
CREATE INDEX issue_events_issue_idx ON issue_events (issue_id, seq);
CREATE INDEX issue_events_execution_idx ON issue_events (execution_id) WHERE execution_id IS NOT NULL;
CREATE INDEX issue_events_command_idx ON issue_events (command_execution_id) WHERE command_execution_id IS NOT NULL;

CREATE TABLE issue_event_redactions (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    event_id    uuid REFERENCES issue_events(id),
    note_id     uuid REFERENCES issue_notes(id),
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    reason_code text NOT NULL,
    reason      text NOT NULL,          -- never the original text
    CHECK (num_nonnulls(event_id, note_id) = 1)
);
ALTER TABLE issue_events ADD FOREIGN KEY (redaction_id) REFERENCES issue_event_redactions(id);

-- ---------------------------------------------------------------- field origins and undo data (§16.7)

CREATE TABLE issue_field_origins (
    issue_id            uuid NOT NULL REFERENCES issues(id),
    field_key           text NOT NULL,
    origin_execution_id uuid REFERENCES execution_refs(id),   -- NULL: seed / import
    origin_event_id     uuid NOT NULL REFERENCES issue_events(id),
    PRIMARY KEY (issue_id, field_key)
);

CREATE TABLE execution_field_changes (       -- kept 30 days; a D12 derived copy
    execution_id uuid NOT NULL REFERENCES execution_refs(id),
    issue_id     uuid NOT NULL REFERENCES issues(id),
    field_key    text NOT NULL,
    before_value jsonb,
    after_value  jsonb,
    reverted_by  uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, issue_id, field_key)
);

CREATE TABLE execution_relation_changes (
    execution_id uuid NOT NULL REFERENCES execution_refs(id),
    relation_id  uuid NOT NULL,                 -- issue_duplicates.id; no FK (removed rows are gone)
    action       text NOT NULL CHECK (action IN ('created', 'removed')),
    reverted_by  uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, relation_id)
);

-- ---------------------------------------------------------------- version history (§17.2)

CREATE TABLE version_events (
    id                   uuid PRIMARY KEY DEFAULT uuidv7(),
    version_id           uuid NOT NULL REFERENCES versions(id),
    seq                  bigint NOT NULL CHECK (seq >= 1),
    at                   timestamptz NOT NULL DEFAULT now(),
    kind                 text NOT NULL CHECK (kind IN ('created', 'updated', 'deleted')),
    origin               text NOT NULL CHECK (origin IN ('human_direct', 'agent_direct', 'external_agent', 'seed', 'import')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    actor_kind           text NOT NULL CHECK (actor_kind IN ('user', 'agent', 'system')),
    on_behalf_of_id      uuid REFERENCES principals(id),
    approver_id          uuid REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    execution_id         uuid REFERENCES execution_refs(id),
    operation_request_id uuid REFERENCES operation_requests(id),
    system_operation     text,
    payload              jsonb NOT NULL,
    redaction_id         uuid,
    UNIQUE (version_id, seq),
    CHECK ((origin = 'human_direct' AND actor_kind = 'user' AND operation_request_id IS NOT NULL
              AND execution_id IS NOT NULL AND on_behalf_of_id IS NULL AND approver_id IS NULL AND delegation_id IS NULL)
        OR (origin = 'agent_direct' AND actor_kind = 'agent' AND operation_request_id IS NOT NULL AND execution_id IS NOT NULL
              AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL AND approver_id IS NULL)
        OR (origin = 'external_agent' AND actor_kind = 'agent' AND operation_request_id IS NOT NULL AND execution_id IS NOT NULL
              AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL AND approver_id IS NOT NULL)
        OR (origin IN ('seed','import') AND actor_kind = 'system' AND system_operation IS NOT NULL AND operation_request_id IS NULL))
);

CREATE TABLE version_event_redactions (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    event_id    uuid NOT NULL UNIQUE REFERENCES version_events(id),
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    reason_code text NOT NULL,
    reason      text NOT NULL
);
ALTER TABLE version_events ADD FOREIGN KEY (redaction_id) REFERENCES version_event_redactions(id);

CREATE TABLE version_field_origins (
    version_id          uuid NOT NULL REFERENCES versions(id),
    field_key           text NOT NULL CHECK (field_key IN ('name', 'description', 'effective_date', 'status')),
    origin_execution_id uuid REFERENCES execution_refs(id),
    PRIMARY KEY (version_id, field_key)
);

CREATE TABLE execution_version_changes (     -- kept 30 days; a D12 derived copy
    execution_id uuid NOT NULL REFERENCES execution_refs(id),
    version_id   uuid NOT NULL REFERENCES versions(id),
    field_key    text NOT NULL CHECK (field_key IN ('name', 'description', 'effective_date', 'status', '_created')),
    before_value jsonb,
    after_value  jsonb,
    reverted_by  uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, version_id, field_key)
);

-- A version always has a creator; the trigger keeps agent authorship honest
-- the same way as issues (§17.2: created_on_behalf_of_id = the delegator).
CREATE FUNCTION versions_on_behalf_of_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    delegator uuid;
BEGIN
    SELECT ap.delegator_user_id INTO delegator
    FROM agent_principals ap WHERE ap.agent_principal_id = NEW.created_by;
    IF FOUND AND NEW.created_on_behalf_of_id IS DISTINCT FROM delegator
       OR NOT FOUND AND NEW.created_on_behalf_of_id IS NOT NULL THEN
        RAISE EXCEPTION 'versions: created_on_behalf_of_id must be the agent''s delegator (and only for agents)'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER versions_on_behalf_of BEFORE INSERT OR UPDATE OF created_by, created_on_behalf_of_id ON versions
    FOR EACH ROW EXECUTE FUNCTION versions_on_behalf_of_guard();

-- ---------------------------------------------------------------- the 0007 date fixes become events (§17.2)

-- One fields_changed event per repaired issue (origin import, system
-- operation u39_date_fix), payload: the old values and why each was cleared.
-- 0007 already bumped lock_version; here only last_event_seq moves.
WITH fixes AS (
    SELECT f.issue_id,
           jsonb_object_agg(f.field_key, jsonb_build_object('old', f.old_value::text, 'new', NULL)) AS changes,
           jsonb_object_agg(f.field_key, f.reason) AS reasons
    FROM migration_date_fixes f GROUP BY f.issue_id
), ev AS (
    INSERT INTO issue_events (issue_id, seq, kind, origin, actor_id, actor_kind, system_operation, payload)
    SELECT x.issue_id, i.last_event_seq + 1, 'fields_changed', 'import',
           '00000000-0000-7000-8000-000000000001', 'system', 'u39_date_fix',
           jsonb_build_object('changes', x.changes, 'reasons', x.reasons)
    FROM fixes x JOIN issues i ON i.id = x.issue_id
    RETURNING id, issue_id
), origins AS (
    INSERT INTO issue_field_origins (issue_id, field_key, origin_execution_id, origin_event_id)
    SELECT f.issue_id, f.field_key, NULL, ev.id
    FROM migration_date_fixes f JOIN ev ON ev.issue_id = f.issue_id
)
UPDATE issues i SET last_event_seq = i.last_event_seq + 1
FROM ev WHERE ev.issue_id = i.id;
DROP TABLE migration_date_fixes;
