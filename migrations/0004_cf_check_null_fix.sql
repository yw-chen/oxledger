-- oxledger: close the NULL hole in issue_cf_check (contract 02 §2 P1). SPDX-License-Identifier: GPL-2.0-or-later
--
-- 0002 ended the type check with `IF NOT ok`. When the CASE yields NULL (for example a string
-- field whose value was written to v_num: `NEW.v_text <> ''` is NULL), `NOT NULL` is NULL, the
-- branch is skipped and the bad row is accepted. The check now rejects anything that is not
-- exactly TRUE.
--
-- Rows written before this migration are scanned first with the same rules. If any row does not
-- match its field type the migration fails and lists a few of them, so nobody runs with values
-- that filters (which read only the field type's value column) silently ignore. Fix or delete
-- those rows, then start again; docs/playbook.md has the query.

DO $$
DECLARE
    bad_count bigint;
    sample    text;
BEGIN
    WITH bad AS (
        SELECT c.issue_id, c.field_id, d.ftype
        FROM issue_cf c
        LEFT JOIN field_defs d ON d.id = c.field_id
        WHERE (CASE
            WHEN d.ftype IS NULL           THEN false
            WHEN d.ftype = 'int'           THEN c.v_num IS NOT NULL AND c.v_num = trunc(c.v_num)
            WHEN d.ftype = 'float'         THEN c.v_num IS NOT NULL
            WHEN d.ftype = 'date'          THEN c.v_date IS NOT NULL
            WHEN d.ftype IN ('string','link') THEN c.v_text <> '' AND octet_length(c.v_text) <= 2000
            WHEN d.ftype = 'text'          THEN c.v_text <> ''
            WHEN d.ftype = 'list'          THEN c.v_ref IS NOT NULL AND EXISTS (
                                                  SELECT 1 FROM field_options o
                                                  WHERE o.id = c.v_ref AND o.field_id = c.field_id)
            WHEN d.ftype = 'user'          THEN c.v_ref IS NOT NULL AND EXISTS (
                                                  SELECT 1 FROM principals p WHERE p.id = c.v_ref)
            WHEN d.ftype = 'bool'          THEN c.v_bool IS NOT NULL
            ELSE false
        END) IS DISTINCT FROM TRUE
    )
    SELECT count(*),
           string_agg(format('(issue %s, field %s, type %s)', issue_id, field_id, coalesce(ftype, '?')), ', ')
               FILTER (WHERE rn <= 5)
    INTO bad_count, sample
    FROM (SELECT bad.*, row_number() OVER (ORDER BY field_id, issue_id) AS rn FROM bad) s;

    IF bad_count > 0 THEN
        RAISE EXCEPTION '% issue_cf row(s) do not match their custom field type, e.g. %', bad_count, sample
            USING ERRCODE = 'check_violation',
                  HINT = 'See docs/playbook.md ("migration 0004 fails") to list and fix them.';
    END IF;
END $$;

CREATE OR REPLACE FUNCTION issue_cf_check() RETURNS trigger LANGUAGE plpgsql AS $$
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
    -- NULL counts as a failure (0002 used `IF NOT ok`, which let NULL through).
    IF ok IS DISTINCT FROM TRUE THEN
        RAISE EXCEPTION 'value does not match type % of custom field %', t, NEW.field_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
