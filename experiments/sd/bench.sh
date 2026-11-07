#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# Times one query file: bench.sh <name> <runs> < query.sql  → prints name, median, p95(max), min (ms) and saves EXPLAIN.
set -euo pipefail
C=${PG_CONTAINER:-oxledger-pg}; U=${PGUSER:-oxledger}; DB=oxledger_sd_bench
HERE=$(cd "$(dirname "$0")" && pwd); name=$1; runs=${2:-15}
sql=$(cat); mkdir -p "$HERE/results/explain"
pre="$(cat "$HERE/sql/q_common.psql")
${SETS:-}"
{ echo "$pre"; echo '\timing on'; for i in $(seq 0 "$runs"); do echo "$sql"; done; } \
 | docker exec -i "$C" psql -X -q -U "$U" -d "$DB" -o /dev/null 2>&1 | grep -o 'Time: [0-9.]*' | awk '{print $2}' | tail -n +2 \
 | sort -n | awk -v n="$name" '{a[NR]=$1} END{printf "%s\t%.2f\t%.2f\t%.2f\n", n, a[int((NR+1)/2)], a[NR], a[1]}'
{ echo "$pre"; echo "EXPLAIN (ANALYZE, BUFFERS) $sql"; } | docker exec -i "$C" psql -X -q -A -t -U "$U" -d "$DB" > "$HERE/results/explain/$name.txt"
