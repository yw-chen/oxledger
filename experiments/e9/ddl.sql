-- Q7: delete a field / change its type. SPDX-License-Identifier: GPL-2.0-or-later
-- Each statement runs 3 times inside BEGIN..ROLLBACK (data unchanged), VACUUM between runs (untimed).
-- A: field 807 (cold list), 3 (hot list) deleted; 800 (cold num), 1 (hot num) converted num -> text.
-- B: same fields, key removed / value rewritten in issues_b.fields. In B, retyping a field that has an
--    expression index (field 1) would also need DROP/CREATE INDEX; not included in the timing.
-- A: the type predicate (v_text IS NOT NULL) lets the delete use the per-type partial index; without it
--    neither the partial indexes nor the PK (issue_id, field_id) can find the rows and it seq-scans issue_cf.
\set QUIET on
\echo @@BEGIN drop_cold A
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 807 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 807 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 807 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
\echo @@END
\echo @@BEGIN drop_cold B
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '807' WHERE fields ? '807';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '807' WHERE fields ? '807';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '807' WHERE fields ? '807';
\timing off
ROLLBACK;
VACUUM issues_b;
\echo @@END
\echo @@BEGIN drop_hot A
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 3 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 3 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
DELETE FROM issue_cf WHERE field_id = 3 AND v_text IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
\echo @@END
\echo @@BEGIN drop_hot B
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '3' WHERE fields ? '3';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '3' WHERE fields ? '3';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = fields - '3' WHERE fields ? '3';
\timing off
ROLLBACK;
VACUUM issues_b;
\echo @@END
\echo @@BEGIN retype_cold A
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 800 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 800 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 800 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
\echo @@END
\echo @@BEGIN retype_cold B
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{800}', to_jsonb(fields->>'800')) WHERE fields ? '800';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{800}', to_jsonb(fields->>'800')) WHERE fields ? '800';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{800}', to_jsonb(fields->>'800')) WHERE fields ? '800';
\timing off
ROLLBACK;
VACUUM issues_b;
\echo @@END
\echo @@BEGIN retype_hot A
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 1 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 1 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
BEGIN;
\timing on
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 1 AND v_num IS NOT NULL;
\timing off
ROLLBACK;
VACUUM issue_cf;
\echo @@END
\echo @@BEGIN retype_hot B
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{1}', to_jsonb(fields->>'1')) WHERE fields ? '1';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{1}', to_jsonb(fields->>'1')) WHERE fields ? '1';
\timing off
ROLLBACK;
VACUUM issues_b;
BEGIN;
\timing on
UPDATE issues_b SET fields = jsonb_set(fields, '{1}', to_jsonb(fields->>'1')) WHERE fields ? '1';
\timing off
ROLLBACK;
VACUUM issues_b;
\echo @@END
