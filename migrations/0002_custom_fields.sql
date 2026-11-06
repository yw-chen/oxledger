-- oxledger custom fields (decision D14). SPDX-License-Identifier: GPL-2.0-or-later
-- Design and measurements: docs/research/02-e9-custom-fields.md (§5.2 schema, §5.3 query rules).
--
-- Typed EAV: one row per (issue, field) in issue_cf, exactly one typed value column set.
-- Every field gets its own covering partial index (field_id, v_x, issue_id) WHERE field_id = N,
-- built and dropped with CONCURRENTLY by the application (see "Per-field indexes" below).
-- No JSONB cache.

-- Field types and the value column each one uses:
--   int, float     -> v_num  (numeric; int is checked to be integral)
--   date           -> v_date
--   string, link   -> v_text (short: at most 2000 bytes so it fits a btree entry)
--   text           -> v_text (long, NOT indexed, only is_null / not_null filters)
--   list           -> v_ref  (field_options.id; renaming an option does not rewrite values)
--   user           -> v_ref  (principals.id)
--   bool           -> v_bool
-- An empty string is not stored: "" means no value (Redmine treats '' as none).
-- Changing a field's type is not supported (Redmine does not allow it either); a trigger rejects it.
--
-- Visibility (Redmine CustomField#visible + custom_fields_roles): a field with visible = false
-- can only be read (filtered on, shown) by admins and by users holding one of field_roles in the
-- issue's project. Only non-builtin roles may be listed (Redmine Role.givable). That keeps
-- Redmine's two checks identical in oxledger: SQL filters use member roles only, while display
-- uses roles_for_project, which adds the builtin Non member / Anonymous role.
CREATE TABLE field_defs (
    id           int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name         text NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    ftype        text NOT NULL CHECK (ftype IN ('int', 'float', 'date', 'string', 'text', 'list', 'user', 'bool', 'link')),
    description  text NOT NULL DEFAULT '',
    is_required  boolean NOT NULL DEFAULT false,
    -- Redmine semantics: the field applies to an issue when its tracker is in field_trackers
    -- and (is_for_all or its project is in field_projects).
    is_for_all   boolean NOT NULL DEFAULT false,
    visible      boolean NOT NULL DEFAULT true,
    position     integer NOT NULL DEFAULT 0,
    -- Lifecycle of the per-field index:
    --   pending  -> row created, index not built yet (filters work, just without the index)
    --   ready    -> index built and valid
    --   none     -> type has no index (text)
    --   dropping -> field is being deleted; new values are rejected
    index_state  text NOT NULL DEFAULT 'pending' CHECK (index_state IN ('pending', 'ready', 'none', 'dropping')),
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX field_defs_name_uniq ON field_defs (lower(name));

CREATE FUNCTION field_defs_freeze_type() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.ftype IS DISTINCT FROM OLD.ftype THEN
        RAISE EXCEPTION 'custom field % cannot change type (% -> %)', OLD.id, OLD.ftype, NEW.ftype
            USING ERRCODE = 'feature_not_supported';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER field_defs_freeze_type BEFORE UPDATE OF ftype ON field_defs
    FOR EACH ROW EXECUTE FUNCTION field_defs_freeze_type();

-- Roles that may read a field with visible = false.
CREATE TABLE field_roles (
    field_id int  NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    role_id  uuid NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (field_id, role_id)
);

CREATE FUNCTION field_roles_givable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM roles r WHERE r.id = NEW.role_id AND r.builtin IS NOT NULL) THEN
        RAISE EXCEPTION 'builtin role % cannot be given custom field visibility', NEW.role_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER field_roles_givable BEFORE INSERT OR UPDATE ON field_roles
    FOR EACH ROW EXECUTE FUNCTION field_roles_givable();

CREATE TABLE field_trackers (
    field_id   int  NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    tracker_id uuid NOT NULL REFERENCES trackers(id) ON DELETE CASCADE,
    PRIMARY KEY (field_id, tracker_id)
);

CREATE TABLE field_projects (
    field_id   int  NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    project_id uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    PRIMARY KEY (field_id, project_id)
);

-- Options of list fields. Values point at the option id. An option in use cannot be deleted
-- (field_options_guard below); deactivate it instead (active = false keeps existing values).
CREATE TABLE field_options (
    id        uuid PRIMARY KEY DEFAULT uuidv7(),
    field_id  int  NOT NULL REFERENCES field_defs(id) ON DELETE CASCADE,
    label     text NOT NULL CHECK (length(label) BETWEEN 1 AND 255),
    position  integer NOT NULL DEFAULT 0,
    active    boolean NOT NULL DEFAULT true
);
CREATE UNIQUE INDEX field_options_label_uniq ON field_options (field_id, lower(label));

-- field_id deliberately has NO foreign key to field_defs: the FK check on deleting a field
-- would look up issue_cf by field_id, which only the per-field partial index can serve, and
-- that index may already be gone. Instead issue_cf_check() locks the field_defs row FOR SHARE
-- and rejects fields being dropped. drop_field (crates/oxledger/src/custom_fields.rs) first sets
-- index_state = 'dropping', which waits for every writer holding that lock, then deletes the
-- values through the field's index, drops the index, and finally deletes the definition.
CREATE TABLE issue_cf (
    issue_id  uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    field_id  int  NOT NULL,
    v_date    date,
    v_bool    boolean,
    v_ref     uuid,
    v_num     numeric,
    v_text    text,
    PRIMARY KEY (issue_id, field_id),
    CHECK (num_nonnulls(v_num, v_date, v_text, v_ref, v_bool) = 1)
);

-- Keeps the value column consistent with the field type, so a filter on v_x for a field of
-- that type sees every value (the filter compiler relies on this).
-- Locks: the field_defs row FOR SHARE (write barrier for drop_field) and the list option FOR KEY
-- SHARE (the lock an FK takes, so deleting the option waits for or fails against this write).
-- Cost per write: one field_defs lookup + row lock (+ option/principal lookup). Not benchmarked;
-- shared row locks from concurrent writers of the same field create multixacts.
CREATE FUNCTION issue_cf_check() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    t  text;
    st text;
    ok boolean;
BEGIN
    SELECT ftype, index_state INTO t, st FROM field_defs WHERE id = NEW.field_id FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'unknown custom field %', NEW.field_id USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF st = 'dropping' THEN
        RAISE EXCEPTION 'custom field % is being deleted', NEW.field_id USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
    ok := CASE
        WHEN t = 'int'              THEN NEW.v_num IS NOT NULL AND NEW.v_num = trunc(NEW.v_num)
        WHEN t = 'float'            THEN NEW.v_num IS NOT NULL
        WHEN t = 'date'             THEN NEW.v_date IS NOT NULL
        WHEN t IN ('string','link') THEN NEW.v_text <> '' AND octet_length(NEW.v_text) <= 2000
        WHEN t = 'text'             THEN NEW.v_text <> ''
        WHEN t = 'list'             THEN NEW.v_ref IS NOT NULL AND EXISTS (
                                             SELECT 1 FROM field_options o WHERE o.id = NEW.v_ref AND o.field_id = NEW.field_id
                                             FOR KEY SHARE)
        WHEN t = 'user'             THEN NEW.v_ref IS NOT NULL AND EXISTS (
                                             SELECT 1 FROM principals p WHERE p.id = NEW.v_ref)
        WHEN t = 'bool'             THEN NEW.v_bool IS NOT NULL
        ELSE false
    END;
    IF NOT ok THEN
        RAISE EXCEPTION 'value does not match type % of custom field %', t, NEW.field_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER issue_cf_check BEFORE INSERT OR UPDATE ON issue_cf
    FOR EACH ROW EXECUTE FUNCTION issue_cf_check();

-- Options: id and field_id never change, and an option still referenced cannot be deleted.
-- The reference check uses a literal field id (dynamic SQL) so the per-field partial index
-- serves it. It is only sound in READ COMMITTED: a REPEATABLE READ deleter's older snapshot
-- could miss a reference committed in the meantime (native RI re-checks with a fresh snapshot;
-- plpgsql cannot), so other isolation levels are refused.
CREATE FUNCTION field_options_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE used boolean;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id <> OLD.id OR NEW.field_id <> OLD.field_id THEN
            RAISE EXCEPTION 'field option id and field_id cannot change' USING ERRCODE = 'check_violation';
        END IF;
        RETURN NEW;
    END IF;
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'deleting a field option requires READ COMMITTED'
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
    EXECUTE format('SELECT EXISTS (SELECT 1 FROM issue_cf WHERE field_id = %s AND v_ref = $1)', OLD.field_id)
        INTO used USING OLD.id;
    IF used THEN
        RAISE EXCEPTION 'option % of custom field % is in use; deactivate it instead', OLD.label, OLD.field_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER field_options_guard BEFORE UPDATE OR DELETE ON field_options
    FOR EACH ROW EXECUTE FUNCTION field_options_guard();

-- Planner statistics (research doc 02 §8, measured on a 1.3M-row synthetic issue_cf): with the
-- joint MCV of (field_id, v_ref), the estimate for `field_id = N AND v_ref = x` on a hot
-- list/user field went from about 30x too low to about 1x. The same object on (field_id, v_num)
-- did not help numeric fields (too many distinct values for an MCV list); the doc lists what remains.
CREATE STATISTICS issue_cf_field_ref (mcv) ON field_id, v_ref FROM issue_cf;

-- ---------------------------------------------------------------- Per-field indexes
--
-- Each field gets  issue_cf_f<id> ON issue_cf (field_id, v_x, issue_id) WHERE field_id = <id>.
-- The filter compiler inlines the field id as a literal so the planner can match the partial
-- index predicate (a bind parameter would not match it under a generic plan).
--
-- CREATE/DROP INDEX CONCURRENTLY cannot run inside a transaction (nor inside a function), so
-- these functions only produce the DDL text; the application executes it outside a transaction:
--   create: INSERT field_defs (index_state 'pending')
--           -> run cf_index_ddl(id) -> index_state 'ready'                    (custom_fields::build_index)
--   delete: index_state 'dropping' -> DELETE FROM issue_cf WHERE field_id = id AND v_x IS NOT NULL
--           -> run cf_drop_index_ddl(id) -> DELETE field_defs                 (custom_fields::drop_field)
-- custom_fields::reconcile_indexes finishes whatever a crash interrupted (pending builds, invalid
-- indexes left by a failed CONCURRENTLY build, half-done drops, orphan indexes) and runs at
-- startup and periodically in `oxledger serve`.

CREATE FUNCTION cf_value_column(ftype text) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT CASE
        WHEN ftype IN ('int', 'float')          THEN 'v_num'
        WHEN ftype = 'date'                     THEN 'v_date'
        WHEN ftype IN ('string', 'text', 'link') THEN 'v_text'
        WHEN ftype IN ('list', 'user')          THEN 'v_ref'
        WHEN ftype = 'bool'                     THEN 'v_bool'
    END
$$;

CREATE FUNCTION cf_index_name(field_id int) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT 'issue_cf_f' || field_id
$$;

-- NULL when the field type is not indexed (text) or the field does not exist.
CREATE FUNCTION cf_index_ddl(fid int) RETURNS text LANGUAGE sql STABLE STRICT AS $$
    SELECT format('CREATE INDEX CONCURRENTLY IF NOT EXISTS %I ON issue_cf (field_id, %I, issue_id) WHERE field_id = %s',
                  cf_index_name(d.id), cf_value_column(d.ftype), d.id)
    FROM field_defs d WHERE d.id = fid AND d.ftype <> 'text'
$$;

CREATE FUNCTION cf_drop_index_ddl(fid int) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT format('DROP INDEX CONCURRENTLY IF EXISTS %I', cf_index_name(fid))
$$;
