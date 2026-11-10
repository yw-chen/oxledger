-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 05 §2.6 (M1.5): issue forms. L1 condition rules and L2 layouts
-- per tracker, at site level (project_id NULL) or for one project, each
-- with its own draft → impact → publish history; marks for issues that do
-- not satisfy rules published with accept_and_mark; and the effective rules
-- version a proposal was created under.
--
-- No role, permission or membership changes: security_epoch does not move.
-- No fixed-list CHECK of an existing table is rebuilt.

CREATE TABLE form_defs (
    id                       uuid PRIMARY KEY DEFAULT uuidv7(),
    tracker_id               uuid NOT NULL REFERENCES trackers(id),
    project_id               uuid REFERENCES projects(id),             -- NULL = site level
    published_rules_version  integer NOT NULL DEFAULT 0 CHECK (published_rules_version >= 0),  -- 0 = no rules
    published_layout_version integer NOT NULL DEFAULT 0 CHECK (published_layout_version >= 0), -- 0 = default layout
    lock_version             integer NOT NULL DEFAULT 0,
    created_at               timestamptz NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (tracker_id, project_id)
);

CREATE TABLE form_rule_versions (
    form_id      uuid NOT NULL REFERENCES form_defs(id),
    version      integer NOT NULL CHECK (version >= 1),
    state        text NOT NULL CHECK (state IN ('draft','published','superseded','discarded')),
    body         jsonb NOT NULL,                  -- §3.6.2
    publish_mode text CHECK (publish_mode IN ('block','accept_and_mark')),
    impact       jsonb,                           -- counts and ids only, never field values (02 §7.5)
    created_by   uuid NOT NULL REFERENCES principals(id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES principals(id),
    published_at timestamptz,
    PRIMARY KEY (form_id, version)
);
CREATE UNIQUE INDEX form_rule_versions_one_draft ON form_rule_versions (form_id) WHERE state = 'draft';

-- LIKE copies the columns, CHECKs, primary key and the one-draft index, not
-- the foreign keys (R23): added below.
CREATE TABLE form_layout_versions (LIKE form_rule_versions INCLUDING ALL);
ALTER TABLE form_layout_versions ADD FOREIGN KEY (form_id) REFERENCES form_defs(id),
    ADD FOREIGN KEY (created_by) REFERENCES principals(id),
    ADD FOREIGN KEY (published_by) REFERENCES principals(id),
    ADD CONSTRAINT form_layout_versions_no_mode CHECK (publish_mode IS NULL);

-- Issues that broke rules published with accept_and_mark. Only rule ids,
-- never values. A later write that passes the checks clears them.
CREATE TABLE form_rule_marks (
    issue_id      uuid NOT NULL REFERENCES issues(id),
    form_id       uuid NOT NULL REFERENCES form_defs(id),
    rules_version integer NOT NULL,
    rule_ids      text[] NOT NULL,
    marked_at     timestamptz NOT NULL DEFAULT now(),
    cleared_at    timestamptz,
    PRIMARY KEY (issue_id, form_id, rules_version)
);
CREATE INDEX form_rule_marks_open ON form_rule_marks (issue_id) WHERE cleared_at IS NULL;

-- The effective rules version (r<site>.<project>.s<L0>.w<workflow>) of the
-- targets when the proposal version was stored: the review queue flags a
-- proposal whose rules changed since (§3.6.3). NULL for versions stored
-- before 0024.
ALTER TABLE changeset_versions ADD COLUMN rules_version text;

-- §2.6: a form change moves describe's schema_version (a hint, 02 §5.1).
CREATE TRIGGER form_defs_schema_version AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON form_defs
    FOR EACH STATEMENT EXECUTE FUNCTION bump_schema_version();
