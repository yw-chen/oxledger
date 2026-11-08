-- SPDX-License-Identifier: GPL-2.0-or-later
-- S7: D12 redaction (contract 02 §7.5, §16.7, §17.13, §18.2).
--
-- B16: project_setting_events got a redaction_id column in 0015 without a
-- table to point at; this is that table, the same shape as
-- version_event_redactions (never the original text).
CREATE TABLE project_setting_event_redactions (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    event_id    uuid NOT NULL UNIQUE REFERENCES project_setting_events(id) ON DELETE CASCADE,
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    reason_code text NOT NULL,
    reason      text NOT NULL,
    -- the event was administrator-only before its payload was replaced (T-PT17)
    admin_only  boolean NOT NULL DEFAULT false
);
ALTER TABLE project_setting_events ADD FOREIGN KEY (redaction_id) REFERENCES project_setting_event_redactions(id);

-- The derived-copy scans (src/redaction.rs `derived`) run inside the
-- redaction transaction while the project row is locked: keep them on indexes.
CREATE INDEX changesets_context_project_idx ON changesets (context_project_id);
CREATE INDEX changeset_version_source_projects_project_idx ON changeset_version_source_projects (project_id);
CREATE INDEX command_execution_source_projects_project_idx ON command_execution_source_projects (project_id);
CREATE INDEX execution_field_changes_issue_idx ON execution_field_changes (issue_id);
CREATE INDEX execution_version_changes_version_idx ON execution_version_changes (version_id);
CREATE INDEX changeset_executions_version_idx ON changeset_executions (changeset_id, version);
