#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
# P2.5 (2026-11-09): the Q3 variants with the product's live order, ORDER BY
# updated_at DESC, id DESC, against the index issues (updated_at DESC, id DESC)
# that migration 0018 adds (sql/20_updated_idx.sql). Writes q/<name>_u_S0.sql
# and q/<name>_u_S2.sql from the S0/S2 files gen.sh wrote.
cd "$(dirname "$0")"
for k in three hot2 hotcold numdate rareint num2; do
  for s in S0 S2; do
    sed 's/ORDER BY i.id DESC LIMIT 50/ORDER BY i.updated_at DESC, i.id DESC LIMIT 50/' ${k}_$s.sql > ${k}_u_$s.sql
  done
done
