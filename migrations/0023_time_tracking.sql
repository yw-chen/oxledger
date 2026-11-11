-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 05 §2.5 (M1.4): time tracking. Activities (site level, with
-- project overrides), the timelog rules in org_settings, time_entries with
-- per-user daily totals, their history, undo copies and D12 redactions,
-- the time commands in the agent op lists, the A1(c) read_own_time switch
-- on delegations, and the M1 permissions for the built-in roles.
--
-- issues_id_project_uq already exists (0022, R25); it is not added again.
--
-- Permissions (user decision 2026-11-10): the M1 permissions go to the
-- built-in roles as in Redmine's default data, all in ONE statement, so the
-- statement-level role_permissions_epoch trigger (0010) bumps every
-- project's security_epoch exactly once. Existing agent work contexts are
-- quarantined and must be rebuilt (docs/impl/upgrade-notes.md).

-- ---------------------------------------------------------------- activities

CREATE TABLE time_entry_activities (            -- Redmine TimeEntryActivity (site level)
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    name       text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 30),
    name_key   text NOT NULL UNIQUE,              -- NFC, casefolded, trimmed (application)
    position   integer NOT NULL DEFAULT 0,
    active     boolean NOT NULL DEFAULT true,
    is_default boolean NOT NULL DEFAULT false,
    CHECK (active OR NOT is_default)
);
CREATE UNIQUE INDEX time_entry_activities_one_default ON time_entry_activities (is_default) WHERE is_default;
INSERT INTO time_entry_activities (name, name_key, position, is_default) VALUES
    ('開發', '開發', 1, true), ('設計', '設計', 2, false), ('測試', '測試', 3, false),
    ('會議', '會議', 4, false), ('其他', '其他', 5, false);

CREATE TABLE project_time_activities (           -- project override: no row = the site's active flag
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    activity_id uuid NOT NULL REFERENCES time_entry_activities(id),
    active      boolean NOT NULL,
    PRIMARY KEY (project_id, activity_id)
);

