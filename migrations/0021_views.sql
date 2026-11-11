-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.2 (§2.3): saved views (ViewDef) and their history, and the
-- project tabs board, triage and view.
--
-- A view stores how to query, sort and draw; it always runs with the
-- opener's visibility (K3). Personal views belong to their owner; project
-- views are project-public content (R-VD2, R-VD3). Limits (R-V4) and the
-- tab references (R-V1, R-V2) are checked by the application.

CREATE TABLE view_defs (
    id             uuid PRIMARY KEY DEFAULT uuidv7(),
    scope          text NOT NULL CHECK (scope IN ('personal', 'project')),
    -- personal: the owner; project: who created it (not an owner).
    owner_id       uuid NOT NULL REFERENCES principals(id),
    -- project: required; personal: optional (NULL = across projects).
    project_id     uuid REFERENCES projects(id),
    kind           text NOT NULL CHECK (kind IN ('list', 'board')),
    name           text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    -- NFC + casefold + trimmed, as versions.name_key.
    name_key       text NOT NULL,
    def            jsonb NOT NULL CHECK (jsonb_typeof(def) = 'object' AND octet_length(def::text) <= 32768),
    def_schema_ver smallint NOT NULL DEFAULT 1,
    lock_version   integer NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    updated_by     uuid REFERENCES principals(id),
    deleted_at     timestamptz,
    -- D12: the name and definition replaced by a redaction marker.
    redacted_at    timestamptz,
    CHECK (scope = 'personal' OR project_id IS NOT NULL)
);
CREATE UNIQUE INDEX view_defs_personal_name ON view_defs (owner_id, name_key)
    WHERE scope = 'personal' AND deleted_at IS NULL;
CREATE UNIQUE INDEX view_defs_project_name ON view_defs (project_id, name_key)
    WHERE scope = 'project' AND deleted_at IS NULL;
CREATE INDEX view_defs_project_idx ON view_defs (project_id) WHERE deleted_at IS NULL;
CREATE INDEX view_defs_owner_idx ON view_defs (owner_id) WHERE scope = 'personal' AND deleted_at IS NULL;

-- Agents cannot save views in M1 (B-4): the owner is a user.
CREATE FUNCTION view_defs_owner_check() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT principal_kind_is(NEW.owner_id, ARRAY['user']) THEN
        RAISE EXCEPTION 'view_defs.owner_id must be a user' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER view_defs_owner_check BEFORE INSERT OR UPDATE OF owner_id ON view_defs
    FOR EACH ROW EXECUTE FUNCTION view_defs_owner_check();

CREATE TABLE view_def_events (
    id                   uuid PRIMARY KEY DEFAULT uuidv7(),
    view_id              uuid NOT NULL REFERENCES view_defs(id),
    at                   timestamptz NOT NULL DEFAULT now(),
    kind                 text NOT NULL CHECK (kind IN ('created', 'updated', 'deleted', 'restored')),
    actor_id             uuid NOT NULL REFERENCES principals(id),
    operation_request_id uuid NOT NULL REFERENCES operation_requests(id),
    -- {"before": {name, def} | null, "after": {name, def} | null}
    payload              jsonb NOT NULL,
    redaction_id         uuid
);
CREATE INDEX view_def_events_idx ON view_def_events (view_id, at DESC, id DESC);
-- As 0017's project_setting_event_redactions: never the original text.
CREATE TABLE view_def_event_redactions (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    event_id    uuid NOT NULL UNIQUE REFERENCES view_def_events(id),
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now(),
    reason_code text NOT NULL,
    reason      text NOT NULL
);
ALTER TABLE view_def_events ADD FOREIGN KEY (redaction_id) REFERENCES view_def_event_redactions(id);

-- Project tabs (02 §18): the built-ins board and triage go last (R-T1),
-- triage hidden (R-V3). project_tabs::BUILTIN lists the same seven.
CREATE OR REPLACE FUNCTION project_tabs_default() RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
    SELECT '[{"id": "timeline", "kind": "builtin", "title": "時程", "hidden": false},
             {"id": "milestones", "kind": "builtin", "title": "里程碑", "hidden": false},
             {"id": "issues", "kind": "builtin", "title": "議題", "hidden": false},
             {"id": "dev", "kind": "builtin", "title": "開發（GitHub）", "hidden": false},
             {"id": "plan", "kind": "builtin", "title": "規劃（心智圖）", "hidden": false},
             {"id": "board", "kind": "builtin", "title": "看板", "hidden": false},
             {"id": "triage", "kind": "builtin", "title": "Triage", "hidden": true}]'::jsonb
$$;

-- Widen the CHECK before the UPDATE: a row with 15 tabs (5 built-ins and
-- 10 custom) has 17 afterwards. 0015's CHECK is inline and unnamed, so
-- PostgreSQL named it project_tab_settings_tabs_check. lock_version moves
-- on, so a PUT based on the old list is a conflict carrying the new one
-- (not builtin_missing).
ALTER TABLE project_tab_settings DROP CONSTRAINT project_tab_settings_tabs_check;
UPDATE project_tab_settings SET tabs = tabs
    || '[{"id": "board", "kind": "builtin", "title": "看板", "hidden": false},
         {"id": "triage", "kind": "builtin", "title": "Triage", "hidden": true}]'::jsonb,
    lock_version = lock_version + 1
    WHERE NOT tabs @> '[{"id": "board"}]'::jsonb;
ALTER TABLE project_tab_settings ADD CONSTRAINT project_tab_settings_tabs_check
    CHECK (jsonb_typeof(tabs) = 'array' AND jsonb_array_length(tabs) BETWEEN 7 AND 17);
-- Tabs of kind 'view': {id: 'v_' + 10 characters, kind: 'view', title,
-- hidden, view_id}; the built-in board may carry view_id (a project board
-- view). Custom tabs (query + link + view) stay at most 10 (R-T2).
