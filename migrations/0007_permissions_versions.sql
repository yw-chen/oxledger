-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S3: permissions, workflow rules, assignees, versions (§6.6, §17.2).

-- Organization settings (D5: single organization, one row).
-- spa_origin: NULL = use the server's OXLEDGER_SPA_ORIGIN (the setting still
-- comes from the environment in the first slice).
CREATE TABLE org_settings (
    singleton               boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    issue_group_assignment  boolean NOT NULL DEFAULT false,
    spa_origin              text,
    timezone                text NOT NULL DEFAULT 'Asia/Taipei',   -- IANA name, validated by the application (U42)
    health_at_risk_days     integer DEFAULT 7  CHECK (health_at_risk_days BETWEEN 0 AND 90),
    health_at_risk_open_pct integer DEFAULT 50 CHECK (health_at_risk_open_pct BETWEEN 0 AND 100),
    schedule_lock_version   integer NOT NULL DEFAULT 0,
    -- describe's schema_version / workflow_version (§4.7): hints that settings
    -- changed, bumped by the triggers below. Never an authorization ticket.
    schema_version          bigint NOT NULL DEFAULT 1,
    workflow_version        bigint NOT NULL DEFAULT 1
);
INSERT INTO org_settings DEFAULT VALUES;

CREATE FUNCTION bump_schema_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE org_settings SET schema_version = schema_version + 1;
    RETURN NULL;
END $$;
CREATE FUNCTION bump_workflow_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE org_settings SET workflow_version = workflow_version + 1;
    RETURN NULL;
END $$;

-- Permission names (Redmine names; manage_versions added for U39). A typo in
-- a role definition fails here instead of silently granting nothing.
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_known CHECK (permission IN (
    'view_issues', 'add_issues', 'edit_issues', 'edit_own_issues', 'add_issue_notes',
    'set_notes_private', 'view_private_notes', 'set_issues_private', 'set_own_issues_private',
    'manage_issue_relations', 'manage_versions'));

ALTER TABLE roles ADD COLUMN assignable boolean NOT NULL DEFAULT true;

-- create_issue without is_private uses this (§6.1).
ALTER TABLE trackers ADD COLUMN private_by_default boolean NOT NULL DEFAULT false;

-- Redmine's author / assignee transition conditions; the condition is part of
-- the key, so one transition may have both an always and an author row.
ALTER TABLE workflow_transitions ADD COLUMN condition text NOT NULL DEFAULT 'always'
    CHECK (condition IN ('always', 'author', 'assignee'));
ALTER TABLE workflow_transitions DROP CONSTRAINT workflow_transitions_pkey;
ALTER TABLE workflow_transitions ADD PRIMARY KEY (tracker_id, role_id, old_status_id, new_status_id, condition);

-- Workflow field rules (Redmine WorkflowPermission).
CREATE TABLE workflow_field_rules (
    tracker_id    uuid NOT NULL REFERENCES trackers(id) ON DELETE CASCADE,
    role_id       uuid NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    old_status_id uuid NOT NULL REFERENCES issue_statuses(id) ON DELETE CASCADE,
    field_key     text NOT NULL CONSTRAINT workflow_field_rules_field_key_check CHECK (field_key ~
      '^(subject|description|assigned_to_id|priority_id|fixed_version_id|start_date|due_date|cf:[0-9]+)$'),
    rule          text NOT NULL CHECK (rule IN ('readonly', 'required')),
    PRIMARY KEY (tracker_id, role_id, old_status_id, field_key)
);

-- Versions (milestones, §17.2).
CREATE TABLE versions (
    id             uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id     uuid NOT NULL REFERENCES projects(id),
    name           text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    name_key       text NOT NULL,                 -- NFC, casefolded, trimmed; computed by the application
    description    text NOT NULL DEFAULT '' CHECK (char_length(description) <= 255),
    effective_date date,
    status         text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'locked', 'closed')),
    sharing        text NOT NULL DEFAULT 'none' CHECK (sharing = 'none'),
    lock_version   integer NOT NULL DEFAULT 0,
    last_event_seq bigint NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz,
    created_by     uuid NOT NULL REFERENCES principals(id),
    created_on_behalf_of_id uuid REFERENCES principals(id),
    UNIQUE (id, project_id),
    CHECK (effective_date IS NULL OR effective_date BETWEEN '1900-01-01' AND '2199-12-31')
);
CREATE UNIQUE INDEX versions_name_uq ON versions (project_id, name_key) WHERE deleted_at IS NULL;
CREATE INDEX versions_project_idx ON versions (project_id, effective_date) WHERE deleted_at IS NULL;

