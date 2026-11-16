-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 06 §2.4 (A2 (a)): attachments. The design numbered this 0035;
-- 0035 went to the agents' wiki writes (M2.3b), so it is 0036 and the Git
-- migrations move up by one (§2.1: merge order = number order).
--
-- Files live under $OXLEDGER_FILES_DIR/<project_id>/<blob id first 2>/<blob id>;
-- a blob row is one generation of a content in one project (R-A12: the
-- same content uploaded after a purge is a new row and a new file).

CREATE TABLE attachment_blobs (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    sha256       bytea NOT NULL CHECK (octet_length(sha256) = 32),
    size         bigint NOT NULL CHECK (size BETWEEN 1 AND 104857600),
    -- R-A3: from the file's first bytes only.
    sniffed_type text NOT NULL CHECK (sniffed_type IN
      ('image/png', 'image/jpeg', 'image/gif', 'image/webp', 'application/octet-stream')),
    state        text NOT NULL DEFAULT 'present' CHECK (state IN ('present', 'purged')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    purged_at    timestamptz,
    purge_reason text CHECK (purge_reason IN ('gc', 'redacted')),
    CHECK ((state = 'purged') = (purged_at IS NOT NULL)),
    CHECK ((state = 'purged') = (purge_reason IS NOT NULL)),
    UNIQUE (project_id, id)
);
-- R-A4: one present generation per content and project (never across projects).
CREATE UNIQUE INDEX attachment_blobs_live_uq ON attachment_blobs (project_id, sha256)
    WHERE state = 'present';

-- R-A1: an upload waits up to 24 hours to be attached, then goes.
CREATE TABLE uploads (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id  uuid NOT NULL REFERENCES projects(id),
    uploader_id uuid NOT NULL REFERENCES principals(id),
    blob_id     uuid NOT NULL,
    filename    text NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255
                                     AND filename !~ '[[:cntrl:]]'),
    size        bigint NOT NULL CHECK (size >= 1),
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    FOREIGN KEY (project_id, blob_id) REFERENCES attachment_blobs (project_id, id)
);
CREATE INDEX uploads_uploader_idx ON uploads (uploader_id) WHERE consumed_at IS NULL;
CREATE INDEX uploads_blob_idx ON uploads (blob_id);
CREATE INDEX uploads_expires_idx ON uploads (expires_at) WHERE consumed_at IS NULL;
CREATE TRIGGER uploads_uploader_guard BEFORE INSERT OR UPDATE OF uploader_id ON uploads
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('uploader_id');

-- R-A2: bytes uploaded per person per day (the organization's time zone).
CREATE TABLE upload_daily_usage (
    user_id uuid NOT NULL REFERENCES principals(id),
    day     date NOT NULL,
    bytes   bigint NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    PRIMARY KEY (user_id, day)
);

CREATE TABLE attachments (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    issue_id     uuid REFERENCES issues(id),
    -- Attached to a note: read as the note is (private notes included).
    note_id      uuid,
    wiki_page_id uuid REFERENCES wiki_pages(id),
    -- The page's version when attached (for display).
    wiki_version integer,
    blob_id      uuid NOT NULL,
    filename     text NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255
                                      AND filename !~ '[[:cntrl:]]'),
    size         bigint NOT NULL CHECK (size >= 1),
    -- = the blob's sniffed_type.
    content_type text NOT NULL,
    author_id    uuid NOT NULL REFERENCES principals(id),
    created_execution_id uuid NOT NULL REFERENCES execution_refs(id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz,
    deleted_by_execution_id uuid REFERENCES execution_refs(id),
    -- R-A12: marked on every row of a redacted blob generation; reads look
    -- at the row, never at the blob's state.
    redacted_at  timestamptz,
    redaction_id uuid,
    FOREIGN KEY (project_id, blob_id) REFERENCES attachment_blobs (project_id, id),
    FOREIGN KEY (issue_id, note_id) REFERENCES issue_notes (issue_id, id),
    CHECK (num_nonnulls(issue_id, wiki_page_id) = 1),
    CHECK (note_id IS NULL OR issue_id IS NOT NULL),
    CHECK ((wiki_page_id IS NULL) = (wiki_version IS NULL)),
    CHECK ((deleted_at IS NULL) = (deleted_by_execution_id IS NULL)),
    CHECK ((redacted_at IS NULL) = (redaction_id IS NULL))
);
CREATE INDEX attachments_issue_idx ON attachments (issue_id) WHERE issue_id IS NOT NULL;
CREATE INDEX attachments_wiki_idx ON attachments (wiki_page_id) WHERE wiki_page_id IS NOT NULL;
CREATE INDEX attachments_blob_idx ON attachments (blob_id);
CREATE TRIGGER attachments_author_guard BEFORE INSERT OR UPDATE OF author_id ON attachments
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('author_id');

-- The union of 0020's list and the attachment events (R-A7; wiki_events
-- has them since 0034). Payloads carry the attachment id and file name.
ALTER TABLE issue_events DROP CONSTRAINT issue_events_kind_check,
    ADD CONSTRAINT issue_events_kind_check CHECK (kind IN
      ('created', 'fields_changed', 'status_changed', 'note_added', 'duplicate_added',
       'note_retracted', 'duplicate_removed', 'triage_changed',
       'attachment_added', 'attachment_removed'));
