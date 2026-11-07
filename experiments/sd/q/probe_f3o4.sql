SELECT count(*) FROM (SELECT 1 FROM issue_cf WHERE field_id = 3 AND v_ref = oid_(3,4) LIMIT 10001) s;
