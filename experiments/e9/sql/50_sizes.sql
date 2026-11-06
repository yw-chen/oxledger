-- Q8: storage per relation (heap incl. TOAST, each index), in MB.
SELECT 'relation', 'kind', 'parent', 'MB';
SELECT c.relname, CASE c.relkind WHEN 'i' THEN 'index' ELSE 'table' END,
       coalesce(t.relname, c.relname),
       round(CASE WHEN c.relkind = 'i' THEN pg_relation_size(c.oid)
                  ELSE pg_table_size(c.oid) END / 1048576.0, 1)
FROM pg_class c
LEFT JOIN pg_index x ON x.indexrelid = c.oid
LEFT JOIN pg_class t ON t.oid = x.indrelid
WHERE c.relnamespace = 'public'::regnamespace
  AND c.relkind IN ('r', 'i')
  AND coalesce(t.relname, c.relname) IN ('issues_a', 'issue_cf', 'issues_b', 'issues_c')
ORDER BY 3, 2 DESC, 1;
SELECT 'TOTAL ' || s, '', '', round(sum(pg_total_relation_size(r)) / 1048576.0, 1)
FROM (VALUES ('A', 'issues_a'), ('A', 'issue_cf'), ('B', 'issues_b'), ('C', 'issues_c'), ('C', 'issue_cf')) v(s, r)
GROUP BY s ORDER BY s;
