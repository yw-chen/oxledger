#!/usr/bin/env bash
# E9 custom-field storage benchmark. SPDX-License-Identifier: GPL-2.0-or-later
# Usage: ./run.sh [all|env|load|sizes [suffix]|reads [tag]|covering|writes|ddl]   (default: all)
# Talks to PostgreSQL through `docker exec` into $PG_CONTAINER, so no local psql is needed.
set -euo pipefail
export MSYS_NO_PATHCONV=1   # Git Bash on Windows: don't mangle /dev/null etc. in docker args

HERE="$(cd "$(dirname "$0")" && pwd)"
PG_CONTAINER="${PG_CONTAINER:-oxledger-pg}"
PGUSER="${PGUSER:-oxledger}"
DB="${E9_DB:-oxledger_e9}"
RUNS="${E9_RUNS:-15}"          # timed runs per read query (after 1 warm-up)
WRITE_SECS="${E9_WRITE_SECS:-20}"
OUT="$HERE/results"
mkdir -p "$OUT"

case "$DB" in oxledger_e9*) ;; *) echo "refusing to touch database '$DB' (must start with oxledger_e9)"; exit 1;; esac

psql_db() { docker exec -i "$PG_CONTAINER" psql -X -q -v ON_ERROR_STOP=1 -U "$PGUSER" -d "$DB" "$@"; }
psql_admin() { docker exec -i "$PG_CONTAINER" psql -X -q -v ON_ERROR_STOP=1 -U "$PGUSER" -d postgres "$@"; }
log() { echo "[$(date +%H:%M:%S)] $*" >&2; }

# ---------------------------------------------------------------- env
do_env() {
  {
    echo "# environment"
    psql_admin -At <<'SQL'
SELECT 'version: ' || version();
SELECT name || ' = ' || setting || coalesce(' ' || unit, '') FROM pg_settings
 WHERE name IN ('shared_buffers','work_mem','maintenance_work_mem','effective_cache_size','jit',
                'max_parallel_workers_per_gather','random_page_cost','default_statistics_target',
                'synchronous_commit','wal_level','autovacuum') ORDER BY name;
SQL
    echo "container cpus: $(docker exec "$PG_CONTAINER" nproc)"
    echo "container mem:  $(docker exec "$PG_CONTAINER" sh -c "free -m | awk '/Mem/{print \$2\" MB\"}'")"
    echo "docker server:  $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo '?')"
  } | tee "$OUT/env.txt"
}

# ---------------------------------------------------------------- load
do_load() {
  log "recreating database $DB"
  psql_admin -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB"
  for f in 00_setup 10_load_a 20_load_b 30_load_c; do
    log "loading $f"
    local t0=$SECONDS
    psql_db < "$HERE/sql/$f.sql"
    log "  $f done in $((SECONDS - t0)) s"
  done
  log "VACUUM ANALYZE"
  psql_db -c "DROP TABLE cf_raw" -c "VACUUM (ANALYZE)"
  psql_db -At < "$HERE/sql/40_distribution.sql" | tee "$OUT/distribution.txt"
}

