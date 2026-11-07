#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Does CREATE INDEX CONCURRENTLY (D14 per-field index build) hold back the outbox safe frontier?
# Builds a full-table index on issue_cf (17.7M rows, long enough to observe) and, every 0.5 s, commits a probe write and
# records whether the previous probe's xid is already below pg_snapshot_xmin(pg_current_snapshot()).
set -uo pipefail
C=${PG_CONTAINER:-oxledger-pg}; U=${PGUSER:-oxledger}; DB=oxledger_sd_bench
q() { docker exec -i "$C" psql -X -q -A -t -U "$U" -d $DB -c "$1"; }
q "DROP INDEX IF EXISTS issue_cf_cic_test" >/dev/null
start=$(date +%s%3N)
(q "CREATE INDEX CONCURRENTLY issue_cf_cic_test ON issue_cf (v_num, v_date, issue_id)" >/dev/null; echo "CIC done at +$(( $(date +%s%3N) - start ))ms") &
bg=$!
prev=""
while kill -0 $bg 2>/dev/null; do
  w=$(q "INSERT INTO fr_probe DEFAULT VALUES RETURNING xid")
  if [[ -n $prev ]]; then
    r=$(q "SELECT ('$prev'::xid8 < pg_snapshot_xmin(pg_current_snapshot()))::text || ' ' || coalesce((SELECT string_agg(left(query,30)||' xid='||backend_xid, '; ') FROM pg_stat_activity WHERE backend_xid IS NOT NULL AND pid <> pg_backend_pid()), '-')")
    printf "+%6dms prev_probe_visible=%s
" "$(( $(date +%s%3N) - start ))" "$r"
  fi
  prev=$w; sleep 0.2
done
wait $bg
