-- M2.7 / M2.8 (design 06 §2.7): CSV export's audit trail, and CSV import.
--
-- Nothing here touches roles, permissions, members or projects: the
-- security epoch of every project stays as it is.

-- ---------------------------------------------------------------- import (M2.8)

CREATE TABLE imports (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    kind         text NOT NULL CHECK (kind IN ('issues', 'time_entries')),
    owner_id     uuid NOT NULL REFERENCES principals(id),
    state        text NOT NULL CHECK (state IN ('uploaded', 'ready', 'validating', 'validated',
                                                'running', 'done', 'canceled', 'failed', 'expired')),
    -- The file as uploaded, at most 10 MB; NULL 24 hours after the import
    -- ended, when it is deleted, and when the project is redacted (D12).
    file         bytea,
    file_sha256  bytea NOT NULL,
    encoding     text NOT NULL,                 -- 'utf-8' | 'utf-16le' | 'utf-16be' | 'big5'
    encoding_guessed boolean NOT NULL,
    delimiter    text NOT NULL CHECK (delimiter IN (',', ';', E'\t')),
    row_count    integer NOT NULL CHECK (row_count BETWEEN 1 AND 5000),
    -- Mapping, value mapping, date format, options: never file content.
    settings     jsonb NOT NULL DEFAULT '{}',
    lock_version bigint NOT NULL DEFAULT 0,
    operation_request_id uuid REFERENCES operation_requests(id),   -- the run's idempotency
    created_at   timestamptz NOT NULL DEFAULT now(),
    started_at   timestamptz,
    finished_at  timestamptz,
    purged_at    timestamptz
);
CREATE INDEX imports_owner_idx ON imports (owner_id, created_at DESC);
CREATE TRIGGER imports_owner_guard BEFORE INSERT OR UPDATE OF owner_id ON imports
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('owner_id');

-- Results only: no cell of the file is kept here.
CREATE TABLE import_rows (
    import_id   uuid NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    row_no      integer NOT NULL CHECK (row_no >= 1),   -- the data row in the file (the header is not counted)
    state       text NOT NULL CHECK (state IN ('pending', 'ok', 'error', 'skipped')),
    error_code  text,
    error_field text,
    issue_id    uuid REFERENCES issues(id),
    time_entry_id uuid REFERENCES time_entries(id),
    execution_id  uuid REFERENCES execution_refs(id),
    PRIMARY KEY (import_id, row_no)
);
ALTER TABLE command_executions ADD FOREIGN KEY (import_id) REFERENCES imports(id);

-- ---------------------------------------------------------------- export (M2.7)

-- U42 / R-E3: a timestamp as ISO 8601 in the organisation's time zone, with
-- its offset (`2026-11-17T09:30:00+08:00`).
CREATE FUNCTION org_iso(ts timestamptz) RETURNS text LANGUAGE sql STABLE AS $$
    SELECT to_char(ts AT TIME ZONE o.timezone, 'YYYY-MM-DD"T"HH24:MI:SS')
           || CASE WHEN x.off < 0 THEN '-' ELSE '+' END
           || lpad((abs(x.off) / 3600)::text, 2, '0') || ':' || lpad((abs(x.off) % 3600 / 60)::text, 2, '0')
    FROM org_settings o,
         LATERAL (SELECT (extract(epoch FROM ((ts AT TIME ZONE o.timezone) - (ts AT TIME ZONE 'UTC'))))::int AS off) x
$$;

-- Who exported which range: never the content, never the conditions.
CREATE TABLE export_events (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    actor_id    uuid NOT NULL REFERENCES principals(id),
    kind        text NOT NULL CHECK (kind IN ('issues', 'time_entries', 'wiki_page')),
    project_id  uuid REFERENCES projects(id),   -- NULL: across projects
    view_id     uuid,
    -- HMAC-SHA256 of the normalized conditions (key: server_secrets
    -- 'export_filter'). Not a plain hash: conditions are few enough to
    -- guess from one.
    filter_hash bytea,
    columns     text[] NOT NULL DEFAULT '{}',   -- column keys (cf:<id>), never values
    row_count   integer NOT NULL,
    bytes       bigint NOT NULL DEFAULT 0,      -- what was sent (R-E6)
    truncated   boolean NOT NULL DEFAULT false, -- cut at the byte limit
    at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX export_events_at_idx ON export_events (at);
