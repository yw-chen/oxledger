-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.0 (§2.2): issues.estimated_hours, the issue_field_changes
-- projection ("was ever X", for ever_in) and its backfill, the new event kind.

-- A core field (as in Redmine). Commands write it as a decimal string.
ALTER TABLE issues ADD COLUMN estimated_hours numeric(7,2)
    CHECK (estimated_hours IS NULL OR estimated_hours BETWEEN 0 AND 99999.99);

ALTER TABLE workflow_field_rules DROP CONSTRAINT workflow_field_rules_field_key_check,
    ADD CONSTRAINT workflow_field_rules_field_key_check CHECK (field_key ~
      '^(subject|description|assigned_to_id|priority_id|fixed_version_id|start_date|due_date|estimated_hours|cf:[0-9]+)$');
-- A new field is never added to an existing delegation (02 §16.3 rule 1).
ALTER TABLE delegation_field_policies DROP CONSTRAINT delegation_field_policies_field_key_check,
    ADD CONSTRAINT delegation_field_policies_field_key_check CHECK (field_key IN
      ('subject', 'description', 'assigned_to_id', 'priority_id', 'start_date', 'due_date',
       'fixed_version_id', 'estimated_hours') OR field_key ~ '^cf:[0-9]+$');

-- The union of 0014's list and triage_changed (M1.3 writes it).
ALTER TABLE issue_events DROP CONSTRAINT issue_events_kind_check,
    ADD CONSTRAINT issue_events_kind_check CHECK (kind IN
      ('created', 'fields_changed', 'status_changed', 'note_added', 'duplicate_added',
       'note_retracted', 'duplicate_removed', 'triage_changed'));

-- ---------------------------------------------------------------- issue_field_changes

-- Typed projection of reference-valued core fields over issue_events. The
-- values are ids, not text; still, a redacted event loses its rows in the
-- same transaction, so a redacted change cannot be filtered on.
CREATE TABLE issue_field_changes (
    event_id  uuid NOT NULL REFERENCES issue_events(id),
    issue_id  uuid NOT NULL REFERENCES issues(id),
    field_key text NOT NULL CHECK (field_key IN
                ('status_id', 'assigned_to_id', 'fixed_version_id', 'priority_id')),
    old_ref   uuid,
    new_ref   uuid,
    at        timestamptz NOT NULL,
    PRIMARY KEY (event_id, field_key)
);
CREATE INDEX issue_field_changes_new_idx ON issue_field_changes (field_key, new_ref, issue_id);
CREATE INDEX issue_field_changes_old_idx ON issue_field_changes (field_key, old_ref, issue_id);
CREATE INDEX issue_field_changes_issue_idx ON issue_field_changes (issue_id);

-- The one mapping from an event to its projection rows, used by the backfill
-- below and by the command engine (in the statement that inserts the event):
-- created → the initial values (new_ref), fields_changed → each change,
-- status_changed → old/new. A redacted payload (a JSON string) gives none.
CREATE FUNCTION issue_field_change_rows(p_kind text, p_payload jsonb)
RETURNS TABLE (field_key text, old_ref uuid, new_ref uuid)
LANGUAGE sql IMMUTABLE AS $$
    SELECT k, NULL::uuid, (p_payload -> 'fields' ->> k)::uuid
    FROM unnest(ARRAY['status_id', 'assigned_to_id', 'fixed_version_id', 'priority_id']) AS f(k)
    WHERE p_kind = 'created'
      AND jsonb_typeof(p_payload -> 'fields') = 'object'
      AND (p_payload -> 'fields') ? k
    UNION ALL
    SELECT c.key, (c.value ->> 'old')::uuid, (c.value ->> 'new')::uuid
    FROM jsonb_each(CASE WHEN p_kind = 'fields_changed'
                          AND jsonb_typeof(p_payload -> 'changes') = 'object'
                         THEN p_payload -> 'changes' ELSE '{}'::jsonb END) AS c
    WHERE c.key IN ('status_id', 'assigned_to_id', 'fixed_version_id', 'priority_id')
    UNION ALL
    SELECT 'status_id', (p_payload ->> 'old')::uuid, (p_payload ->> 'new')::uuid
    WHERE p_kind = 'status_changed' AND jsonb_typeof(p_payload) = 'object'
$$;

-- Backfill: every event not redacted.
INSERT INTO issue_field_changes (event_id, issue_id, field_key, old_ref, new_ref, at)
SELECT e.id, e.issue_id, r.field_key, r.old_ref, r.new_ref, e.at
FROM issue_events e, issue_field_change_rows(e.kind, e.payload) r
WHERE e.redaction_id IS NULL
  AND e.kind IN ('created', 'fields_changed', 'status_changed');
