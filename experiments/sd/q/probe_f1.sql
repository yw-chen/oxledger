SELECT count(*) FROM (SELECT 1 FROM issue_cf WHERE field_id = 1 AND v_num >= 10 LIMIT 10001) s;
