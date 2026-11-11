-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.0 (§2.2): new permissions, time entry visibility per role,
-- the week start. No role gets a new permission here (no epoch bump).

-- Redmine names; triage_issues and manage_public_views are oxledger's.
ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_known;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_known CHECK (permission IN (
    'view_issues', 'add_issues', 'edit_issues', 'edit_own_issues', 'add_issue_notes',
    'set_notes_private', 'view_private_notes', 'set_issues_private', 'set_own_issues_private',
    'manage_issue_relations', 'manage_versions', 'manage_project',
    'view_time_entries', 'log_time', 'edit_time_entries', 'edit_own_time_entries',
    'log_time_for_other_users', 'triage_issues', 'manage_public_views'));

-- Only issue permissions may be limited to trackers (as in Redmine).
-- Existing rows that break this keep working as before (NOT VALID): the
-- constraint is validated only when there are none (05 appendix C R17).
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_tracker_scope CHECK (
    all_trackers OR permission IN ('view_issues', 'add_issues', 'edit_issues', 'edit_own_issues',
                                   'add_issue_notes')) NOT VALID;
DO $$
DECLARE n bigint;
BEGIN
    SELECT count(*) INTO n FROM role_permissions
    WHERE NOT all_trackers
      AND permission NOT IN ('view_issues', 'add_issues', 'edit_issues', 'edit_own_issues',
                             'add_issue_notes');
    IF n = 0 THEN
        ALTER TABLE role_permissions VALIDATE CONSTRAINT role_permissions_tracker_scope;
    ELSE
        RAISE NOTICE 'role_permissions_tracker_scope left NOT VALID: % existing tracker-limited non-issue permission(s)', n;
    END IF;
END $$;

-- Redmine Role#time_entries_visibility.
ALTER TABLE roles ADD COLUMN time_entries_visibility text NOT NULL DEFAULT 'all'
    CHECK (time_entries_visibility IN ('all', 'own'));
-- 0010's roles_epoch looked at issues_visibility only; now both columns
-- (a real change bumps every project, U1).
DROP TRIGGER roles_epoch ON roles;
CREATE TRIGGER roles_epoch AFTER UPDATE OF issues_visibility, time_entries_visibility ON roles
    FOR EACH ROW WHEN (OLD.issues_visibility IS DISTINCT FROM NEW.issues_visibility
                       OR OLD.time_entries_visibility IS DISTINCT FROM NEW.time_entries_visibility)
    EXECUTE FUNCTION epoch_all_projects();

-- "This week" (Redmine Setting.start_of_week): 1 = Monday … 7 = Sunday.
ALTER TABLE org_settings ADD COLUMN week_start smallint NOT NULL DEFAULT 1
    CHECK (week_start BETWEEN 1 AND 7);

-- The time_tracking module is a value of enabled_modules.module (text, no
-- DDL); 0010's enabled_modules epoch trigger already covers it.
