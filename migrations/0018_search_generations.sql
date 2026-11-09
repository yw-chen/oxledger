-- P2 search wrap-up (design 03 §4.8–4.9, §7 steps 7–11). SPDX-License-Identifier: GPL-2.0-or-later
--
-- Search generations: each index generation consumes the outbox as its own
-- consumer `search@<generation uuid>`. The index's commit payload is the
-- recovery basis; these rows coordinate leases, monitoring and the outbox GC.
--   active        serving and syncing
--   bootstrapping being rebuilt from a snapshot (holds back the GC from its
--                 lease cursor on, 03 §4.8 step 1)
--   retained      replaced, kept for the rollback window (retained_until)
--   retired       done; does not hold back the GC, removed by it
ALTER TABLE outbox_cursors
    ADD COLUMN state text NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'bootstrapping', 'retained', 'retired')),
    ADD COLUMN retained_until timestamptz,
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now(),
    ADD CONSTRAINT outbox_cursors_retained_until CHECK ((state = 'retained') = (retained_until IS NOT NULL));

-- The pre-generation consumer: its index directory has no payload, so the
-- first start rebuilds from a snapshot (docs/impl/upgrade-notes.md).
DELETE FROM outbox_cursors WHERE consumer = 'search';

-- `oxledger reindex` asks the running server for a blue-green rebuild.
ALTER TABLE search_settings ADD COLUMN rebuild_requested_at timestamptz;

-- Design 03 §7 step 11: custom field definitions carry a version that
-- filters may pin (`field_definition_changed` when it moved on) and a unit
-- (`resolve(kind: "custom_field")`). The version moves whenever what a
-- filter value means could change: type, unit, or the option list.
ALTER TABLE field_defs
    ADD COLUMN definition_version integer NOT NULL DEFAULT 1 CHECK (definition_version >= 1),
    ADD COLUMN unit text CHECK (unit IS NULL OR char_length(unit) BETWEEN 1 AND 20);

CREATE FUNCTION field_defs_bump_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.ftype IS DISTINCT FROM OLD.ftype OR NEW.unit IS DISTINCT FROM OLD.unit THEN
        NEW.definition_version := OLD.definition_version + 1;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER field_defs_bump_version BEFORE UPDATE OF ftype, unit ON field_defs
    FOR EACH ROW EXECUTE FUNCTION field_defs_bump_version();

CREATE FUNCTION field_options_bump_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.label IS NOT DISTINCT FROM OLD.label
       AND NEW.active IS NOT DISTINCT FROM OLD.active THEN
        RETURN NULL;
    END IF;
    -- Options written in the transaction that created the field are part
    -- of its first definition (created_at = now() = this transaction).
    UPDATE field_defs SET definition_version = definition_version + 1
    WHERE id = COALESCE(NEW.field_id, OLD.field_id)
      AND created_at <> now();
    RETURN NULL;
END $$;
CREATE TRIGGER field_options_bump_version AFTER INSERT OR UPDATE OR DELETE ON field_options
    FOR EACH ROW EXECUTE FUNCTION field_options_bump_version();

-- Design 03 §5.4: the filter-only list is ordered by (updated_at, id)
-- descending; strategy S2 walks this index backwards and probes each
-- issue's custom field conditions until a page is full.
CREATE INDEX IF NOT EXISTS issues_updated_idx ON issues (updated_at DESC, id DESC);
