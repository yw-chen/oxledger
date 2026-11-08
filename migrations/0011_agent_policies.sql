-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6 (D20): per-delegation write policies (§16.3), the agent's
-- stable handle (§16.2), and the version commands in the op lists (§17).
--
-- The delegation's proposal_ops / writable_fields stay the ceiling (§3.5).
-- The policy rows say how each command and field inside the ceiling is
-- written: direct, review (a ChangeSet), or off. A missing row is "not in
-- the delegation" (rule 1): new commands or fields are never added to an
-- existing delegation by themselves.

CREATE TYPE agent_policy_mode AS ENUM ('direct', 'review', 'off');

CREATE TABLE delegation_command_policies (
    delegation_id uuid NOT NULL REFERENCES delegations(id),
    command_kind  text NOT NULL CHECK (command_kind IN
                    ('create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
                     'create_version', 'update_version')),
    mode          agent_policy_mode NOT NULL DEFAULT 'direct',
    PRIMARY KEY (delegation_id, command_kind)
);

-- subject, description, assigned_to_id, priority_id, start_date, due_date,
-- fixed_version_id, cf:<id>. status_id is not a field policy: a status
-- change is the transition command.
CREATE TABLE delegation_field_policies (
    delegation_id uuid NOT NULL REFERENCES delegations(id),
    field_key     text NOT NULL CHECK (field_key IN
                    ('subject', 'description', 'assigned_to_id', 'priority_id', 'start_date',
                     'due_date', 'fixed_version_id') OR field_key ~ '^cf:[0-9]+$'),
    mode          agent_policy_mode NOT NULL DEFAULT 'direct',
    PRIMARY KEY (delegation_id, field_key)
);

-- Rule 4: policies change only through the parent row (FOR UPDATE, +1).
ALTER TABLE delegations ADD COLUMN policy_version bigint NOT NULL DEFAULT 1;

-- Version commands (§17.11).
ALTER TABLE delegations DROP CONSTRAINT delegations_proposal_ops_check;
ALTER TABLE delegations ADD CONSTRAINT delegations_proposal_ops_check CHECK (proposal_ops <@ ARRAY[
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version']);
ALTER TABLE changeset_version_ops DROP CONSTRAINT changeset_version_ops_kind_check;
ALTER TABLE changeset_version_ops ADD CONSTRAINT changeset_version_ops_kind_check CHECK (kind IN (
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version'));

-- Existing delegations: every command and field in their ceiling is direct
-- (the D20 default for what the delegator already chose).
INSERT INTO delegation_command_policies (delegation_id, command_kind)
SELECT d.id, op FROM delegations d, unnest(d.proposal_ops) op;
INSERT INTO delegation_field_policies (delegation_id, field_key)
SELECT d.id, f FROM delegations d, unnest(d.writable_fields) f
WHERE f <> 'status_id';
UPDATE delegations SET writable_fields = array_remove(writable_fields, 'status_id');

-- §16.2: claude-code@alice. Stable: not changed with display names.
ALTER TABLE agent_principals
    ADD COLUMN handle text,
    ADD COLUMN status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired'));
UPDATE agent_principals ap SET handle =
    trim(BOTH '-' FROM regexp_replace(lower(c.name), '[^a-z0-9]+', '-', 'g')) || '@' || u.login
FROM agent_clients c, principals u
WHERE c.id = ap.agent_client_id AND u.id = ap.delegator_user_id;
ALTER TABLE agent_principals ALTER COLUMN handle SET NOT NULL;
ALTER TABLE agent_principals ADD CONSTRAINT agent_principals_handle_key UNIQUE (handle);
