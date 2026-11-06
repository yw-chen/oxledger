-- Scheme A: typed EAV. SPDX-License-Identifier: GPL-2.0-or-later
-- issues_a has no custom-field column; values live in issue_cf, one row per (issue, field),
-- exactly one typed value column non-null (CHECK).

CREATE TABLE issues_a (
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
    updated_at     timestamptz NOT NULL
);
INSERT INTO issues_a
SELECT id, project_id, tracker_id, status_id, subject, author_id, assigned_to_id, is_private,
       lock_version, created_at, updated_at
FROM issues_base ORDER BY n;
CREATE INDEX issues_a_vis ON issues_a (project_id, is_private, author_id);

-- Column order: fixed-width first to limit alignment padding. list values are stored as text
-- (option label); a production schema would likely store an option id (int) instead.
CREATE TABLE issue_cf (
    issue_id  uuid NOT NULL,
    field_id  int NOT NULL,
    v_date    date,
    v_bool    boolean,
    v_ref     uuid,
    v_num     numeric,
    v_text    text,
    PRIMARY KEY (issue_id, field_id),
    CHECK (num_nonnulls(v_num, v_date, v_text, v_ref, v_bool) = 1)
);

INSERT INTO issue_cf (issue_id, field_id, v_date, v_bool, v_ref, v_num, v_text)
SELECT nid(c.n), c.f,
  CASE WHEN d.ftype = 'date' THEN date '2019-01-01' + floor(u * 2900)::int END,
  CASE WHEN d.ftype = 'bool' THEN u < 0.3 END,
  CASE WHEN d.ftype = 'user' THEN uid(1 + floor(500 * u ^ 2)::int) END,
  CASE WHEN d.ftype = 'num' THEN
       CASE WHEN d.id = 1 THEN floor(u * 21)::numeric
            ELSE round((u * CASE WHEN d.id = 8 THEN 1000 ELSE 10000 END)::numeric, 2) END END,
  CASE WHEN d.ftype = 'text' THEN 'txt-' || floor(u * d.card)::int
       WHEN d.ftype = 'list' THEN 'opt' || floor(d.card * u ^ 2)::int END
FROM (SELECT n, f, r(n::bigint * 1024 + f, 3) AS u FROM cf_raw) c
JOIN cf_defs d ON d.id = c.f
ORDER BY c.n, c.f;

ALTER TABLE issue_cf ADD FOREIGN KEY (issue_id) REFERENCES issues_a (id) ON DELETE CASCADE;
ALTER TABLE issue_cf ADD FOREIGN KEY (field_id) REFERENCES cf_defs (id) ON DELETE CASCADE;

-- One partial index per value type: (field_id, value). Rows of other types are not in the index.
CREATE INDEX issue_cf_num  ON issue_cf (field_id, v_num)  WHERE v_num  IS NOT NULL;
CREATE INDEX issue_cf_date ON issue_cf (field_id, v_date) WHERE v_date IS NOT NULL;
CREATE INDEX issue_cf_text ON issue_cf (field_id, v_text) WHERE v_text IS NOT NULL;
CREATE INDEX issue_cf_ref  ON issue_cf (field_id, v_ref)  WHERE v_ref  IS NOT NULL;
CREATE INDEX issue_cf_bool ON issue_cf (field_id, v_bool) WHERE v_bool IS NOT NULL;
