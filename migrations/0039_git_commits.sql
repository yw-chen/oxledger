-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 06 §2.5, §3.7 (R-C1–R-C11): commits from GitHub `push` events and
-- the issues their messages name. Read-only as before (U45a): nothing is
-- written back to GitHub.

-- R-C2: every event that carries a `repository` object updates it; NULL =
-- not known yet.
ALTER TABLE github_repos ADD COLUMN default_branch text;

-- §6.10: the admin page shows when an installation last sent a `push`
-- (NULL = never: the App is not subscribed to push yet).
ALTER TABLE github_installations ADD COLUMN last_push_at timestamptz;

CREATE TABLE github_commits (
    repo_id      uuid NOT NULL REFERENCES github_repos(id),
    sha          text NOT NULL CHECK (sha ~ '^[0-9a-f]{40}$'),
    message      text NOT NULL CHECK (char_length(message) <= 4096),   -- cut; the first line is shown
    author_login text,           -- the commit's author.username: NOT verified (anyone can write any author)
    sender_login text,           -- sender.login of the push that first brought it (verified by GitHub)
    committed_at timestamptz,
    -- No html_url: built on read from github_repos.full_name and the sha.
    on_default_branch boolean NOT NULL DEFAULT false,
    first_ref    text,           -- the branch it was first seen on (cut to 255)
    first_seen   timestamptz NOT NULL DEFAULT now(),
    last_seen    timestamptz NOT NULL DEFAULT now(),
    redacted_at  timestamptz,    -- after a revoke: message = '[已遮蔽]'; an upsert never writes it back (R-C3)
    PRIMARY KEY (repo_id, sha)
);

CREATE TABLE commit_issue_refs (
    repo_id    uuid NOT NULL,
    sha        text NOT NULL,
    issue_id   uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id uuid NOT NULL,
    link_id    uuid NOT NULL REFERENCES project_repo_links(id),
    first_seen timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, sha, issue_id),
    FOREIGN KEY (repo_id, sha) REFERENCES github_commits (repo_id, sha) ON DELETE CASCADE
);
CREATE INDEX commit_issue_refs_issue_idx ON commit_issue_refs (issue_id);
CREATE INDEX commit_issue_refs_link_idx ON commit_issue_refs (link_id);

CREATE TABLE project_commit_hidden (          -- hidden in one project only (as project_pr_hidden)
    project_id uuid NOT NULL REFERENCES projects(id),
    repo_id    uuid NOT NULL,
    sha        text NOT NULL,
    hidden_by  uuid NOT NULL REFERENCES principals(id),
    hidden_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, repo_id, sha),
    FOREIGN KEY (repo_id, sha) REFERENCES github_commits (repo_id, sha) ON DELETE CASCADE
);

-- R-C7: the tombstone of a revoked commit. Kept for good and not tied to
-- the commit row (no FK to it): the same sha pushed again builds no ref in
-- this project.
CREATE TABLE project_commit_revocations (
    project_id uuid NOT NULL REFERENCES projects(id),
    repo_id    uuid NOT NULL REFERENCES github_repos(id),
    sha        text NOT NULL CHECK (sha ~ '^[0-9a-f]{40}$'),
    revoked_by uuid NOT NULL REFERENCES principals(id),
    revoked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, repo_id, sha)
);

ALTER TABLE project_setting_events DROP CONSTRAINT project_setting_events_kind_check;
ALTER TABLE project_setting_events ADD CONSTRAINT project_setting_events_kind_check
    CHECK (kind IN ('tabs_updated', 'tabs_reset', 'repo_linked', 'repo_unlinked',
                    'pr_hidden', 'pr_unhidden', 'github_content_revoked',
                    'triage_settings_updated', 'wiki_start_page',
                    'commit_hidden', 'commit_unhidden'));
