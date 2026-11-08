-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6b.6: project tabs (§18, U44).
--
-- One settings row per project, created by trigger in the creating
-- transaction (B20, same as project_schedule_settings in 0007), so the
-- first change is an UPDATE guarded by lock_version, never a racing INSERT.
-- The default tabs live in one SQL function; the trigger and the reset use
-- it, and a test compares it with project_tabs::BUILTIN.
--
-- manage_project is new (§18.4, also governs §19 repo links) and goes to
-- the Manager role. Granting it inserts role_permissions rows, which bumps
-- every project's security_epoch (0010): existing agent work contexts are
-- quarantined and must be rebuilt after the upgrade (B17, accepted by the
-- user 2026-11-08; see docs/impl/upgrade-notes.md).

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_known;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_known CHECK (permission IN (
    'view_issues', 'add_issues', 'edit_issues', 'edit_own_issues', 'add_issue_notes',
    'set_notes_private', 'view_private_notes', 'set_issues_private', 'set_own_issues_private',
    'manage_issue_relations', 'manage_versions', 'manage_project'));

CREATE FUNCTION project_tabs_default() RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
    SELECT '[{"id": "timeline", "kind": "builtin", "title": "時程", "hidden": false},
             {"id": "milestones", "kind": "builtin", "title": "里程碑", "hidden": false},
             {"id": "issues", "kind": "builtin", "title": "議題", "hidden": false},
             {"id": "dev", "kind": "builtin", "title": "開發（GitHub）", "hidden": false},
             {"id": "plan", "kind": "builtin", "title": "規劃（心智圖）", "hidden": false}]'::jsonb
$$;

CREATE TABLE project_tab_settings (
    project_id    uuid PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    tabs          jsonb NOT NULL DEFAULT project_tabs_default(),
    default_tab   text  NOT NULL DEFAULT 'timeline',
    is_default    boolean NOT NULL DEFAULT true,
    lock_version  integer NOT NULL DEFAULT 0,
    updated_by    uuid REFERENCES principals(id),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (jsonb_typeof(tabs) = 'array' AND jsonb_array_length(tabs) BETWEEN 5 AND 15)
);
INSERT INTO project_tab_settings (project_id) SELECT id FROM projects;
CREATE FUNCTION project_tab_settings_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO project_tab_settings (project_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER project_tab_settings_row AFTER INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION project_tab_settings_row();

-- Project settings history (not issue events). The kinds of §19 are here
-- already so 0016 does not touch the CHECK. redaction_id is only a column
-- until S7 adds project_setting_event_redactions (B16).
CREATE TABLE project_setting_events (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    at           timestamptz NOT NULL DEFAULT now(),
    kind         text NOT NULL CHECK (kind IN ('tabs_updated', 'tabs_reset', 'repo_linked', 'repo_unlinked',
                                               'pr_hidden', 'pr_unhidden', 'github_content_revoked')),
    actor_id     uuid NOT NULL REFERENCES principals(id),
    operation_request_id uuid NOT NULL REFERENCES operation_requests(id),
    payload      jsonb NOT NULL,
    redaction_id uuid
);
CREATE INDEX project_setting_events_idx ON project_setting_events (project_id, at DESC, id DESC);

INSERT INTO role_permissions (role_id, permission)
    SELECT id, 'manage_project' FROM roles WHERE name = 'Manager' AND builtin IS NULL
    ON CONFLICT DO NOTHING;
