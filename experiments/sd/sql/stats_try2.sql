-- (4) per-field expression index whose predicate the query implies, plus expression statistics.
\pset footer off
\timing on
CREATE INDEX issue_cf_f8_case ON issue_cf ((CASE WHEN field_id = 8 THEN v_num END), issue_id)
  WHERE (CASE WHEN field_id = 8 THEN v_num END) IS NOT NULL;
CREATE STATISTICS issue_cf_f8_x ON (CASE WHEN field_id = 8 THEN v_num END) FROM issue_cf;
ANALYZE issue_cf;
\timing off
SELECT '(4) case expr, alone' AS t, * FROM est('SELECT 1 FROM issue_cf WHERE (CASE WHEN field_id = 8 THEN v_num END) BETWEEN 100 AND 300');
EXPLAIN SELECT 1 FROM issue_cf WHERE (CASE WHEN field_id = 8 THEN v_num END) BETWEEN 100 AND 300;
SELECT pg_size_pretty(pg_relation_size('issue_cf_f8_case')) AS case_idx, pg_size_pretty(pg_relation_size('issue_cf_f8')) AS f8_idx;
