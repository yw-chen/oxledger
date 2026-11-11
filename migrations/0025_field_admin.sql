-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 05 §2.7 (M1.5): custom field management (L0). Deactivation and
-- an optimistic lock on field_defs, the Q-S9 type conversion jobs, and the
-- L0 counter that the effective rules version includes (appendix C R13).
--
-- No role, permission or membership changes: security_epoch does not move.
-- Visibility and scope changes keep going through the 0010 epoch triggers.

ALTER TABLE field_defs
    ADD COLUMN deactivated_at timestamptz,   -- not in describe or forms, no new values; old values stay, filters work
    ADD COLUMN lock_version   integer NOT NULL DEFAULT 0;

-- Q-S9 (03 §5.7): a new field, a copy with conversion in batches, then
-- (by hand) the old field deactivated.
CREATE TABLE custom_field_conversions (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    -- Deleting either field (the dropping flow) removes its conversions.
    from_field_id   int NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    to_field_id     int NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    mapping         jsonb NOT NULL,           -- option map (list→list, string→list) or parse rule
    state           text NOT NULL CHECK (state IN ('previewed','running','succeeded','failed','canceled')),
    batches_done    integer NOT NULL DEFAULT 0,
    -- Resume point: the batches run in issue id order; a batch commits its
    -- writes and this cursor together, so a rerun never writes twice.
    cursor_issue_id uuid,
    report          jsonb,                    -- counts, failing issue ids and reason codes; never values
    created_by      uuid NOT NULL REFERENCES principals(id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    started_at      timestamptz,
    finished_at     timestamptz,
    CHECK (from_field_id <> to_field_id)
);
CREATE INDEX custom_field_conversions_running ON custom_field_conversions (created_at) WHERE state = 'running';

-- ---------------------------------------------------------------- L0 in rules_version (R13)

-- The effective rules version an agent pins with expected_rules_version
-- covers L0 changes that change what a write must satisfy: required,
-- deactivation, scope, visibility, dropping, and options appearing,
-- disappearing or (de)activating. Renames and layout changes do not move it.
ALTER TABLE org_settings ADD COLUMN form_l0_version bigint NOT NULL DEFAULT 1;

CREATE FUNCTION bump_form_l0_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE org_settings SET form_l0_version = form_l0_version + 1;
    RETURN NULL;
END $$;

CREATE TRIGGER field_defs_l0_insdel AFTER INSERT OR DELETE ON field_defs
    FOR EACH STATEMENT EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_defs_l0_update AFTER UPDATE ON field_defs
    FOR EACH ROW WHEN (OLD.is_required IS DISTINCT FROM NEW.is_required
                       OR OLD.deactivated_at IS DISTINCT FROM NEW.deactivated_at
                       OR OLD.visible IS DISTINCT FROM NEW.visible
                       OR OLD.is_for_all IS DISTINCT FROM NEW.is_for_all
                       OR (OLD.index_state <> 'dropping' AND NEW.index_state = 'dropping'))
    EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_trackers_l0 AFTER INSERT OR UPDATE OR DELETE ON field_trackers
    FOR EACH STATEMENT EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_projects_l0 AFTER INSERT OR UPDATE OR DELETE ON field_projects
    FOR EACH STATEMENT EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_roles_l0 AFTER INSERT OR UPDATE OR DELETE ON field_roles
    FOR EACH STATEMENT EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_options_l0_insdel AFTER INSERT OR DELETE ON field_options
    FOR EACH STATEMENT EXECUTE FUNCTION bump_form_l0_version();
CREATE TRIGGER field_options_l0_update AFTER UPDATE ON field_options
    FOR EACH ROW WHEN (OLD.active IS DISTINCT FROM NEW.active)
    EXECUTE FUNCTION bump_form_l0_version();
