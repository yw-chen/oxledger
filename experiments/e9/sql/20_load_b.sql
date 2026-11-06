-- Scheme B: JSONB on the issue row. SPDX-License-Identifier: GPL-2.0-or-later
-- fields = {"<field_id>": value}; num -> JSON number, date -> 'YYYY-MM-DD' string, text/list -> string,
-- user -> uuid string, bool -> JSON boolean. Built from issue_cf so A and B hold identical values.

CREATE TABLE issues_b (
    id             uuid PRIMARY KEY,
    project_id     uuid NOT NULL,
    tracker_id     int NOT NULL,
    status_id      int NOT NULL,
    subject        text NOT NULL,
    author_id      uuid NOT NULL,
    assigned_to_id uuid,
    is_private     boolean NOT NULL,
    lock_version   int NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL,
    updated_at     timestamptz NOT NULL,
    fields         jsonb NOT NULL DEFAULT '{}'
);

INSERT INTO issues_b
SELECT i.id, i.project_id, i.tracker_id, i.status_id, i.subject, i.author_id, i.assigned_to_id,
       i.is_private, i.lock_version, i.created_at, i.updated_at, coalesce(f.fields, '{}')
FROM issues_a i
LEFT JOIN (
    SELECT issue_id,
           jsonb_object_agg(field_id::text,
               coalesce(to_jsonb(v_num), to_jsonb(v_date::text), to_jsonb(v_text),
                        to_jsonb(v_ref::text), to_jsonb(v_bool))) AS fields
    FROM issue_cf GROUP BY issue_id
) f ON f.issue_id = i.id
ORDER BY i.id;

CREATE INDEX issues_b_vis ON issues_b (project_id, is_private, author_id);
-- Containment (@>) and jsonpath (@?, @@) on any field. jsonb_path_ops: smaller than jsonb_ops,
-- but no key-existence (?) support; neither opclass supports ranges or ordering.
CREATE INDEX issues_b_gin ON issues_b USING gin (fields jsonb_path_ops);
-- Expression indexes for the "hot" fields the benchmark filters/sorts on. Dates compare as ISO text
-- because text::date is not IMMUTABLE (depends on DateStyle) and cannot be indexed directly.
CREATE INDEX issues_b_f1 ON issues_b (((fields->>'1')::numeric));  -- story points (num)
CREATE INDEX issues_b_f2 ON issues_b ((fields->>'2'));             -- target date (ISO text)
CREATE INDEX issues_b_f3 ON issues_b ((fields->>'3'));             -- severity (list)
CREATE INDEX issues_b_f4 ON issues_b ((fields->>'4'));             -- reviewer (user uuid text)
CREATE INDEX issues_b_f8 ON issues_b (((fields->>'8')::numeric));  -- estimate (num)
