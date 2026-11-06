-- E9 read queries. SPDX-License-Identifier: GPL-2.0-or-later
-- Format: "-- @<name> <scheme> [runs=N]" then one statement. Lines starting with \set are psql
-- variables shared by every query. Schemes:
--   A  = typed EAV (issues_a + issue_cf, EXISTS semijoins)
--   B  = JSONB, natural syntax: @> (GIN jsonb_path_ops) for equality, ->> casts for range/sort
--   Bx = JSONB through the per-field expression indexes (only fields 1,2,3,4,8 have one)
--   Bp = JSONB via jsonpath @@ (to show GIN cannot serve ranges)
--   C  = hybrid read cache (issues_c.cf_cache); filters in C are identical to A, so only Q9 differs
-- Fields: 1 num hot(95%), 2 date hot(90%), 3 list hot(95%, 5 opts), 4 user hot(85%), 8 num hot(80%),
--         800 num cold, 806 date cold, 807 list cold (41 opts). Coverage: results/distribution.txt
-- Visibility: projects 101..130 (~12% of issues), and private issues only when author = uid(7).
\set VIS 'ARRAY[pid(101),pid(102),pid(103),pid(104),pid(105),pid(106),pid(107),pid(108),pid(109),pid(110),pid(111),pid(112),pid(113),pid(114),pid(115),pid(116),pid(117),pid(118),pid(119),pid(120),pid(121),pid(122),pid(123),pid(124),pid(125),pid(126),pid(127),pid(128),pid(129),pid(130)]'
\set ME 'uid(7)'

---------------------------------------------------------------- Q1 single-field equality
-- @q1_hot_eq_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 4 AND c.v_ref = uid(3));
-- @q1_hot_eq_count B
SELECT count(*) FROM issues_b WHERE fields @> '{"4":"00000000-0001-7000-8000-000000000003"}';
-- @q1_hot_eq_count Bx
SELECT count(*) FROM issues_b WHERE fields->>'4' = '00000000-0001-7000-8000-000000000003';

-- @q1_hot_eq_page A
SELECT i.id FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 4 AND c.v_ref = uid(3)) ORDER BY i.id DESC LIMIT 50;
-- @q1_hot_eq_page B
SELECT id FROM issues_b WHERE fields @> '{"4":"00000000-0001-7000-8000-000000000003"}' ORDER BY id DESC LIMIT 50;
-- @q1_hot_eq_page Bx
SELECT id FROM issues_b WHERE fields->>'4' = '00000000-0001-7000-8000-000000000003' ORDER BY id DESC LIMIT 50;

-- @q1_cold_eq_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 807 AND c.v_text = 'opt0');
-- @q1_cold_eq_count B
SELECT count(*) FROM issues_b WHERE fields @> '{"807":"opt0"}';

-- @q1_cold_eq_page A
SELECT i.id FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 807 AND c.v_text = 'opt0') ORDER BY i.id DESC LIMIT 50;
-- @q1_cold_eq_page B
SELECT id FROM issues_b WHERE fields @> '{"807":"opt0"}' ORDER BY id DESC LIMIT 50;

---------------------------------------------------------------- Q2 range
-- @q2_num_hot_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 8 AND c.v_num BETWEEN 100 AND 110);
-- @q2_num_hot_count Bx
SELECT count(*) FROM issues_b WHERE (fields->>'8')::numeric BETWEEN 100 AND 110;
-- @q2_num_hot_count Bp runs=5
SELECT count(*) FROM issues_b WHERE fields @@ '$."8" >= 100 && $."8" <= 110';

-- @q2_num_hot_page A
SELECT i.id FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 8 AND c.v_num BETWEEN 100 AND 110) ORDER BY i.id DESC LIMIT 50;
-- @q2_num_hot_page Bx
SELECT id FROM issues_b WHERE (fields->>'8')::numeric BETWEEN 100 AND 110 ORDER BY id DESC LIMIT 50;

-- @q2_date_hot_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 2 AND c.v_date BETWEEN '2024-01-01' AND '2024-01-31');
-- @q2_date_hot_count Bx
SELECT count(*) FROM issues_b WHERE fields->>'2' BETWEEN '2024-01-01' AND '2024-01-31';

-- @q2_date_hot_page A
SELECT i.id FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 2 AND c.v_date BETWEEN '2024-01-01' AND '2024-01-31') ORDER BY i.id DESC LIMIT 50;
-- @q2_date_hot_page Bx
SELECT id FROM issues_b WHERE fields->>'2' BETWEEN '2024-01-01' AND '2024-01-31' ORDER BY id DESC LIMIT 50;

