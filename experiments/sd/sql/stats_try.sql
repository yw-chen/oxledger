-- Can any statistics object fix the per-field range estimate? (field 8, continuous numeric, 80% coverage)
\pset footer off
SELECT 'baseline' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE field_id = 8 AND v_num BETWEEN 100 AND 300');
-- (1) bigger per-column statistics target
ALTER TABLE issue_cf ALTER COLUMN v_num SET STATISTICS 1000; ANALYZE issue_cf;
SELECT 'v_num target 1000' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE field_id = 8 AND v_num BETWEEN 100 AND 300');
ALTER TABLE issue_cf ALTER COLUMN v_num SET STATISTICS -1;
-- (2) partial expression index: ANALYZE samples only rows matching the predicate; does the planner use it?
CREATE INDEX issue_cf_f8_expr ON issue_cf ((v_num + 0)) WHERE field_id = 8; ANALYZE issue_cf;
SELECT 'partial expr index, expr query' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE field_id = 8 AND v_num + 0 BETWEEN 100 AND 300');
DROP INDEX issue_cf_f8_expr;
-- (3) expression statistics on a per-field projection, query written with the same expression
CREATE STATISTICS issue_cf_f8_x ON (CASE WHEN field_id = 8 THEN v_num END) FROM issue_cf; ANALYZE issue_cf;
SELECT 'expr stats CASE, with field_id' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE field_id = 8 AND (CASE WHEN field_id = 8 THEN v_num END) BETWEEN 100 AND 300');
SELECT 'expr stats CASE, alone' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE (CASE WHEN field_id = 8 THEN v_num END) BETWEEN 100 AND 300');
DROP STATISTICS issue_cf_f8_x; ANALYZE issue_cf;
