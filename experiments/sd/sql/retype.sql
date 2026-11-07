-- Custom field type change, in place (field keeps its id), measured on the bench data.
-- SPDX-License-Identifier: GPL-2.0-or-later
-- (a) batched: one short transaction per ~12,500 issues (≈10k values), so no transaction holds an
--     xid for long (a long one would stall the search outbox frontier, see frontier.sh).
-- (b) single statement on another hot field, for comparison.
\timing on
CREATE OR REPLACE PROCEDURE retype_num_to_text(fid int, step int) LANGUAGE plpgsql AS $$
DECLARE lo uuid := '00000000-0000-0000-0000-000000000000'; hi uuid; n bigint; total bigint := 0; b int := 0;
        t0 timestamptz; worst interval := '0'; t_all timestamptz := clock_timestamp();
BEGIN
  LOOP
    SELECT id INTO hi FROM (SELECT id FROM issues WHERE id > lo ORDER BY id LIMIT step) s ORDER BY id DESC LIMIT 1;
    EXIT WHEN hi IS NULL;
    t0 := clock_timestamp();
    EXECUTE format('UPDATE issue_cf SET v_text = v_num::text, v_num = NULL
                    WHERE field_id = %s AND v_num IS NOT NULL AND issue_id > $1 AND issue_id <= $2', fid) USING lo, hi;
    GET DIAGNOSTICS n = ROW_COUNT;
    COMMIT;
    worst := greatest(worst, clock_timestamp() - t0); total := total + n; b := b + 1; lo := hi;
  END LOOP;
  RAISE NOTICE 'field % : % rows in % batches, total %, slowest batch %', fid, total, b, clock_timestamp() - t_all, worst;
END $$;
CALL retype_num_to_text(8, 12500);
CREATE INDEX CONCURRENTLY issue_cf_f8_t ON issue_cf (field_id, v_text, issue_id) WHERE field_id = 8;
DROP INDEX CONCURRENTLY issue_cf_f8;
-- (b) single statement: field 1 (num, 95%)
UPDATE issue_cf SET v_text = v_num::text, v_num = NULL WHERE field_id = 1 AND v_num IS NOT NULL;
SELECT n_dead_tup, n_tup_upd, n_tup_hot_upd FROM pg_stat_user_tables WHERE relname = 'issue_cf';
