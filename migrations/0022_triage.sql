-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 05 §2.4 (M1.3): Triage settings per project, the per-issue
-- triage state (with the A5 flags self_triage / same_sponsor), and the
-- triage_decide command in the agent op lists.
--
-- No role gets triage_issues here (user decision 2026-11-10: M1.4 grants
-- the M1 permissions to the built-in roles in one migration). Existing
-- issues never enter the inbox (§2.4: enabling Triage does not backfill).

-- R25: composite key so issue_triage.project_id can only be the issue's
-- project. 0023 (M1.4) must not add it again.
ALTER TABLE issues ADD CONSTRAINT issues_id_project_uq UNIQUE (id, project_id);

-- ---------------------------------------------------------------- settings

CREATE TABLE project_triage_settings (
    project_id          uuid PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    enabled             boolean NOT NULL DEFAULT false,
    entry_rule          text NOT NULL DEFAULT 'creator_without_triage'
                          CHECK (entry_rule IN ('creator_without_triage', 'all_new')),
    responsible_user_id uuid REFERENCES principals(id),
    decline_status_id   uuid REFERENCES issue_statuses(id),
    lock_version        integer NOT NULL DEFAULT 0,
    updated_by          uuid REFERENCES principals(id),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CHECK (NOT enabled OR decline_status_id IS NOT NULL)
);

INSERT INTO project_triage_settings (project_id) SELECT id FROM projects;

CREATE FUNCTION project_triage_settings_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO project_triage_settings (project_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER project_triage_settings_row AFTER INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION project_triage_settings_row();

-- The responsible person is a user (not a group or an agent); the decline
-- status is a closed one (category done). Checked when the value is set or
-- changed: a value that went stale later (a status edited by hand) does not
-- block saving other settings; the API warns about it instead.
CREATE FUNCTION project_triage_settings_check() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.responsible_user_id IS NOT NULL
       AND (TG_OP = 'INSERT' OR NEW.responsible_user_id IS DISTINCT FROM OLD.responsible_user_id)
       AND NOT EXISTS (
        SELECT 1 FROM principals WHERE id = NEW.responsible_user_id AND kind = 'user') THEN
        RAISE EXCEPTION 'project_triage_settings.responsible_user_id must be a user'
            USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.decline_status_id IS NOT NULL
       AND (TG_OP = 'INSERT' OR NEW.decline_status_id IS DISTINCT FROM OLD.decline_status_id)
       AND NOT EXISTS (
        SELECT 1 FROM issue_statuses WHERE id = NEW.decline_status_id AND category = 'done') THEN
        RAISE EXCEPTION 'project_triage_settings.decline_status_id must be a done status'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER project_triage_settings_check BEFORE INSERT OR UPDATE ON project_triage_settings
    FOR EACH ROW EXECUTE FUNCTION project_triage_settings_check();

-- ---------------------------------------------------------------- per issue

-- Written only by the command engine inside the issue's transaction.
CREATE TABLE issue_triage (
    issue_id      uuid PRIMARY KEY REFERENCES issues(id),
    project_id    uuid NOT NULL REFERENCES projects(id),
    state         text NOT NULL CHECK (state IN ('pending', 'accepted', 'declined', 'duplicate')),
    entered_at    timestamptz NOT NULL,
    snoozed_until timestamptz,
    snoozed_at    timestamptz,
    snoozed_by    uuid REFERENCES principals(id),
    decided_at    timestamptz,
    decided_by    uuid REFERENCES principals(id),
    -- A5: computed by the engine at the moment of the decision.
    self_triage   boolean NOT NULL DEFAULT false,
    same_sponsor  boolean NOT NULL DEFAULT false,
    FOREIGN KEY (issue_id, project_id) REFERENCES issues(id, project_id) ON UPDATE CASCADE,
    CHECK ((snoozed_until IS NULL) = (snoozed_at IS NULL)),
    CHECK (state = 'pending' OR snoozed_until IS NULL),
    CHECK ((state = 'pending') = (decided_at IS NULL)),
    CHECK (NOT (self_triage AND same_sponsor)),
    CHECK (state <> 'pending' OR NOT (self_triage OR same_sponsor))
);
CREATE INDEX issue_triage_pending_idx ON issue_triage (project_id, entered_at) WHERE state = 'pending';
CREATE INDEX issue_triage_decided_idx ON issue_triage (project_id, decided_at DESC) WHERE state <> 'pending';

-- ---------------------------------------------------------------- op lists
-- Union of every value so far (0011) plus triage_decide. Existing
-- delegations are not extended (D20 rule 1); only new ones get it.

ALTER TABLE delegation_command_policies DROP CONSTRAINT delegation_command_policies_command_kind_check;
ALTER TABLE delegation_command_policies ADD CONSTRAINT delegation_command_policies_command_kind_check
    CHECK (command_kind IN (
        'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
        'create_version', 'update_version', 'triage_decide'));
ALTER TABLE delegations DROP CONSTRAINT delegations_proposal_ops_check;
ALTER TABLE delegations ADD CONSTRAINT delegations_proposal_ops_check CHECK (proposal_ops <@ ARRAY[
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide']);
ALTER TABLE changeset_version_ops DROP CONSTRAINT changeset_version_ops_kind_check;
ALTER TABLE changeset_version_ops ADD CONSTRAINT changeset_version_ops_kind_check CHECK (kind IN (
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide'));

-- Settings history: union of 0015's kinds plus triage_settings_updated.
ALTER TABLE project_setting_events DROP CONSTRAINT project_setting_events_kind_check;
ALTER TABLE project_setting_events ADD CONSTRAINT project_setting_events_kind_check
    CHECK (kind IN ('tabs_updated', 'tabs_reset', 'repo_linked', 'repo_unlinked',
                    'pr_hidden', 'pr_unhidden', 'github_content_revoked',
                    'triage_settings_updated'));
