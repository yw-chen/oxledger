#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Which open transactions hold back pg_snapshot_xmin(pg_current_snapshot()) — the
# D11 outbox "safe frontier" — as seen from database oxledger_sd_bench?
# For each case: start a holder transaction (in background, sleeping 6 s), then commit a
# write in oxledger_sd_bench and check whether its xid is already below the frontier.
set -euo pipefail
C=${PG_CONTAINER:-oxledger-pg}; U=${PGUSER:-oxledger}; DB=oxledger_sd_bench
psql_() { docker exec -i "$C" psql -U "$U" -X -q -A -t -d "$1"; }
psql_ $DB <<<"CREATE TABLE IF NOT EXISTS fr_probe (id bigserial PRIMARY KEY, xid xid8 NOT NULL DEFAULT pg_current_xact_id());"
case_() {  # name, holder db, holder sql (runs inside BEGIN ... pg_sleep(6) ... COMMIT)
  local name=$1 hdb=$2 hsql=$3
  (printf 'BEGIN ISOLATION LEVEL REPEATABLE READ;\n%s\nSELECT pg_sleep(6);\nCOMMIT;\n' "$hsql" | psql_ "$hdb" >/dev/null) &
  local bg=$!
  sleep 1.5
  local w; w=$(psql_ $DB <<<"INSERT INTO fr_probe DEFAULT VALUES RETURNING xid;")
  local r; r=$(psql_ $DB <<<"SELECT pg_snapshot_xmin(pg_current_snapshot()) || ' ' || ('$w'::xid8 < pg_snapshot_xmin(pg_current_snapshot()));")
  wait $bg
  local after; after=$(psql_ $DB <<<"SELECT '$w'::xid8 < pg_snapshot_xmin(pg_current_snapshot());")
  printf '%-44s write_xid=%s frontier=%s visible_during=%s visible_after=%s\n' "$name" "$w" "${r% *}" "${r#* }" "$after"
}
case_ "none (baseline)"                       postgres "SELECT 1;"
case_ "other db, read-only snapshot (no xid)"  postgres "SELECT count(*) FROM pg_class;"
case_ "other db, xid assigned"                postgres "SELECT pg_current_xact_id();"
case_ "same db, read-only snapshot (no xid)"   $DB      "SELECT count(*) FROM fr_probe;"
case_ "same db, xid assigned"                 $DB      "SELECT pg_current_xact_id();"