-- The project's time settings version (§3.5.3 PUT /projects/{id}/time-activities
-- takes lock_version). One row per project, created with the project.
CREATE TABLE project_time_settings (
    project_id   uuid PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    lock_version integer NOT NULL DEFAULT 0,
    updated_by   uuid REFERENCES principals(id),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
INSERT INTO project_time_settings (project_id) SELECT id FROM projects;
CREATE FUNCTION project_time_settings_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO project_time_settings (project_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER project_time_settings_row AFTER INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION project_time_settings_row();

-- ---------------------------------------------------------------- rules

ALTER TABLE org_settings
    ADD COLUMN timelog_max_hours_per_day     numeric(4,2) NOT NULL DEFAULT 0
        CHECK (timelog_max_hours_per_day BETWEEN 0 AND 24),            -- 0 = no limit
    ADD COLUMN timelog_accept_0_hours        boolean NOT NULL DEFAULT true,
    ADD COLUMN timelog_accept_future_dates   boolean NOT NULL DEFAULT true,
    ADD COLUMN timelog_accept_closed_issues  boolean NOT NULL DEFAULT true,
    ADD COLUMN timelog_require_issue         boolean NOT NULL DEFAULT false,
    ADD COLUMN timelog_require_comments      boolean NOT NULL DEFAULT false,
    ADD COLUMN timelog_lock_version          integer NOT NULL DEFAULT 0;

-- ---------------------------------------------------------------- entries

CREATE TABLE time_entries (
    id                      uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id              uuid NOT NULL REFERENCES projects(id),
    issue_id                uuid,
    user_id                 uuid NOT NULL REFERENCES principals(id),         -- whose time (a user)
    author_id               uuid NOT NULL REFERENCES principals(id),         -- who logged it (user or agent)
    created_on_behalf_of_id uuid REFERENCES principals(id),                  -- the delegator when the author is an agent
    activity_id             uuid NOT NULL REFERENCES time_entry_activities(id),
    spent_on                date NOT NULL CHECK (spent_on BETWEEN '1900-01-01' AND '2199-12-31'),
    hours                   numeric(6,2) NOT NULL CHECK (hours >= 0 AND hours <= 1000),
    comments                text NOT NULL DEFAULT '' CHECK (char_length(comments) <= 1024),
    lock_version            integer NOT NULL DEFAULT 0,
    last_event_seq          bigint NOT NULL DEFAULT 0,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    deleted_at              timestamptz,                                     -- soft delete: audit and undo
    deleted_by_execution_id uuid REFERENCES execution_refs(id),
    redacted_at             timestamptz,                                     -- D12: comments become '[已遮蔽]'
    FOREIGN KEY (issue_id, project_id) REFERENCES issues (id, project_id),   -- the issue is in the same project
    CHECK ((deleted_at IS NULL) = (deleted_by_execution_id IS NULL))
);
CREATE INDEX time_entries_project_idx ON time_entries (project_id, spent_on) WHERE deleted_at IS NULL;
CREATE INDEX time_entries_issue_idx   ON time_entries (issue_id) WHERE deleted_at IS NULL AND issue_id IS NOT NULL;
CREATE INDEX time_entries_user_idx    ON time_entries (user_id, spent_on) WHERE deleted_at IS NULL;
CREATE INDEX time_entries_list_idx    ON time_entries (spent_on DESC, created_at DESC, id) WHERE deleted_at IS NULL;

-- user_id is a user (0005); an agent author carries its delegator (0008).
CREATE TRIGGER time_entries_user_guard BEFORE INSERT OR UPDATE OF user_id ON time_entries
    FOR EACH ROW EXECUTE FUNCTION user_only_guard();
CREATE TRIGGER time_entries_on_behalf_of BEFORE INSERT OR UPDATE OF author_id, created_on_behalf_of_id ON time_entries
    FOR EACH ROW EXECUTE FUNCTION on_behalf_of_guard();

-- Outbox topic 'time_entry' (no consumer in M1; search reads topic 'issue' only).
CREATE FUNCTION time_entries_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO outbox (topic, entity_id) VALUES ('time_entry', COALESCE(NEW.id, OLD.id));
    RETURN NULL;
END $$;
CREATE TRIGGER time_entries_outbox AFTER INSERT OR UPDATE ON time_entries
    FOR EACH ROW EXECUTE FUNCTION time_entries_enqueue();

-- Per user and day: the daily maximum is serialized on this row (§3.5.5
-- T6). Kept by the command engine in the same transaction, never by hand.
CREATE TABLE time_daily_totals (
    user_id  uuid NOT NULL REFERENCES principals(id),
    spent_on date NOT NULL,
    hours    numeric(7,2) NOT NULL DEFAULT 0 CHECK (hours >= 0),
    PRIMARY KEY (user_id, spent_on)
);

-- ---------------------------------------------------------------- history (identity as version_events, 02 §17.2)

CREATE TABLE time_entry_events (
    id                   uuid PRIMARY KEY DEFAULT uuidv7(),
    time_entry_id        uuid NOT NULL REFERENCES time_entries(id),
    seq                  bigint NOT NULL CHECK (seq >= 1),
    at                   timestamptz NOT NULL DEFAULT now(),
    kind                 text NOT NULL CHECK (kind IN ('created', 'updated', 'deleted', 'restored')),
    origin               text NOT NULL CHECK (origin IN ('human_direct', 'agent_direct', 'external_agent', 'seed', 'import')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    actor_kind           text NOT NULL CHECK (actor_kind IN ('user', 'agent', 'system')),
    on_behalf_of_id      uuid REFERENCES principals(id),
    approver_id          uuid REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    execution_id         uuid REFERENCES execution_refs(id),
    operation_request_id uuid REFERENCES operation_requests(id),
    system_operation     text,
    payload              jsonb NOT NULL,     -- {"changes": {field: {"old","new"}}}
    redaction_id         uuid,
    UNIQUE (time_entry_id, seq),
    CHECK ((origin = 'human_direct' AND actor_kind = 'user' AND operation_request_id IS NOT NULL
              AND execution_id IS NOT NULL AND on_behalf_of_id IS NULL AND approver_id IS NULL AND delegation_id IS NULL)
        OR (origin = 'agent_direct' AND actor_kind = 'agent' AND operation_request_id IS NOT NULL AND execution_id IS NOT NULL
              AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL AND approver_id IS NULL)
        OR (origin = 'external_agent' AND actor_kind = 'agent' AND operation_request_id IS NOT NULL AND execution_id IS NOT NULL
              AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL AND approver_id IS NOT NULL)
        OR (origin IN ('seed','import') AND actor_kind = 'system' AND system_operation IS NOT NULL AND operation_request_id IS NULL))
);
CREATE INDEX time_entry_events_entry_idx ON time_entry_events (time_entry_id, seq);

CREATE TABLE time_entry_event_redactions (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    event_id    uuid NOT NULL UNIQUE REFERENCES time_entry_events(id),
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    reason_code text NOT NULL,
    reason      text NOT NULL                -- never the original text
);
ALTER TABLE time_entry_events ADD FOREIGN KEY (redaction_id) REFERENCES time_entry_event_redactions(id);

-- ---------------------------------------------------------------- undo (02 §16.7, U18: 30 days)

CREATE TABLE time_entry_field_origins (
    time_entry_id       uuid NOT NULL REFERENCES time_entries(id),
    field_key           text NOT NULL CHECK (field_key IN ('hours', 'spent_on', 'activity_id', 'comments', 'issue_id', 'user_id')),
    origin_execution_id uuid REFERENCES execution_refs(id),
    PRIMARY KEY (time_entry_id, field_key)
);
CREATE TABLE execution_time_entry_changes (     -- kept 30 days; a D12 derived copy
    execution_id  uuid NOT NULL REFERENCES execution_refs(id),
    time_entry_id uuid NOT NULL REFERENCES time_entries(id),
    field_key     text NOT NULL CHECK (field_key IN ('hours', 'spent_on', 'activity_id', 'comments', 'issue_id',
                                                     'user_id', '_created', '_deleted')),
    before_value  jsonb,
    after_value   jsonb,
    reverted_by   uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, time_entry_id, field_key)
);
CREATE INDEX execution_time_entry_changes_entry_idx ON execution_time_entry_changes (time_entry_id);

-- ---------------------------------------------------------------- op lists
-- Unions of every value so far (0011, 0013, 0022) plus the time commands
-- (§2.1 rule for fixed-list CHECKs; T-M1-124 compares them with the code).
-- delete_time_entry is people only and never in an agent list.

ALTER TABLE delegation_command_policies DROP CONSTRAINT delegation_command_policies_command_kind_check;
ALTER TABLE delegation_command_policies ADD CONSTRAINT delegation_command_policies_command_kind_check
    CHECK (command_kind IN (
        'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
        'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry'));
ALTER TABLE delegations DROP CONSTRAINT delegations_proposal_ops_check;
ALTER TABLE delegations ADD CONSTRAINT delegations_proposal_ops_check CHECK (proposal_ops <@ ARRAY[
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry']);
ALTER TABLE changeset_version_ops DROP CONSTRAINT changeset_version_ops_kind_check;
ALTER TABLE changeset_version_ops ADD CONSTRAINT changeset_version_ops_kind_check CHECK (kind IN (
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry'));
-- §4.3: the hourly log_time quota counts 'time_write'.
ALTER TABLE agent_write_log DROP CONSTRAINT agent_write_log_kind_check;
ALTER TABLE agent_write_log ADD CONSTRAINT agent_write_log_kind_check
    CHECK (kind IN ('direct_write', 'issue_create', 'issue_write', 'propose', 'audience_blocked',
                    'version_create', 'version_write', 'validate', 'time_write'));

-- ---------------------------------------------------------------- A1(c): read_own_time

-- Part of the delegation's read scope (not a D20 command policy). Existing
-- delegations stay false (never switched on retroactively).
ALTER TABLE delegations
    ADD COLUMN read_own_time        boolean NOT NULL DEFAULT false,
    ADD COLUMN read_own_time_ack_at timestamptz,          -- the delegator ticked "I understand" (§6.14)
    ADD CONSTRAINT delegations_read_own_time_ack CHECK (NOT read_own_time OR read_own_time_ack_at IS NOT NULL);

-- ---------------------------------------------------------------- time visibility in SQL

-- What `viewer`'s roles allow on the time entries of `project`, whether or
-- not the time_tracking module is on: 'all', 'own' or NULL. The project must
-- be active or closed; roles are the viewer's member roles (directly or
-- through a group), or the Non member role on a public project the viewer
-- is not a member of; admins read all. Writes use it to tell an entry the
-- caller could read (`module_disabled`) from one they cannot (`not_found`).
CREATE FUNCTION time_role_visibility(project uuid, viewer uuid) RETURNS text
LANGUAGE sql STABLE AS $$
    WITH p AS (
        SELECT pr.id, pr.is_public FROM projects pr
        WHERE pr.id = project AND pr.status IN ('active', 'closed')
    ), u AS (
        SELECT id, is_admin FROM principals WHERE id = viewer AND kind = 'user' AND status = 'active'
    ), mine AS (
        SELECT m.id FROM members m, p
        WHERE m.project_id = p.id
          AND (m.principal_id = viewer
               OR m.principal_id IN (SELECT group_id FROM group_members WHERE user_id = viewer))
    ), roles_of AS (
        SELECT mr.role_id FROM member_roles mr WHERE mr.member_id IN (SELECT id FROM mine)
        UNION
        SELECT r.id FROM roles r, p
        WHERE r.builtin = 'non_member' AND p.is_public AND NOT EXISTS (SELECT 1 FROM mine)
    ), vis AS (
        SELECT r.time_entries_visibility AS v FROM roles r
        JOIN role_permissions rp ON rp.role_id = r.id AND rp.permission = 'view_time_entries'
        WHERE r.id IN (SELECT role_id FROM roles_of)
    )
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM p) OR NOT EXISTS (SELECT 1 FROM u) THEN NULL
        WHEN (SELECT is_admin FROM u) THEN 'all'
        WHEN EXISTS (SELECT 1 FROM vis WHERE v = 'all') THEN 'all'
        WHEN EXISTS (SELECT 1 FROM vis) THEN 'own'
        ELSE NULL END
$$;

-- How `viewer` may read the time entries of `project` (contract 05 §3.5.4,
-- Redmine TimeEntry.visible_condition): `time_role_visibility` where the
-- time_tracking module is on, else NULL (nobody reads time there). Whether
-- the entry's issue is visible is checked separately.
CREATE FUNCTION time_visibility(project uuid, viewer uuid) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN EXISTS (SELECT 1 FROM enabled_modules m
                             WHERE m.project_id = project AND m.module = 'time_tracking')
                THEN time_role_visibility(project, viewer) END
$$;

-- ---------------------------------------------------------------- modules and permissions

-- Every existing project gets the time_tracking module (Redmine's default
-- project modules include it). The per-row module epoch trigger is held
-- off for this one insert: the permission grant below bumps every project
-- once anyway, so each project's epoch moves by exactly one.
ALTER TABLE enabled_modules DISABLE TRIGGER enabled_modules_epoch;
INSERT INTO enabled_modules (project_id, module)
    SELECT id, 'time_tracking' FROM projects ON CONFLICT DO NOTHING;
ALTER TABLE enabled_modules ENABLE TRIGGER enabled_modules_epoch;

-- Redmine lib/redmine/default_data/loader.rb: Manager gets every permission
-- (setable_permissions); Developer and Reporter get log_time and
-- view_time_entries. triage_issues and manage_public_views are oxledger's
-- and go to Manager. Roles are found as 0015 does (by name, not built-in).
-- Non member / Anonymous (Redmine: view_time_entries) get nothing here;
-- see docs/impl/m1-checklist.md M1.4-D1. One statement = one epoch bump.
INSERT INTO role_permissions (role_id, permission)
    SELECT r.id, g.permission
    FROM roles r
    JOIN (VALUES
        ('Manager', 'view_time_entries'), ('Manager', 'log_time'), ('Manager', 'edit_time_entries'),
        ('Manager', 'edit_own_time_entries'), ('Manager', 'log_time_for_other_users'),
        ('Manager', 'triage_issues'), ('Manager', 'manage_public_views'),
        ('Developer', 'view_time_entries'), ('Developer', 'log_time'),
        ('Reporter', 'view_time_entries'), ('Reporter', 'log_time')
    ) AS g(role_name, permission) ON g.role_name = r.name
    WHERE r.builtin IS NULL
    ON CONFLICT DO NOTHING;
