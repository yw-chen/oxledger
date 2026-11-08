-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6b.4: undo of agent writes (§16.7).
--
-- An add_note is undone by retracting it (B4): readers see that it was
-- retracted and by whom, the body is hidden, the events stay. Retraction is
-- not a privacy change, so no epoch bump (§3.8 L362). A duplicate relation
-- is undone by removing that very instance (duplicate_removed).

ALTER TABLE issue_notes
    ADD COLUMN retracted_at timestamptz,
    ADD COLUMN retracted_by_execution_id uuid REFERENCES execution_refs(id),
    ADD CONSTRAINT issue_notes_retracted_pair
        CHECK ((retracted_at IS NULL) = (retracted_by_execution_id IS NULL));

ALTER TABLE issue_events DROP CONSTRAINT issue_events_kind_check;
ALTER TABLE issue_events ADD CONSTRAINT issue_events_kind_check
    CHECK (kind IN ('created', 'fields_changed', 'status_changed', 'note_added', 'duplicate_added',
                    'note_retracted', 'duplicate_removed'));

CREATE INDEX command_executions_reverts_idx ON command_executions (reverts_execution_id)
    WHERE reverts_execution_id IS NOT NULL;
CREATE INDEX command_executions_behalf_idx ON command_executions (on_behalf_of_id, created_at)
    WHERE on_behalf_of_id IS NOT NULL;
