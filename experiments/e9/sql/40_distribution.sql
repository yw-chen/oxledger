-- Data distribution summary (printed after load).
SELECT 'issues: ' || count(*) FROM issues_a;
SELECT 'issue_cf rows: ' || count(*) FROM issue_cf;
SELECT 'fields per issue: avg ' || round(avg(k), 1) || ', p5 ' || percentile_disc(0.05) WITHIN GROUP (ORDER BY k)
       || ', p50 ' || percentile_disc(0.5) WITHIN GROUP (ORDER BY k)
       || ', p95 ' || percentile_disc(0.95) WITHIN GROUP (ORDER BY k) || ', max ' || max(k)
FROM (SELECT count(*) AS k FROM issue_cf GROUP BY issue_id) s;
SELECT 'field types: ' || string_agg(ftype || '=' || c, ', ' ORDER BY ftype)
FROM (SELECT ftype, count(*) c FROM cf_defs GROUP BY ftype) s;
WITH cov AS (SELECT d.id, count(c.issue_id) AS k FROM cf_defs d LEFT JOIN issue_cf c ON c.field_id = d.id GROUP BY d.id)
SELECT 'fields by coverage: ' || string_agg(b || '=' || c, ', ' ORDER BY o)
FROM (SELECT CASE WHEN k >= 500000 THEN '>=50%' WHEN k >= 100000 THEN '10-50%' WHEN k >= 10000 THEN '1-10%'
                  WHEN k >= 3000 THEN '0.3-1%' ELSE '<0.3%' END AS b,
             CASE WHEN k >= 500000 THEN 1 WHEN k >= 100000 THEN 2 WHEN k >= 10000 THEN 3 WHEN k >= 3000 THEN 4 ELSE 5 END AS o,
             count(*) c FROM cov GROUP BY 1, 2) s;
WITH cov AS (SELECT d.id, count(c.issue_id) AS k FROM cf_defs d LEFT JOIN issue_cf c ON c.field_id = d.id GROUP BY d.id)
SELECT 'coverage of benchmark fields: ' || string_agg(id || '(' || ftype || ')=' || k, ', ' ORDER BY id)
FROM cov JOIN cf_defs USING (id) WHERE id IN (1,2,3,4,8,11,300,301,302,303,304,305,306,307,308,309,310)
   OR id IN (SELECT id FROM cf_defs WHERE id BETWEEN 800 AND 820);
SELECT 'visible issues (projects 101..130, non-private or author=uid(7)): ' || count(*)
FROM issues_a WHERE project_id = ANY (ARRAY(SELECT pid(g) FROM generate_series(101, 130) g))
  AND (NOT is_private OR author_id = uid(7));
SELECT 'avg fields jsonb bytes: ' || round(avg(pg_column_size(fields))) || ', max ' || max(pg_column_size(fields))
FROM issues_b;
