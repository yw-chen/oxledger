-- D14-shaped typed EAV on the E9 data (same generator, sql/00_setup.sql copied from experiments/e9).
-- SPDX-License-Identifier: GPL-2.0-or-later
-- Differences from E9 scheme A, matching migrations/0002_custom_fields.sql:
--   * list values are option ids in v_ref (E9 stored the label in v_text);
--   * one covering partial index per field: (field_id, v_x, issue_id) WHERE field_id = N
--     (only built for the fields the queries use; other fields' indexes do not affect these plans);
--   * CREATE STATISTICS (mcv) ON field_id, v_ref;
--   * issues has the production visibility index.
CREATE FUNCTION oid_(f int, k int) RETURNS uuid LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT ('00000000-0002-7000-8' || lpad(to_hex(f), 3, '0') || '-' || lpad(to_hex(k), 12, '0'))::uuid $$;

CREATE TABLE issues (
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
INSERT INTO issues
SELECT id, project_id, tracker_id, status_id, subject, author_id, assigned_to_id, is_private,
       lock_version, created_at, updated_at
FROM issues_base ORDER BY n;
CREATE INDEX issues_visibility_idx ON issues (project_id, tracker_id, is_private, author_id, assigned_to_id);

CREATE TABLE issue_cf (
    issue_id  uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    field_id  int  NOT NULL,
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
  CASE WHEN d.ftype = 'user' THEN uid(1 + floor(500 * u ^ 2)::int)
       WHEN d.ftype = 'list' THEN oid_(d.id, floor(d.card * u ^ 2)::int) END,
  CASE WHEN d.ftype = 'num' THEN
       CASE WHEN d.id = 1 THEN floor(u * 21)::numeric
            ELSE round((u * CASE WHEN d.id = 8 THEN 1000 ELSE 10000 END)::numeric, 2) END END,
  CASE WHEN d.ftype = 'text' THEN 'txt-' || floor(u * d.card)::int END
FROM (SELECT n, f, r(n::bigint * 1024 + f, 3) AS u FROM cf_raw) c
JOIN cf_defs d ON d.id = c.f
ORDER BY c.n, c.f;

CREATE INDEX issue_cf_f1   ON issue_cf (field_id, v_num,  issue_id) WHERE field_id = 1;
CREATE INDEX issue_cf_f2   ON issue_cf (field_id, v_date, issue_id) WHERE field_id = 2;
CREATE INDEX issue_cf_f3   ON issue_cf (field_id, v_ref,  issue_id) WHERE field_id = 3;
CREATE INDEX issue_cf_f4   ON issue_cf (field_id, v_ref,  issue_id) WHERE field_id = 4;
CREATE INDEX issue_cf_f8   ON issue_cf (field_id, v_num,  issue_id) WHERE field_id = 8;
CREATE INDEX issue_cf_f9   ON issue_cf (field_id, v_date, issue_id) WHERE field_id = 9;
CREATE INDEX issue_cf_f800 ON issue_cf (field_id, v_num,  issue_id) WHERE field_id = 800;
CREATE INDEX issue_cf_f806 ON issue_cf (field_id, v_date, issue_id) WHERE field_id = 806;
CREATE INDEX issue_cf_f807 ON issue_cf (field_id, v_ref,  issue_id) WHERE field_id = 807;
CREATE STATISTICS issue_cf_field_ref (mcv) ON field_id, v_ref FROM issue_cf;
DROP TABLE cf_raw;
VACUUM (ANALYZE) issues, issue_cf;
