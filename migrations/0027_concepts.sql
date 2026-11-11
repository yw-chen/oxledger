-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.7 (§2.9, §3.9): concepts and their labels for query-side
-- alias expansion, and the outbox events the wider BM25F index needs.
--
-- No role, permission or membership changes: security_epoch does not move.
-- Concepts and labels are public site data (every signed-in person reads
-- them); only administrators manage them (application rule).
--
-- R18 (appendix C): labels are compared after NFC + casefold, both in the
-- unique index and in the application (`casefold(normalize(x, NFC))`, the
-- same SQL expression everywhere), not with lower(). Before the index is
-- built, concept labels that would collide are demoted to `staging` (one
-- stays active per normalized label), so the migration cannot fail on
-- existing data.

CREATE TABLE concepts (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    pref_label   text NOT NULL CHECK (char_length(pref_label) BETWEEN 1 AND 60),
    status       text NOT NULL DEFAULT 'active' CHECK (status IN ('active','retired')),
    lock_version integer NOT NULL DEFAULT 0,
    created_by   uuid NOT NULL REFERENCES principals(id),
    created_at   timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE aliases
    ADD COLUMN status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active','staging')),
    ADD COLUMN created_by uuid REFERENCES principals(id),
    ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();

-- Concept labels written before 0027 (seed or import) had no concept row:
-- give each one, made by the system principal (0008). The preferred label
-- is the `pref` label when there is one, else the smallest label (bytewise).
INSERT INTO concepts (id, pref_label, created_by)
SELECT a.entity_id,
       COALESCE(NULLIF(left(btrim(
           (array_agg(a.label ORDER BY (a.label_kind = 'pref') DESC, a.label COLLATE "C"))[1]), 60), ''),
           'concept'),
       '00000000-0000-7000-8000-000000000001'
FROM aliases a
WHERE a.entity_kind = 'concept'
GROUP BY a.entity_id;

-- R18: one active label per normalized label. Within a group the `pref`
-- label of the oldest concept (uuidv7 order) wins, then the smallest label.
DO $$
DECLARE n bigint;
BEGIN
    WITH ranked AS (
        SELECT entity_id, label,
               row_number() OVER (PARTITION BY casefold(normalize(label, NFC))
                                  ORDER BY (label_kind = 'pref') DESC, entity_id, label COLLATE "C") AS rn
        FROM aliases
        WHERE entity_kind = 'concept' AND status = 'active'
    ), demoted AS (
        UPDATE aliases a SET status = 'staging'
        FROM ranked r
        WHERE a.entity_kind = 'concept' AND a.entity_id = r.entity_id AND a.label = r.label
          AND r.rn > 1
        RETURNING 1
    )
    SELECT count(*) INTO n FROM demoted;
    IF n > 0 THEN
        RAISE NOTICE '0027: % concept label(s) collided after NFC + casefold and were set to staging', n;
    END IF;
END $$;

-- Every concept keeps exactly one pref label, and it is active: its name
-- must expand and hold its place in the unique index. A pref label that
-- was demoted above (or a second pref label) becomes an alt label; a
-- concept left without an active pref label promotes one of its active
-- labels (alt before hidden, then bytewise smallest, 1-60 characters) and
-- takes it as its name; a concept with no active label left is retired
-- (its labels stay, as staging, for an administrator to sort out).
UPDATE aliases SET label_kind = 'alt'
WHERE entity_kind = 'concept' AND label_kind = 'pref' AND status = 'staging';
WITH keep AS (
    SELECT DISTINCT ON (entity_id) entity_id, label
    FROM aliases
    WHERE entity_kind = 'concept' AND label_kind = 'pref'
    ORDER BY entity_id, label COLLATE "C")
UPDATE aliases a SET label_kind = 'alt'
FROM keep
WHERE a.entity_kind = 'concept' AND a.entity_id = keep.entity_id
  AND a.label_kind = 'pref' AND a.label <> keep.label;
WITH pick AS (
    SELECT DISTINCT ON (a.entity_id) a.entity_id, a.label
    FROM aliases a
    WHERE a.entity_kind = 'concept' AND a.status = 'active'
      AND char_length(a.label) BETWEEN 1 AND 60
      AND NOT EXISTS (SELECT 1 FROM aliases p
                      WHERE p.entity_kind = 'concept' AND p.entity_id = a.entity_id
                        AND p.label_kind = 'pref')
    ORDER BY a.entity_id, (a.label_kind = 'alt') DESC, a.label COLLATE "C")
UPDATE aliases a SET label_kind = 'pref'
FROM pick
WHERE a.entity_kind = 'concept' AND a.entity_id = pick.entity_id AND a.label = pick.label;
UPDATE concepts c SET pref_label = a.label
FROM aliases a
WHERE a.entity_kind = 'concept' AND a.entity_id = c.id AND a.label_kind = 'pref'
  AND char_length(a.label) BETWEEN 1 AND 60 AND c.pref_label <> a.label;
UPDATE concepts c SET status = 'retired'
WHERE NOT EXISTS (SELECT 1 FROM aliases p
                  WHERE p.entity_kind = 'concept' AND p.entity_id = c.id
                    AND p.label_kind = 'pref' AND p.status = 'active');

-- Same label in one active concept only, so an expansion is never ambiguous.
CREATE UNIQUE INDEX aliases_concept_label_uq ON aliases (casefold(normalize(label, NFC)))
    WHERE entity_kind = 'concept' AND status = 'active';
CREATE INDEX aliases_concept_idx ON aliases (entity_id) WHERE entity_kind = 'concept';

-- ---------------------------------------------------------------- outbox (BM25F sources)

-- The index now holds the issue keys, the public notes and the visible
-- string/text/link custom field values (§3.9). Their changes enqueue the
-- issue like an issue update does (topic 'issue'). Correctness does not
-- depend on these events: every hit is re-checked against the current
-- text in SQL (03 §3.4); they keep the ranking fresh.
--
-- At most one event per issue and transaction from these triggers: a
-- command that updates an issue and adds a note, or a new issue and its
-- key, still enqueues the issue once (the reader dedupes by issue anyway;
-- this keeps the outbox at one message per write).
CREATE FUNCTION outbox_issue_once(i uuid) RETURNS void LANGUAGE sql AS $$
    INSERT INTO outbox (topic, entity_id)
    SELECT 'issue', i
    WHERE i IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM outbox o
        WHERE o.xid = pg_current_xact_id() AND o.topic = 'issue' AND o.entity_id = i);
