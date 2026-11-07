-- Single-condition estimates vs actual (D14 query shape: no redundant IS NOT NULL).
-- Output: name | estimated rows | actual rows | under-estimate factor
CREATE OR REPLACE FUNCTION est(q text) RETURNS TABLE(est float8, act float8) LANGUAGE plpgsql AS $$
DECLARE j json;
BEGIN
  EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || q INTO j;
  est := (j->0->'Plan'->>'Plan Rows')::float8; act := (j->0->'Plan'->>'Actual Rows')::float8; RETURN NEXT;
END $$;
SELECT n, e.est, e.act, round((e.act / greatest(e.est,1))::numeric, 2) AS under
FROM (VALUES
 ('f1 num hot >= 10',        'SELECT 1 FROM issue_cf WHERE field_id = 1 AND v_num >= 10'),
 ('f8 num hot 100..300',     'SELECT 1 FROM issue_cf WHERE field_id = 8 AND v_num BETWEEN 100 AND 300'),
 ('f2 date hot >= 2025-01-01','SELECT 1 FROM issue_cf WHERE field_id = 2 AND v_date >= ''2025-01-01'''),
 ('f9 date hot 2020',        'SELECT 1 FROM issue_cf WHERE field_id = 9 AND v_date BETWEEN ''2020-01-01'' AND ''2020-12-31'''),
 ('f800 num cold 1000..5000','SELECT 1 FROM issue_cf WHERE field_id = 800 AND v_num BETWEEN 1000 AND 5000'),
 ('f806 date cold >= 2024',  'SELECT 1 FROM issue_cf WHERE field_id = 806 AND v_date >= ''2024-01-01'''),
 ('f3 ref hot = opt0',       'SELECT 1 FROM issue_cf WHERE field_id = 3 AND v_ref = oid_(3,0)'),
 ('f1 presence',             'SELECT 1 FROM issue_cf WHERE field_id = 1 AND v_num IS NOT NULL')
) t(n, q), LATERAL est(q) e;
