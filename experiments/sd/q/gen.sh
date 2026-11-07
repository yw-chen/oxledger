#!/usr/bin/env bash
# Generates the Q3 query variants. S0 = D14 shape (EXISTS semijoins). S1 = conditions wrapped in
# (...) IS TRUE (PG turned them into hashed SubPlans: worse). S2 = each EXISTS gets OFFSET 0,
# which stops PostgreSQL from pulling it up into a semijoin or a hashed SubPlan, so it is a
# per-row correlated probe (PK issue_cf(issue_id, field_id)) while walking issues by id DESC.
cd "$(dirname "$0")"
V="i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)"
P="ORDER BY i.id DESC LIMIT 50"
ex() { echo "EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = $1 AND $2$3)"; }
declare -A C=(
 [three]="3|c.v_ref = oid_(3,0);1|c.v_num >= 10;2|c.v_date >= '2025-01-01'"
 [hot2]="3|c.v_ref = oid_(3,1);1|c.v_num >= 13"
 [hotcold]="3|c.v_ref = oid_(3,0);800|c.v_num BETWEEN 1000 AND 5000"
 [numdate]="8|c.v_num BETWEEN 100 AND 300;2|c.v_date >= '2025-01-01'"
 [rareint]="3|c.v_ref = oid_(3,4);9|c.v_date BETWEEN '2019-01-01' AND '2019-03-31'"
 [num2]="8|c.v_num BETWEEN 100 AND 300;1|c.v_num >= 10"
)
for k in "${!C[@]}"; do
  s0=""; s2=""
  IFS=';' read -ra cs <<<"${C[$k]}"
  for c in "${cs[@]}"; do f=${c%%|*}; t=${c#*|}; s0+=" AND $(ex $f "$t" "")"; s2+=" AND $(ex $f "$t" " OFFSET 0")"; done
  echo "SELECT i.id FROM issues i WHERE $V$s0 $P;" > ${k}_S0.sql
  echo "SELECT i.id FROM issues i WHERE $V$s2 $P;" > ${k}_S2.sql
  echo "SELECT i.id FROM issues i WHERE $V AND (${s0# AND }) IS TRUE $P;" > ${k}_S1.sql
  echo "SELECT count(*) FROM issues i WHERE $V$s0;" > ${k}_count.sql
done
