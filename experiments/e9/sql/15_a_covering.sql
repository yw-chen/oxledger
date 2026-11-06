-- Scheme A, tuned: replace the (field_id, v_x) partial indexes with covering (field_id, v_x, issue_id)
-- so filters / aggregates over many issues become index-only scans instead of bitmap heap scans over
-- the whole issue_cf heap (hot-field rows are spread over every page, because rows cluster by issue).
-- SPDX-License-Identifier: GPL-2.0-or-later
CREATE INDEX IF NOT EXISTS issue_cf_num_cov  ON issue_cf (field_id, v_num,  issue_id) WHERE v_num  IS NOT NULL;
CREATE INDEX IF NOT EXISTS issue_cf_date_cov ON issue_cf (field_id, v_date, issue_id) WHERE v_date IS NOT NULL;
CREATE INDEX IF NOT EXISTS issue_cf_text_cov ON issue_cf (field_id, v_text, issue_id) WHERE v_text IS NOT NULL;
CREATE INDEX IF NOT EXISTS issue_cf_ref_cov  ON issue_cf (field_id, v_ref,  issue_id) WHERE v_ref  IS NOT NULL;
CREATE INDEX IF NOT EXISTS issue_cf_bool_cov ON issue_cf (field_id, v_bool, issue_id) WHERE v_bool IS NOT NULL;
DROP INDEX IF EXISTS issue_cf_num, issue_cf_date, issue_cf_text, issue_cf_ref, issue_cf_bool;
VACUUM (ANALYZE) issue_cf;  -- index-only scans need an up-to-date visibility map
