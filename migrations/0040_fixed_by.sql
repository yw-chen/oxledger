-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 06 §2.6, §3.8 (R-F1–R-F11): `fixed_by`. A project's managers set
-- "keyword → target status" rules (off by default); a pull request merged
-- into the default branch whose title, body or commits say `fixes AUTH-1`
-- moves the issue, as the integration acting for the rule's sponsor. A
-- direct push to the default branch only leaves a hint (decision A3 (i)).

CREATE TABLE project_fix_rules (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    enabled      boolean NOT NULL DEFAULT false,
    sponsor_id   uuid NOT NULL REFERENCES principals(id),   -- who last enabled or changed it
    keywords     text[] NOT NULL CHECK (cardinality(keywords) BETWEEN 1 AND 10),   -- lower case
    target_status_id uuid NOT NULL REFERENCES issue_statuses(id),
    tracker_ids  uuid[],                        -- NULL = every tracker
    disabled_reason text CHECK (disabled_reason IN ('sponsor_inactive', 'sponsor_lost_permission',
                                                    'status_removed', 'manual')),
    lock_version bigint NOT NULL DEFAULT 0,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Not in the contract's DDL: the order on the settings page.
    position     smallint NOT NULL DEFAULT 0
);
CREATE INDEX project_fix_rules_project_idx ON project_fix_rules (project_id, position);
CREATE TRIGGER project_fix_rules_sponsor_guard BEFORE INSERT OR UPDATE OF sponsor_id ON project_fix_rules
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('sponsor_id');
-- At most 5 per project and no keyword twice in a project: checked in the
-- transaction that holds the project's settings lock (02 §19.7).

-- Not in the contract's DDL: the whole list is replaced under one version
-- (PUT /projects/{id}/fix-rules, `lock_version`).
ALTER TABLE project_tab_settings ADD COLUMN fix_rules_version bigint NOT NULL DEFAULT 0;

CREATE TABLE issue_fix_links (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    issue_id    uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id  uuid NOT NULL,
    repo_id     uuid NOT NULL REFERENCES github_repos(id),
    pr_id       uuid REFERENCES github_pull_requests(id) ON DELETE CASCADE,
    sha         text,
    link_id     uuid NOT NULL REFERENCES project_repo_links(id),
    rule_id     uuid REFERENCES project_fix_rules(id) ON DELETE SET NULL,   -- NULL = no enabled rule (a hint only)
    keyword     text NOT NULL,
    confirm_head text,                          -- push source: the push's `after` sha
    state       text NOT NULL CHECK (state IN ('pending', 'applied', 'skipped', 'suggested')),
    reason      text,                           -- why it was skipped or only suggested (R-F6)
    execution_id uuid REFERENCES execution_refs(id),
    attempts    smallint NOT NULL DEFAULT 0,
    next_at     timestamptz NOT NULL DEFAULT now(),
    detected_at timestamptz NOT NULL DEFAULT now(),
    decided_at  timestamptz,
    CHECK (num_nonnulls(pr_id, sha) = 1),
    CHECK (sha IS NULL OR sha ~ '^[0-9a-f]{40}$')
);
CREATE UNIQUE INDEX issue_fix_links_pr_uq ON issue_fix_links (issue_id, pr_id) WHERE pr_id IS NOT NULL;
CREATE UNIQUE INDEX issue_fix_links_sha_uq ON issue_fix_links (issue_id, repo_id, sha) WHERE sha IS NOT NULL;
CREATE INDEX issue_fix_links_pending_idx ON issue_fix_links (next_at) WHERE state = 'pending';
CREATE INDEX issue_fix_links_link_idx ON issue_fix_links (link_id);
CREATE INDEX issue_fix_links_pr_idx ON issue_fix_links (pr_id) WHERE pr_id IS NOT NULL;
CREATE INDEX issue_fix_links_project_idx ON issue_fix_links (project_id, state);

ALTER TABLE project_setting_events DROP CONSTRAINT project_setting_events_kind_check;
ALTER TABLE project_setting_events ADD CONSTRAINT project_setting_events_kind_check
    CHECK (kind IN ('tabs_updated', 'tabs_reset', 'repo_linked', 'repo_unlinked',
                    'pr_hidden', 'pr_unhidden', 'github_content_revoked',
                    'triage_settings_updated', 'wiki_start_page',
                    'commit_hidden', 'commit_unhidden', 'fix_rule_changed'));