# ---------------------------------------------------------------- reads
# queries.sql blocks:  -- @<name> <scheme> [runs=N]   followed by one SQL statement ending in ';'
# All schemes of one query are interleaved (A1 B1 A2 B2 ...) so background noise on the host hits
# them equally. Iteration 0 is an untimed warm-up.
do_reads() {  # $1 = tag for output files (base = A with plain partial indexes, covering = after 15_a_covering.sql)
  local tag="${1:-base}" script="$OUT/.reads.psql"
  mkdir -p "$OUT/explain/$tag"
  awk -v runs="$RUNS" '
    function flush_group(   i, k, maxn) {
      if (ng == 0) return
      maxn = 0; for (k = 1; k <= ng; k++) if (gn[k] > maxn) maxn = gn[k]
      for (i = 0; i <= maxn; i++)
        for (k = 1; k <= ng; k++)
          if (i <= gn[k]) printf "\\echo @@T %s %s %d\n%s\n", gname, gs[k], i, gsql[k]
      ng = 0
    }
    function end_block() {
      if (name == "") return
      if (name != gname) { flush_group(); gname = name }
      ng++; gs[ng] = scheme; gsql[ng] = sql; gn[ng] = (opt ~ /^runs=/) ? substr(opt, 6) + 0 : runs
      name = ""
    }
    /^\\set/ { print; next }
    /^-- @/ { end_block(); name = substr($2, 2); scheme = $3; opt = $4; sql = ""; next }
    /^--/ || /^[[:space:]]*$/ { next }
    { sql = sql (sql == "" ? "" : "\n") $0 }
    END { end_block(); flush_group() }
  ' "$HERE/queries.sql" > "$script"
  { echo '\o /dev/null'; echo '\timing on'; cat "$script"; } > "$script.full"
  log "running read queries ($RUNS timed runs each, schemes interleaved)"
  psql_db < "$script.full" > "$OUT/.reads_$tag.raw"
  awk '
    /^@@T/   { key = $2 "\t" $3; it = $4 + 0; if (!(key in seen)) { seen[key] = 1; order[++nk] = key }; next }
    /^Time:/ { if (it > 0) { c[key]++; t[key, c[key]] = $2 }; next }
    END {
      for (q = 1; q <= nk; q++) {
        key = order[q]; n = c[key]
        for (i = 1; i <= n; i++) a[i] = t[key, i]
        for (i = 2; i <= n; i++) { v = a[i]; j = i - 1; while (j > 0 && a[j] > v) { a[j+1] = a[j]; j-- } a[j+1] = v }
        med = (n % 2) ? a[(n+1)/2] : (a[n/2] + a[n/2+1]) / 2
        p95 = a[int(0.95 * n + 0.999999)]
        printf "%s\t%d\t%.3f\t%.3f\t%.3f\n", key, n, med, p95, a[1]
      }
    }
  ' "$OUT/.reads_$tag.raw" | { printf "query\tscheme\truns\tmedian_ms\tp95_ms\tmin_ms\n"; cat; } | tee "$OUT/reads_$tag.tsv"

  log "capturing EXPLAIN (ANALYZE, BUFFERS)"
  local sets; sets=$(grep '^\\set' "$HERE/queries.sql")
  awk '
    /^\\set/ { next }
    /^-- @/ { if (name != "") print name "\t" scheme "\t" sql; name = substr($2, 2); scheme = $3; sql = ""; next }
    /^--/ || /^[[:space:]]*$/ { next }
    { sql = sql " " $0 }
    END { if (name != "") print name "\t" scheme "\t" sql }
  ' "$HERE/queries.sql" | while IFS=$'\t' read -r name scheme sql; do
    printf '%s\nEXPLAIN (ANALYZE, BUFFERS) %s\n' "$sets" "$sql" | psql_db -At > "$OUT/explain/$tag/${name}_${scheme}.txt"
  done
}

# ---------------------------------------------------------------- A covering indexes
do_covering() { log "A: switching to covering partial indexes"; psql_db < "$HERE/sql/15_a_covering.sql"; }

# ---------------------------------------------------------------- ddl (Q7)
do_ddl() {
  log "field delete / retype (each run inside BEGIN..ROLLBACK, VACUUM between runs)"
  psql_db < "$HERE/ddl.sql" > "$OUT/.ddl.raw"
  awk '
    /^@@BEGIN/ { name = $2; scheme = $3; n = 0; next }
    /^Time:/   { t[++n] = $2; next }
    /^@@END/   {
      for (i = 2; i <= n; i++) { v = t[i]; j = i - 1; while (j > 0 && t[j] > v) { t[j+1] = t[j]; j-- } t[j+1] = v }
      med = (n % 2) ? t[(n+1)/2] : (t[n/2] + t[n/2+1]) / 2
      printf "%s\t%s\t%d\t%.1f\t%.1f\t%.1f\n", name, scheme, n, med, t[n], t[1]; delete t
    }
  ' "$OUT/.ddl.raw" | { printf "op\tscheme\truns\tmedian_ms\tmax_ms\tmin_ms\n"; cat; } | tee "$OUT/ddl.tsv"
}

