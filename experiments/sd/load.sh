#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Loads oxledger_sd_bench (refuses any other name). ~1M issues, ~17.7M issue_cf rows (E9 generator).
set -euo pipefail
C=${PG_CONTAINER:-oxledger-pg}; U=${PGUSER:-oxledger}; DB=${SD_DB:-oxledger_sd_bench}
[[ $DB == oxledger_sd_bench* ]] || { echo "refusing DB $DB"; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
for f in 00_setup 10_load; do
  echo "$(date +%T) $f"; docker exec -i "$C" psql -X -q -v ON_ERROR_STOP=1 -U "$U" -d "$DB" < "$HERE/sql/$f.sql"
done
echo "$(date +%T) done"
