-- Q6 scheme B: edit one custom field of a random issue (jsonb_set rewrites the whole fields value).
\set n random(1, 1000000)
\set v random(0, 100000)
UPDATE issues_b SET fields = jsonb_set(fields, '{8}', to_jsonb(:v / 100.0)),
       lock_version = lock_version + 1, updated_at = now()
 WHERE id = nid(:n);