-- Health thresholds (U43): organization default, project override.
CREATE TABLE project_schedule_settings (
    project_id              uuid PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    inherit_org             boolean NOT NULL DEFAULT true,
    health_at_risk_days     integer CHECK (health_at_risk_days BETWEEN 0 AND 90),
    health_at_risk_open_pct integer CHECK (health_at_risk_open_pct BETWEEN 0 AND 100),
    lock_version            integer NOT NULL DEFAULT 0,
    updated_by              uuid REFERENCES principals(id),
    updated_at              timestamptz NOT NULL DEFAULT now()
);
-- Every project has a row: existing ones now, new ones in the creating
-- transaction (trigger), so the first override is an UPDATE guarded by
-- lock_version, never a racing INSERT.
INSERT INTO project_schedule_settings (project_id) SELECT id FROM projects;
CREATE FUNCTION project_schedule_settings_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO project_schedule_settings (project_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER projects_schedule_settings AFTER INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION project_schedule_settings_row();

-- Issue dates that break the new CHECKs, fixed below before VALIDATE (§17.2
-- "既有日期的修復"). The contract writes each fix as a fields_changed event;
-- issue_events arrives in S4, whose migration turns these rows into events.
CREATE TABLE migration_date_fixes (
    issue_id   uuid NOT NULL REFERENCES issues(id),
    field_key  text NOT NULL CHECK (field_key IN ('start_date', 'due_date')),
    old_value  date NOT NULL,
    reason     text NOT NULL CHECK (reason IN ('out_of_range', 'date_order')),
    fixed_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issue_id, field_key)
);

ALTER TABLE issues
    ADD COLUMN fixed_version_id uuid,
    ADD CONSTRAINT issues_version_same_project
        FOREIGN KEY (fixed_version_id, project_id) REFERENCES versions (id, project_id),
    ADD CONSTRAINT issues_date_order CHECK (start_date IS NULL OR due_date IS NULL OR start_date <= due_date) NOT VALID,
    ADD CONSTRAINT issues_date_range CHECK (
        (start_date IS NULL OR start_date BETWEEN '1900-01-01' AND '2199-12-31') AND
        (due_date   IS NULL OR due_date   BETWEEN '1900-01-01' AND '2199-12-31')) NOT VALID;

-- The fix, reproducible and without guessing intent: (1) out-of-range dates
-- become NULL; (2) if both remain and start > due, both become NULL. One
-- UPDATE per issue, lock_version + 1 (the outbox trigger re-indexes it).
WITH bad AS (
    SELECT id, start_date, due_date,
           start_date IS NOT NULL AND start_date NOT BETWEEN '1900-01-01' AND '2199-12-31' AS start_out,
           due_date   IS NOT NULL AND due_date   NOT BETWEEN '1900-01-01' AND '2199-12-31' AS due_out
    FROM issues
), plan AS (
    SELECT id, start_date, due_date, start_out, due_out,
           (NOT start_out AND NOT due_out AND start_date IS NOT NULL AND due_date IS NOT NULL
            AND start_date > due_date) AS order_bad
    FROM bad
    WHERE start_out OR due_out
       OR (start_date IS NOT NULL AND due_date IS NOT NULL AND start_date > due_date)
), logged AS (
    INSERT INTO migration_date_fixes (issue_id, field_key, old_value, reason)
    SELECT id, 'start_date', start_date, CASE WHEN start_out THEN 'out_of_range' ELSE 'date_order' END
    FROM plan WHERE start_out OR order_bad
    UNION ALL
    SELECT id, 'due_date', due_date, CASE WHEN due_out THEN 'out_of_range' ELSE 'date_order' END
    FROM plan WHERE due_out OR order_bad
    RETURNING issue_id
)
UPDATE issues i SET
    start_date = CASE WHEN p.start_out OR p.order_bad THEN NULL ELSE i.start_date END,
    due_date   = CASE WHEN p.due_out   OR p.order_bad THEN NULL ELSE i.due_date END,
    lock_version = i.lock_version + 1,
    updated_at = now()
FROM plan p WHERE p.id = i.id;

ALTER TABLE issues VALIDATE CONSTRAINT issues_date_order;
ALTER TABLE issues VALIDATE CONSTRAINT issues_date_range;
CREATE INDEX issues_version_idx ON issues (project_id, fixed_version_id);
CREATE INDEX issues_due_idx     ON issues (project_id, due_date) WHERE due_date IS NOT NULL;

-- Settings-version triggers (statement level: one bump per statement).
CREATE TRIGGER field_defs_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON field_defs
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER field_trackers_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON field_trackers
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER field_projects_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON field_projects
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER field_options_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON field_options
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER field_roles_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON field_roles
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER project_trackers_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON project_trackers
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER trackers_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON trackers
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
CREATE TRIGGER workflow_transitions_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON workflow_transitions
    FOR EACH STATEMENT EXECUTE FUNCTION bump_workflow_version();
CREATE TRIGGER workflow_field_rules_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON workflow_field_rules
    FOR EACH STATEMENT EXECUTE FUNCTION bump_workflow_version();