$$;

-- The issue's own trigger (0001) uses the same rule, so the order of the
-- writes in a transaction does not matter.
CREATE OR REPLACE FUNCTION issues_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM outbox_issue_once(COALESCE(NEW.id, OLD.id));
    RETURN NULL;
END $$;

CREATE FUNCTION issue_child_outbox() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM outbox_issue_once(COALESCE(NEW.issue_id, OLD.issue_id));
    IF TG_OP = 'UPDATE' AND NEW.issue_id IS DISTINCT FROM OLD.issue_id THEN
        PERFORM outbox_issue_once(OLD.issue_id);
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER issue_notes_outbox
    AFTER INSERT OR DELETE OR UPDATE OF body, is_private, retracted_at, redacted_at, issue_id
    ON issue_notes FOR EACH ROW EXECUTE FUNCTION issue_child_outbox();
CREATE TRIGGER issue_keys_outbox
    AFTER INSERT OR DELETE OR UPDATE OF key, issue_id
    ON issue_keys FOR EACH ROW EXECUTE FUNCTION issue_child_outbox();

-- Custom field values: one statement-level event per issue, text-like
-- fields only (other types are not indexed). Transition tables allow one
-- event per trigger.
CREATE FUNCTION issue_cf_outbox() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM outbox_issue_once(x.issue_id)
    FROM (SELECT DISTINCT c.issue_id FROM changed_rows c
          JOIN field_defs d ON d.id = c.field_id AND d.ftype IN ('string', 'text', 'link')) x;
    RETURN NULL;
END $$;
CREATE TRIGGER issue_cf_outbox_ins AFTER INSERT ON issue_cf
    REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION issue_cf_outbox();
CREATE TRIGGER issue_cf_outbox_upd AFTER UPDATE ON issue_cf
    REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION issue_cf_outbox();
CREATE TRIGGER issue_cf_outbox_del AFTER DELETE ON issue_cf
    REFERENCING OLD TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION issue_cf_outbox();

-- A text-like field whose visibility, scope or life cycle changes: every
-- issue holding a value of it (03 §3.8 "欄位權限有變化時…要對受影響的 issue
-- 發 outbox"). An admin action; it reads issue_cf by field_id.
CREATE FUNCTION field_defs_outbox() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO outbox (topic, entity_id)
    SELECT DISTINCT 'issue', cf.issue_id
    FROM new_defs n JOIN old_defs o ON o.id = n.id
    JOIN issue_cf cf ON cf.field_id = n.id
    WHERE n.ftype IN ('string', 'text', 'link')
      AND (n.visible IS DISTINCT FROM o.visible
           OR n.is_for_all IS DISTINCT FROM o.is_for_all
           OR (n.index_state = 'dropping') IS DISTINCT FROM (o.index_state = 'dropping'));
    RETURN NULL;
END $$;
CREATE TRIGGER field_defs_outbox AFTER UPDATE ON field_defs
    REFERENCING OLD TABLE AS old_defs NEW TABLE AS new_defs
    FOR EACH STATEMENT EXECUTE FUNCTION field_defs_outbox();

CREATE FUNCTION field_scope_outbox() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO outbox (topic, entity_id)
    SELECT DISTINCT 'issue', cf.issue_id
    FROM (SELECT DISTINCT field_id FROM changed_rows) c
    JOIN field_defs d ON d.id = c.field_id AND d.ftype IN ('string', 'text', 'link')
    JOIN issue_cf cf ON cf.field_id = c.field_id;
    RETURN NULL;
END $$;
CREATE TRIGGER field_trackers_outbox_ins AFTER INSERT ON field_trackers
    REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION field_scope_outbox();
CREATE TRIGGER field_trackers_outbox_del AFTER DELETE ON field_trackers
    REFERENCING OLD TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION field_scope_outbox();
CREATE TRIGGER field_projects_outbox_ins AFTER INSERT ON field_projects
    REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION field_scope_outbox();
CREATE TRIGGER field_projects_outbox_del AFTER DELETE ON field_projects
    REFERENCING OLD TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION field_scope_outbox();