# ---------------------------------------------------------------- writes (Q6)
table_stats() {  # $1 = label
  psql_db -At -F $'\t' -c "
    SELECT '$1', relname, n_tup_upd, n_tup_hot_upd, n_tup_ins, n_dead_tup,
           pg_relation_size(relid), pg_indexes_size(relid), pg_total_relation_size(relid)
    FROM pg_stat_user_tables WHERE relname IN ('issues_a','issue_cf','issues_b','issues_c') ORDER BY relname"
}
do_writes() {
  log "pgbench writes, ${WRITE_SECS}s per run"
  psql_db -c "ALTER TABLE issues_a SET (autovacuum_enabled = off)" -c "ALTER TABLE issue_cf SET (autovacuum_enabled = off)" \
          -c "ALTER TABLE issues_b SET (autovacuum_enabled = off)" -c "ALTER TABLE issues_c SET (autovacuum_enabled = off)"
  : > "$OUT/writes.tsv"; : > "$OUT/writes_stats.tsv"
  printf "scheme\tsync_commit\tclients\ttps\tlat_avg_ms\ttransactions\twal_bytes_per_tx\n" > "$OUT/writes.tsv"
  printf "label\trelname\tn_tup_upd\tn_tup_hot_upd\tn_tup_ins\tn_dead_tup\theap_bytes\tindex_bytes\ttotal_bytes\n" > "$OUT/writes_stats.tsv"
  psql_db -c "VACUUM (ANALYZE) issues_a, issue_cf, issues_b, issues_c" -c "SELECT pg_stat_force_next_flush()" >/dev/null
  sleep 2; table_stats "before" >> "$OUT/writes_stats.tsv"
  for s in a b c; do
    docker exec -i "$PG_CONTAINER" sh -c "cat > /tmp/e9_write_$s.sql" < "$HERE/writes/$s.sql"
    for sync in on off; do
      for c in 1 8; do
        log "  scheme $s, synchronous_commit=$sync, $c client(s)"
        local o lsn0 lsn1 tps lat tx wal
        lsn0=$(psql_db -At -c "SELECT pg_current_wal_lsn()")
        o=$(docker exec -e PGOPTIONS="-c synchronous_commit=$sync" "$PG_CONTAINER" pgbench -n -M prepared -U "$PGUSER" \
              -c "$c" -j "$c" -T "$WRITE_SECS" -f "/tmp/e9_write_$s.sql" "$DB" 2>&1)
        lsn1=$(psql_db -At -c "SELECT pg_current_wal_lsn()")
        echo "$o" > "$OUT/.pgbench_${s}_sync${sync}_c$c.txt"
        tps=$(echo "$o" | awk '/^tps/ {print $3; exit}')
        lat=$(echo "$o" | awk '/latency average/ {print $4; exit}')
        tx=$(echo "$o" | awk '/number of transactions actually processed/ {print $NF; exit}')
        wal=$(psql_db -At -c "SELECT round(pg_wal_lsn_diff('$lsn1', '$lsn0') / greatest($tx, 1))")
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$s" "$sync" "$c" "$tps" "$lat" "$tx" "$wal" | tee -a "$OUT/writes.tsv"
      done
    done
    sleep 2; table_stats "after_$s" >> "$OUT/writes_stats.tsv"
  done
  log "VACUUM after writes"
  psql_db -c "VACUUM issues_a, issue_cf, issues_b, issues_c" >/dev/null; sleep 2
  table_stats "after_vacuum" >> "$OUT/writes_stats.tsv"
  psql_db -c "ALTER TABLE issues_a RESET (autovacuum_enabled)" -c "ALTER TABLE issue_cf RESET (autovacuum_enabled)" \
          -c "ALTER TABLE issues_b RESET (autovacuum_enabled)" -c "ALTER TABLE issues_c RESET (autovacuum_enabled)"
  cat "$OUT/writes_stats.tsv"
}

# ---------------------------------------------------------------- sizes (Q8)
do_sizes() { psql_db -At -F $'\t' < "$HERE/sql/50_sizes.sql" | tee "$OUT/sizes${1:-}.tsv"; }

case "${1:-all}" in
  env) do_env ;;
  load) do_load ;;
  reads) do_reads "${2:-base}" ;;
  covering) do_covering ;;
  ddl) do_ddl ;;
  writes) do_writes ;;
  sizes) do_sizes "${2:-}" ;;
  # writes before ddl: the ddl step rewrites ~1M issues_b rows (rolled back) and would bloat B first.
  all) do_env; do_load; do_sizes _base; do_reads base; do_covering; do_sizes _covering; do_reads covering
       do_writes; do_sizes _after_writes; do_ddl ;;
  *) echo "unknown step $1"; exit 1 ;;
esac
log "done: ${1:-all}"
