-- SPDX-License-Identifier: GPL-2.0-or-later
-- M2.0 (design 06 §2.2; planned there as 0031, renumbered because 0031 is
-- the P2 API gaps): permissions, text formats, mentions, the integration
-- origin and the import link of command executions.
--
-- CHECK lists are rebuilt as the union of what 0001–0031 allow plus the new
-- values (design 05 §2.1); T-M2-96 compares them with the code constants.
--
-- security_epoch does not move (design 06 §2.1, B-37): the new permissions
-- are granted with role_permissions_epoch disabled. Unlike 0023 (which
-- paused only enabled_modules_epoch and let its grants bump once on
-- purpose), these grants change no reader of existing content: none of them
-- is view_issues, no role's issues_visibility changes, and there is no wiki
-- or attachment content before this migration. The upgrade test asserts the
-- epochs.

-- ---------------------------------------------------------------- permissions

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_known;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_known CHECK (permission IN (
    'view_issues', 'add_issues', 'edit_issues', 'edit_own_issues', 'add_issue_notes',
    'set_notes_private', 'view_private_notes', 'set_issues_private', 'set_own_issues_private',
    'manage_issue_relations', 'manage_versions', 'manage_project',
    'view_time_entries', 'log_time', 'edit_time_entries', 'edit_own_time_entries',
    'log_time_for_other_users', 'triage_issues', 'manage_public_views',
    -- M2 (design 06 §2.2; no export permission: A4 (a))
    'view_wiki_pages', 'view_wiki_edits', 'edit_wiki_pages', 'rename_wiki_pages',
    'delete_wiki_pages', 'protect_wiki_pages', 'manage_wiki',
    'import_issues', 'import_time_entries'));
-- None of them is tracker-scoped: role_permissions_tracker_scope is unchanged.

ALTER TABLE role_permissions DISABLE TRIGGER role_permissions_epoch;
-- Wiki readers: roles that see issues with visibility all or default
-- (builtin Non member and Anonymous included). Roles limited to their own
-- issues get none (B-50).
INSERT INTO role_permissions (role_id, permission)
    SELECT r.id, p.perm
    FROM roles r
    CROSS JOIN (VALUES ('view_wiki_pages'), ('view_wiki_edits')) AS p(perm)
    WHERE r.issues_visibility IN ('all', 'default')
      AND EXISTS (SELECT 1 FROM role_permissions v WHERE v.role_id = r.id AND v.permission = 'view_issues')
ON CONFLICT DO NOTHING;
INSERT INTO role_permissions (role_id, permission)
    SELECT e.role_id, 'edit_wiki_pages' FROM role_permissions e WHERE e.permission = 'edit_issues'
ON CONFLICT DO NOTHING;
INSERT INTO role_permissions (role_id, permission)
    SELECT m.role_id, p.perm
    FROM role_permissions m
    CROSS JOIN (VALUES ('rename_wiki_pages'), ('delete_wiki_pages'), ('protect_wiki_pages'),
                       ('manage_wiki'), ('import_issues'), ('import_time_entries')) AS p(perm)
    WHERE m.permission = 'manage_project'
ON CONFLICT DO NOTHING;
ALTER TABLE role_permissions ENABLE TRIGGER role_permissions_epoch;

-- ---------------------------------------------------------------- text formats (R-M9, K2)

-- Added as 'plain' (existing rows keep showing as plain text; a constant
-- default does not rewrite the table), then new rows default to Markdown.
ALTER TABLE issues ADD COLUMN description_format text NOT NULL DEFAULT 'plain'
    CONSTRAINT issues_description_format_check CHECK (description_format IN ('plain', 'markdown'));
ALTER TABLE issues ALTER COLUMN description_format SET DEFAULT 'markdown';
ALTER TABLE issue_notes ADD COLUMN body_format text NOT NULL DEFAULT 'plain'
    CONSTRAINT issue_notes_body_format_check CHECK (body_format IN ('plain', 'markdown'));
ALTER TABLE issue_notes ALTER COLUMN body_format SET DEFAULT 'markdown';

-- ---------------------------------------------------------------- mentions (R-MN3)

-- Who an event mentions: user ids only, never text. Rows of a redacted
-- event or note are deleted with it (redaction::derived).
CREATE TABLE issue_event_mentions (
    event_id uuid NOT NULL REFERENCES issue_events(id),
    user_id  uuid NOT NULL REFERENCES principals(id),
    PRIMARY KEY (event_id, user_id)
);
CREATE INDEX issue_event_mentions_user_idx ON issue_event_mentions (user_id);
CREATE TRIGGER issue_event_mentions_user_guard BEFORE INSERT OR UPDATE OF user_id ON issue_event_mentions
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

-- ---------------------------------------------------------------- notifications

ALTER TABLE notifications DROP CONSTRAINT notifications_reasons_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_reasons_check CHECK (
    reasons <@ ARRAY['watching', 'assigned', 'author', 'agent_on_behalf', 'triage_decided',
                     'ai_budget', 'mentioned']::text[]
    AND cardinality(reasons) >= 1);
ALTER TABLE notification_preferences ADD COLUMN on_mentioned boolean NOT NULL DEFAULT true;

-- ---------------------------------------------------------------- integration origin (§3.8)

-- fixed_by writes (M2.6): the system principal acts for the rule's sponsor,
-- always through the command engine.
ALTER TABLE issue_events DROP CONSTRAINT issue_events_origin_check;
ALTER TABLE issue_events ADD CONSTRAINT issue_events_origin_check CHECK (origin IN (
    'human_direct', 'agent_direct', 'external_agent', 'seed', 'import', 'integration'));

-- 0008's provenance CHECK (auto-named issue_events_check1), verbatim, plus
-- the integration branch.
ALTER TABLE issue_events DROP CONSTRAINT issue_events_check1;
ALTER TABLE issue_events ADD CONSTRAINT issue_events_check1 CHECK (
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
        AND system_operation IS NOT NULL
        AND changeset_id IS NULL AND execution_id IS NULL AND operation_request_id IS NULL
        AND command_execution_id IS NULL AND proposer_id IS NULL AND approver_id IS NULL
        AND on_behalf_of_id IS NULL AND delegation_id IS NULL)
    OR (origin = 'integration' AND actor_kind = 'system'
        AND system_operation = 'fixed_by' AND on_behalf_of_id IS NOT NULL
        AND command_execution_id IS NOT NULL AND operation_request_id IS NOT NULL
        AND changeset_id IS NULL AND execution_id IS NULL AND proposer_id IS NULL
        AND approver_id IS NULL AND delegation_id IS NULL));

ALTER TABLE command_executions DROP CONSTRAINT command_executions_origin_check;
ALTER TABLE command_executions ADD CONSTRAINT command_executions_origin_check CHECK (origin IN (
    'agent_direct', 'human_direct', 'integration'));
ALTER TABLE command_executions ADD CONSTRAINT command_executions_integration_check CHECK (
    origin <> 'integration'
    OR (on_behalf_of_id IS NOT NULL AND delegation_id IS NULL AND context_id IS NULL));

-- ---------------------------------------------------------------- imports (§2.7)

-- The CSV import a command execution belongs to; the FK comes with the
-- imports table (0037).
ALTER TABLE command_executions ADD COLUMN import_id uuid;
CREATE INDEX command_executions_import_idx ON command_executions (import_id) WHERE import_id IS NOT NULL;
