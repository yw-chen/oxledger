-- Q6 scheme C: A's upsert plus keeping the JSONB read cache on the issue row in sync.
\set n random(1, 1000000)
\set v random(0, 100000)
BEGIN;
UPDATE issues_c SET cf_cache = jsonb_set(cf_cache, '{8}', to_jsonb(:v / 100.0)),
       lock_version = lock_version + 1, updated_at = now()
 WHERE id = nid(:n);
INSERT INTO issue_cf (issue_id, field_id, v_num) VALUES (nid(:n), 8, :v / 100.0)
  ON CONFLICT (issue_id, field_id) DO UPDATE SET v_num = EXCLUDED.v_num;
COMMIT;
