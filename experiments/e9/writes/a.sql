-- Q6 scheme A: edit one custom field of a random issue (bump lock_version + upsert the value).
\set n random(1, 1000000)
\set v random(0, 100000)
BEGIN;
UPDATE issues_a SET lock_version = lock_version + 1, updated_at = now() WHERE id = nid(:n);
INSERT INTO issue_cf (issue_id, field_id, v_num) VALUES (nid(:n), 8, :v / 100.0)
  ON CONFLICT (issue_id, field_id) DO UPDATE SET v_num = EXCLUDED.v_num;
COMMIT;
