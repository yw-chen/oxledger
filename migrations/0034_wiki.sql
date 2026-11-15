-- SPDX-License-Identifier: GPL-2.0-or-later
-- M2.2 (design 06 §2.3, §3.5): the project wiki. Planned there as 0033;
-- renumbered because 0033 is the P3 backend gaps.
--
--   * wiki_settings: one row per project (backfilled here, and created with
--     every new project by a trigger, as 0015's project tab settings). The
--     row is the lock that serializes tree changes and the page limit
--     (R-W10, R-W15).
--   * wiki_pages, wiki_page_versions (content history), wiki_redirects (old
--     titles), wiki_events (everything but content), wiki_redactions (D12),
--     execution_wiki_changes (undo, R-W9; a D12 derived copy of titles).
--   * wiki_title_key(): the uniqueness key of a title (R-W2, C13): Unicode
--     runs of white space and '_' become one '_' (a URL writes a space as
--     '_', so 'a b', 'a_b' and 'a__b' are one title, as in Redmine) and the
--     result is NFKC_Casefold, through
--     the builtin pg_unicode_fast collation (the database's own collation is
--     C, whose casefold() does not fold 'ß'; decisions.tsv M2.0 C13).
--   * The fixed lists (design 05 §2.1 union rule, values verbatim from 0022,
--     0023 and 0029): wiki_put and wiki_move join the agent op lists (agents
--     get them in M2.3; until then the engine refuses every wiki op from an
--     agent), agent_write_log gets wiki_write, project_setting_events gets
--     wiki_start_page.
--   * The built-in project tab `wiki` (R-T1: appended), hidden by the web
--     app when the reader cannot read the wiki (as `dev`).
--
-- The wiki module is not turned on for existing projects (B-9), no role,
-- permission or membership changes: security_epoch does not move. Wiki
-- writes reach the outbox as topic 'wiki_page' (R-W11); no consumer reads
-- that topic before M2.3 (search and notify read 'issue' only).

-- ---------------------------------------------------------------- title key (R-W2, C13)

CREATE FUNCTION wiki_title_key(title text) RETURNS text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT casefold(regexp_replace(normalize(title, NFKC),
                                   '[\s_\u0085   -     　]+',
                                   '_', 'g') COLLATE pg_unicode_fast)
$$;

-- ---------------------------------------------------------------- settings

CREATE TABLE wiki_settings (
    project_id    uuid PRIMARY KEY REFERENCES projects(id),
    start_page_id uuid,                       -- NULL: the page titled 'Wiki', else the index (R-W13)
    page_count    integer NOT NULL DEFAULT 0 CHECK (page_count BETWEEN 0 AND 5000),   -- live pages
    lock_version  bigint NOT NULL DEFAULT 0,  -- moves with PUT /wiki/settings and a start page deletion
    updated_by    uuid REFERENCES principals(id),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
INSERT INTO wiki_settings (project_id) SELECT id FROM projects;
CREATE FUNCTION wiki_settings_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO wiki_settings (project_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER wiki_settings_row AFTER INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION wiki_settings_row();

-- ---------------------------------------------------------------- pages

CREATE TABLE wiki_pages (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id      uuid NOT NULL REFERENCES projects(id),
    title           text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    title_key       text NOT NULL,            -- wiki_title_key(title), written by the engine
    parent_id       uuid,
    current_version integer NOT NULL CHECK (current_version BETWEEN 1 AND 10000),
    lock_version    bigint NOT NULL DEFAULT 0,
    protected       boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    deleted_by_execution_id uuid REFERENCES execution_refs(id),
    CHECK ((deleted_at IS NULL) = (deleted_by_execution_id IS NULL)),
    CHECK (parent_id IS DISTINCT FROM id),
    CHECK (title_key = wiki_title_key(title)),
    UNIQUE (project_id, id)                   -- for the same-project composite FKs
);
ALTER TABLE wiki_pages ADD FOREIGN KEY (project_id, parent_id) REFERENCES wiki_pages (project_id, id);
CREATE UNIQUE INDEX wiki_pages_title_uq ON wiki_pages (project_id, title_key) WHERE deleted_at IS NULL;
CREATE INDEX wiki_pages_tree_idx ON wiki_pages (project_id, parent_id) WHERE deleted_at IS NULL;
CREATE INDEX wiki_pages_deleted_idx ON wiki_pages (project_id, deleted_at DESC) WHERE deleted_at IS NOT NULL;
ALTER TABLE wiki_settings ADD FOREIGN KEY (project_id, start_page_id) REFERENCES wiki_pages (project_id, id);

-- R-W11: every page write (a new version moves current_version) is a
-- message for the wiki index (M2.3).
CREATE FUNCTION wiki_pages_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO outbox (topic, entity_id) VALUES ('wiki_page', NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER wiki_pages_outbox AFTER INSERT OR UPDATE ON wiki_pages
    FOR EACH ROW EXECUTE FUNCTION wiki_pages_enqueue();

-- The provenance of versions and events: issue_events' columns and CHECK
-- (0008) for the three origins that write content through the engine.

CREATE TABLE wiki_page_versions (             -- append-only, except a D12 redaction
    page_id      uuid NOT NULL REFERENCES wiki_pages(id),
    version      integer NOT NULL CHECK (version BETWEEN 1 AND 10000),
    body         text NOT NULL CHECK (octet_length(body) <= 524288),
    body_format  text NOT NULL DEFAULT 'markdown' CHECK (body_format = 'markdown'),
    comment      text NOT NULL DEFAULT '' CHECK (char_length(comment) <= 255),
    origin               text NOT NULL CHECK (origin IN ('human_direct', 'agent_direct', 'external_agent')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    actor_kind           text NOT NULL CHECK (actor_kind IN ('user', 'agent')),
    proposer_id          uuid REFERENCES principals(id),
    approver_id          uuid REFERENCES principals(id),
    on_behalf_of_id      uuid REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    agent_client_id      uuid REFERENCES agent_clients(id),
    operation_request_id uuid REFERENCES operation_requests(id),
    command_execution_id uuid REFERENCES command_executions(id),
    changeset_id         uuid,
    changeset_version    integer,
    execution_id         uuid REFERENCES execution_refs(id),
    op_id                uuid,
    created_at   timestamptz NOT NULL DEFAULT now(),
    redaction_id uuid,                        -- set: body and comment are '[已遮蔽]'
    PRIMARY KEY (page_id, version),
    CHECK (
      (origin = 'human_direct' AND actor_kind = 'user'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND proposer_id IS NULL
         AND approver_id IS NULL AND on_behalf_of_id IS NULL AND delegation_id IS NULL)
   OR (origin = 'agent_direct' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND approver_id IS NULL)
   OR (origin = 'external_agent' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND changeset_id IS NOT NULL
         AND changeset_version IS NOT NULL AND execution_id IS NOT NULL AND op_id IS NOT NULL
         AND proposer_id IS NOT NULL AND approver_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND command_execution_id IS NULL))
);
CREATE INDEX wiki_page_versions_command_idx ON wiki_page_versions (command_execution_id)
    WHERE command_execution_id IS NOT NULL;

CREATE TABLE wiki_redirects (                 -- old titles after a rename (R-W4)
    project_id uuid NOT NULL REFERENCES projects(id),
    title_key  text NOT NULL,
    page_id    uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, title_key),
    FOREIGN KEY (project_id, page_id) REFERENCES wiki_pages (project_id, id)
);
CREATE INDEX wiki_redirects_page_idx ON wiki_redirects (page_id);

CREATE TABLE wiki_events (                    -- changes other than content; titles in payload (D12)
    id      uuid PRIMARY KEY DEFAULT uuidv7(),
    page_id uuid NOT NULL REFERENCES wiki_pages(id),
    seq     bigint NOT NULL CHECK (seq >= 1),
    kind    text NOT NULL CHECK (kind IN ('created', 'renamed', 'moved', 'deleted', 'restored',
                                          'protected', 'unprotected', 'attachment_added', 'attachment_removed')),
    payload jsonb NOT NULL,                   -- created {title, parent_id}; renamed {from, to};
                                              -- moved {from_parent, to_parent}; deleted {children}
    origin               text NOT NULL CHECK (origin IN ('human_direct', 'agent_direct', 'external_agent')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    actor_kind           text NOT NULL CHECK (actor_kind IN ('user', 'agent')),
    proposer_id          uuid REFERENCES principals(id),
    approver_id          uuid REFERENCES principals(id),
    on_behalf_of_id      uuid REFERENCES principals(id),
    delegation_id        uuid REFERENCES delegations(id),
    agent_client_id      uuid REFERENCES agent_clients(id),
    operation_request_id uuid REFERENCES operation_requests(id),
    command_execution_id uuid REFERENCES command_executions(id),
    changeset_id         uuid,
    changeset_version    integer,
    execution_id         uuid REFERENCES execution_refs(id),
    op_id                uuid,
    at      timestamptz NOT NULL DEFAULT now(),
    redaction_id uuid,
    UNIQUE (page_id, seq),
    CHECK (
      (origin = 'human_direct' AND actor_kind = 'user'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND proposer_id IS NULL
         AND approver_id IS NULL AND on_behalf_of_id IS NULL AND delegation_id IS NULL)
   OR (origin = 'agent_direct' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND command_execution_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND changeset_id IS NULL AND execution_id IS NULL AND approver_id IS NULL)
   OR (origin = 'external_agent' AND actor_kind = 'agent'
         AND operation_request_id IS NOT NULL AND changeset_id IS NOT NULL
         AND changeset_version IS NOT NULL AND execution_id IS NOT NULL AND op_id IS NOT NULL
         AND proposer_id IS NOT NULL AND approver_id IS NOT NULL
         AND on_behalf_of_id IS NOT NULL AND delegation_id IS NOT NULL
         AND command_execution_id IS NULL))
);

CREATE TABLE wiki_redactions (                -- who, when, why; never the original text
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    page_id     uuid NOT NULL REFERENCES wiki_pages(id),
    target      text NOT NULL CHECK (target IN ('version', 'event', 'title')),   -- 'version': body and comment
    version     integer,
    event_id    uuid REFERENCES wiki_events(id),
    reason_code text NOT NULL,
    reason      text NOT NULL,
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((target = 'version') = (version IS NOT NULL)),
    CHECK ((target = 'event') = (event_id IS NOT NULL))
);
ALTER TABLE wiki_page_versions ADD FOREIGN KEY (redaction_id) REFERENCES wiki_redactions(id);
ALTER TABLE wiki_events ADD FOREIGN KEY (redaction_id) REFERENCES wiki_redactions(id);

-- Undo (R-W9, 30 days, U18): what one execution did to each page it wrote,
-- a wiki_delete with children: "reparent" one row per moved child too.
--   * a created page: before_version, before_deleted, before_protected and
--     before_lock_version are NULL;
--   * titles are kept only when the title changed (a D12 derived copy:
--     redaction::derived replaces them with the marker);
--   * parents only when parent_changed (NULL is the top level);
--   * versions, the deleted and protected flags and lock versions always.
-- "Later" writes of a page are its rows with a larger after_lock_version.
CREATE TABLE execution_wiki_changes (
    execution_id        uuid NOT NULL REFERENCES execution_refs(id),
    page_id             uuid NOT NULL REFERENCES wiki_pages(id),
    before_version      integer,
    after_version       integer NOT NULL,
    before_title        text,
    after_title         text,
    parent_changed      boolean NOT NULL DEFAULT false,
    before_parent       uuid,
    after_parent        uuid,
    before_deleted      boolean,
    after_deleted       boolean NOT NULL,
    before_protected    boolean,
    after_protected     boolean NOT NULL,
    before_lock_version bigint,
    after_lock_version  bigint NOT NULL,
    reverted_by         uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, page_id)
);
CREATE INDEX execution_wiki_changes_page_idx ON execution_wiki_changes (page_id, after_lock_version);

-- ---------------------------------------------------------------- fixed lists (union rule)

ALTER TABLE delegation_command_policies DROP CONSTRAINT delegation_command_policies_command_kind_check;
ALTER TABLE delegation_command_policies ADD CONSTRAINT delegation_command_policies_command_kind_check
    CHECK (command_kind IN (
        'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
        'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry',
        'wiki_put', 'wiki_move'));
ALTER TABLE delegations DROP CONSTRAINT delegations_proposal_ops_check;
ALTER TABLE delegations ADD CONSTRAINT delegations_proposal_ops_check CHECK (proposal_ops <@ ARRAY[
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry',
    'wiki_put', 'wiki_move']);
ALTER TABLE changeset_version_ops DROP CONSTRAINT changeset_version_ops_kind_check;
ALTER TABLE changeset_version_ops ADD CONSTRAINT changeset_version_ops_kind_check CHECK (kind IN (
    'create_issue', 'update_fields', 'transition', 'add_note', 'duplicate_of',
    'create_version', 'update_version', 'triage_decide', 'log_time', 'update_time_entry',
    'wiki_put', 'wiki_move'));
ALTER TABLE agent_write_log DROP CONSTRAINT agent_write_log_kind_check;
ALTER TABLE agent_write_log ADD CONSTRAINT agent_write_log_kind_check
    CHECK (kind IN ('direct_write', 'issue_create', 'issue_write', 'propose', 'audience_blocked',
                    'version_create', 'version_write', 'validate', 'time_write',
                    'chain_review', 'time_taint_review', 'wiki_write'));
ALTER TABLE project_setting_events DROP CONSTRAINT project_setting_events_kind_check;
ALTER TABLE project_setting_events ADD CONSTRAINT project_setting_events_kind_check
    CHECK (kind IN ('tabs_updated', 'tabs_reset', 'repo_linked', 'repo_unlinked',
                    'pr_hidden', 'pr_unhidden', 'github_content_revoked',
                    'triage_settings_updated', 'wiki_start_page'));

-- ---------------------------------------------------------------- project tab (§2.3)

CREATE OR REPLACE FUNCTION project_tabs_default() RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
    SELECT '[{"id": "timeline", "kind": "builtin", "title": "時程", "hidden": false},
             {"id": "milestones", "kind": "builtin", "title": "里程碑", "hidden": false},
             {"id": "issues", "kind": "builtin", "title": "議題", "hidden": false},
             {"id": "dev", "kind": "builtin", "title": "開發（GitHub）", "hidden": false},
             {"id": "plan", "kind": "builtin", "title": "規劃（心智圖）", "hidden": false},
             {"id": "board", "kind": "builtin", "title": "看板", "hidden": false},
             {"id": "triage", "kind": "builtin", "title": "Triage", "hidden": true},
             {"id": "wiki", "kind": "builtin", "title": "Wiki", "hidden": false}]'::jsonb
$$;
-- Widen the CHECK first (8 built-ins + 10 custom), then append the tab;
-- lock_version moves on, so a PUT based on the old list is a conflict that
-- carries the new one (as 0021).
ALTER TABLE project_tab_settings DROP CONSTRAINT project_tab_settings_tabs_check;
UPDATE project_tab_settings
    SET tabs = tabs || '[{"id": "wiki", "kind": "builtin", "title": "Wiki", "hidden": false}]'::jsonb,
        lock_version = lock_version + 1
    WHERE NOT tabs @> '[{"id": "wiki"}]'::jsonb;
ALTER TABLE project_tab_settings ADD CONSTRAINT project_tab_settings_tabs_check
    CHECK (jsonb_typeof(tabs) = 'array' AND jsonb_array_length(tabs) BETWEEN 8 AND 18);