-- @q2_num_cold_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 800 AND c.v_num BETWEEN 1000 AND 2000);
-- @q2_num_cold_count B runs=5
SELECT count(*) FROM issues_b WHERE (fields->>'800')::numeric BETWEEN 1000 AND 2000;
-- @q2_num_cold_count Bp runs=5
SELECT count(*) FROM issues_b WHERE fields @@ '$."800" >= 1000 && $."800" <= 2000';

-- @q2_num_cold_page A
SELECT i.id FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 800 AND c.v_num BETWEEN 1000 AND 2000) ORDER BY i.id DESC LIMIT 50;
-- @q2_num_cold_page B runs=5
SELECT id FROM issues_b WHERE (fields->>'800')::numeric BETWEEN 1000 AND 2000 ORDER BY id DESC LIMIT 50;

-- @q2_date_cold_count A
SELECT count(*) FROM issues_a i WHERE EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 806 AND c.v_date BETWEEN '2024-01-01' AND '2024-06-30');
-- @q2_date_cold_count B runs=5
SELECT count(*) FROM issues_b WHERE fields->>'806' BETWEEN '2024-01-01' AND '2024-06-30';

---------------------------------------------------------------- Q3 AND combinations + visibility
-- @q3_hot2_count A
SELECT count(*) FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt1')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 1 AND c.v_num >= 13);
-- @q3_hot2_count B
SELECT count(*) FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt1"}' AND (fields->>'1')::numeric >= 13;

-- @q3_hot2_page A
SELECT i.id FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt1')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 1 AND c.v_num >= 13)
  ORDER BY i.id DESC LIMIT 50;
-- @q3_hot2_page B
SELECT i.id FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt1"}' AND (fields->>'1')::numeric >= 13
  ORDER BY i.id DESC LIMIT 50;

-- @q3_hot_cold_count A
SELECT count(*) FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt1')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 800 AND c.v_num BETWEEN 1000 AND 5000);
-- @q3_hot_cold_count B
SELECT count(*) FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt1"}' AND (fields->>'800')::numeric BETWEEN 1000 AND 5000;

-- @q3_hot_cold_page A
SELECT i.id FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt1')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 800 AND c.v_num BETWEEN 1000 AND 5000)
  ORDER BY i.id DESC LIMIT 50;
-- @q3_hot_cold_page B
SELECT i.id FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt1"}' AND (fields->>'800')::numeric BETWEEN 1000 AND 5000
  ORDER BY i.id DESC LIMIT 50;

-- @q3_three_count A
SELECT count(*) FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt0')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 1 AND c.v_num >= 10)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 2 AND c.v_date >= '2025-01-01');
-- @q3_three_count B
SELECT count(*) FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt0"}' AND (fields->>'1')::numeric >= 10 AND fields->>'2' >= '2025-01-01';

-- @q3_three_page A
SELECT i.id FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 3 AND c.v_text = 'opt0')
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 1 AND c.v_num >= 10)
  AND EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = 2 AND c.v_date >= '2025-01-01')
  ORDER BY i.id DESC LIMIT 50;
-- @q3_three_page B
SELECT i.id FROM issues_b i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND fields @> '{"3":"opt0"}' AND (fields->>'1')::numeric >= 10 AND fields->>'2' >= '2025-01-01'
  ORDER BY i.id DESC LIMIT 50;

---------------------------------------------------------------- Q4 sort by custom field, top 50
-- (inner-join semantics: issues without a value are not listed; see report)
-- @q4_sort_hot_all A
SELECT i.id, c.v_num FROM issue_cf c JOIN issues_a i ON i.id = c.issue_id
  WHERE c.field_id = 8 AND c.v_num IS NOT NULL ORDER BY c.v_num DESC LIMIT 50;
-- @q4_sort_hot_all Bx
SELECT id, (fields->>'8')::numeric FROM issues_b WHERE (fields->>'8')::numeric IS NOT NULL
  ORDER BY (fields->>'8')::numeric DESC LIMIT 50;

-- @q4_sort_hot_vis A
SELECT i.id, c.v_num FROM issue_cf c JOIN issues_a i ON i.id = c.issue_id
  WHERE c.field_id = 8 AND c.v_num IS NOT NULL AND i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  ORDER BY c.v_num DESC LIMIT 50;
-- @q4_sort_hot_vis Bx
SELECT id, (fields->>'8')::numeric FROM issues_b WHERE (fields->>'8')::numeric IS NOT NULL
  AND project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME)
  ORDER BY (fields->>'8')::numeric DESC LIMIT 50;

