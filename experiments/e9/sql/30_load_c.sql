-- Scheme C (hybrid): A's issue_cf is authoritative (filters, sorts, aggregates go there);
-- issues_c.cf_cache is a denormalised JSONB copy used only for reading/rendering an issue.
-- No index on cf_cache. SPDX-License-Identifier: GPL-2.0-or-later

CREATE TABLE issues_c (
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
    cf_cache       jsonb NOT NULL DEFAULT '{}'
);
INSERT INTO issues_c SELECT * FROM issues_b ORDER BY id;
CREATE INDEX issues_c_vis ON issues_c (project_id, is_private, author_id);