-- @q4_sort_cold_vis A
SELECT i.id, c.v_num FROM issue_cf c JOIN issues_a i ON i.id = c.issue_id
  WHERE c.field_id = 800 AND c.v_num IS NOT NULL AND i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  ORDER BY c.v_num DESC LIMIT 50;
-- @q4_sort_cold_vis B
SELECT id, (fields->>'800')::numeric FROM issues_b WHERE (fields->>'800')::numeric IS NOT NULL
  AND project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME)
  ORDER BY (fields->>'800')::numeric DESC LIMIT 50;

-- @q4_sort_date_filtered A
SELECT i.id, c.v_date FROM issue_cf c JOIN issues_a i ON i.id = c.issue_id
  WHERE c.field_id = 2 AND c.v_date IS NOT NULL AND i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  AND EXISTS (SELECT 1 FROM issue_cf c3 WHERE c3.issue_id = i.id AND c3.field_id = 3 AND c3.v_text = 'opt1')
  ORDER BY c.v_date DESC LIMIT 50;
-- @q4_sort_date_filtered Bx
SELECT id, fields->>'2' FROM issues_b WHERE fields->>'2' IS NOT NULL
  AND project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME) AND fields @> '{"3":"opt1"}'
  ORDER BY fields->>'2' DESC LIMIT 50;

---------------------------------------------------------------- Q5 group by / sum
-- @q5_group_hot_all A
SELECT v_text, count(*) FROM issue_cf WHERE field_id = 3 AND v_text IS NOT NULL GROUP BY v_text;
-- @q5_group_hot_all B runs=5
SELECT fields->>'3', count(*) FROM issues_b WHERE fields->>'3' IS NOT NULL GROUP BY 1;

-- @q5_group_hot_vis A
SELECT c.v_text, count(*) FROM issues_a i JOIN issue_cf c ON c.issue_id = i.id AND c.field_id = 3 AND c.v_text IS NOT NULL
  WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME) GROUP BY 1;
-- @q5_group_hot_vis B
SELECT fields->>'3', count(*) FROM issues_b
  WHERE fields->>'3' IS NOT NULL AND project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME) GROUP BY 1;

-- @q5_group_cold_all A
SELECT v_text, count(*) FROM issue_cf WHERE field_id = 807 AND v_text IS NOT NULL GROUP BY v_text;
-- @q5_group_cold_all B runs=5
SELECT fields->>'807', count(*) FROM issues_b WHERE fields->>'807' IS NOT NULL GROUP BY 1;

-- @q5_sum_hot_vis A
SELECT sum(c.v_num) FROM issues_a i JOIN issue_cf c ON c.issue_id = i.id AND c.field_id = 8 AND c.v_num IS NOT NULL
  WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME);
-- @q5_sum_hot_vis B
SELECT sum((fields->>'8')::numeric) FROM issues_b WHERE project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME);

-- @q5_group_sum_vis A
SELECT c3.v_text, count(*), sum(c8.v_num) FROM issues_a i
  JOIN issue_cf c3 ON c3.issue_id = i.id AND c3.field_id = 3 AND c3.v_text IS NOT NULL
  LEFT JOIN issue_cf c8 ON c8.issue_id = i.id AND c8.field_id = 8
  WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME) GROUP BY 1;
-- @q5_group_sum_vis B
SELECT fields->>'3', count(*), sum((fields->>'8')::numeric) FROM issues_b
  WHERE fields->>'3' IS NOT NULL AND project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME) GROUP BY 1;

---------------------------------------------------------------- Q9 (extra) render a 50-issue page with all custom fields
-- @q9_page_render A
SELECT i.id, i.subject,
  (SELECT jsonb_object_agg(c.field_id::text, coalesce(to_jsonb(c.v_num), to_jsonb(c.v_date::text), to_jsonb(c.v_text),
                                                      to_jsonb(c.v_ref::text), to_jsonb(c.v_bool)))
     FROM issue_cf c WHERE c.issue_id = i.id)
  FROM issues_a i WHERE i.project_id = ANY (:VIS) AND (NOT i.is_private OR i.author_id = :ME)
  ORDER BY i.id DESC LIMIT 50;
-- @q9_page_render B
SELECT id, subject, fields FROM issues_b WHERE project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME)
  ORDER BY id DESC LIMIT 50;
-- @q9_page_render C
SELECT id, subject, cf_cache FROM issues_c WHERE project_id = ANY (:VIS) AND (NOT is_private OR author_id = :ME)
  ORDER BY id DESC LIMIT 50;
