-- oxledger M0 schema. SPDX-License-Identifier: GPL-2.0-or-later
-- Design: docs/research/01-redmine-rust-rewrite.md §5.5 (decisions D5 single org, D8 PG18 uuidv7, D9 project-prefixed keys, D11 xid8 outbox)

-- Principals: users and groups share one id space so an issue can be assigned to either (Redmine semantics).
CREATE TABLE principals (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    kind        text NOT NULL CHECK (kind IN ('user', 'group')),
    login       text UNIQUE,                       -- users only
    name        text NOT NULL,
    is_admin    boolean NOT NULL DEFAULT false,
    status      text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'locked')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (kind = 'user' OR (login IS NULL AND NOT is_admin))
);

CREATE TABLE group_members (
    group_id    uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);

-- API keys are stored hashed (sha256 hex); the raw key is shown once.
CREATE TABLE api_keys (
    token_sha256 text PRIMARY KEY,
    principal_id uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    label        text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE projects (
    id            uuid PRIMARY KEY DEFAULT uuidv7(),
    key           text NOT NULL UNIQUE CHECK (key ~ '^[A-Z][A-Z0-9]{1,9}$'),
    name          text NOT NULL,
    description   text NOT NULL DEFAULT '',
    is_public     boolean NOT NULL DEFAULT false,
    status        text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'closed', 'archived')),
    parent_id     uuid REFERENCES projects(id),
    issue_counter integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE enabled_modules (
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    module      text NOT NULL,
    PRIMARY KEY (project_id, module)
);

-- Roles. builtin = non_member / anonymous are the Redmine built-in roles used for public projects.
CREATE TABLE roles (
    id                uuid PRIMARY KEY DEFAULT uuidv7(),
    name              text NOT NULL UNIQUE,
    builtin           text CHECK (builtin IN ('non_member', 'anonymous')),
    issues_visibility text NOT NULL DEFAULT 'default' CHECK (issues_visibility IN ('all', 'default', 'own')),
    position          integer NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX roles_builtin_uniq ON roles(builtin) WHERE builtin IS NOT NULL;

CREATE TABLE role_permissions (
    role_id      uuid NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission   text NOT NULL,
    all_trackers boolean NOT NULL DEFAULT true,
    PRIMARY KEY (role_id, permission)
);

CREATE TABLE trackers (
    id                uuid PRIMARY KEY DEFAULT uuidv7(),
    name              text NOT NULL UNIQUE,
    default_status_id uuid,
    position          integer NOT NULL DEFAULT 0
);

-- When role_permissions.all_trackers is false, the permission applies only to these trackers.
CREATE TABLE role_permission_trackers (
    role_id     uuid NOT NULL,
    permission  text NOT NULL,
    tracker_id  uuid NOT NULL REFERENCES trackers(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission, tracker_id),
    FOREIGN KEY (role_id, permission) REFERENCES role_permissions(role_id, permission) ON DELETE CASCADE
);

CREATE TABLE members (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    principal_id uuid NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    UNIQUE (project_id, principal_id)
);

CREATE TABLE member_roles (
    member_id uuid NOT NULL REFERENCES members(id) ON DELETE CASCADE,
    role_id   uuid NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (member_id, role_id)
);

CREATE TABLE issue_statuses (
    id        uuid PRIMARY KEY DEFAULT uuidv7(),
    name      text NOT NULL UNIQUE,
    category  text NOT NULL CHECK (category IN ('todo', 'doing', 'done')),
    position  integer NOT NULL DEFAULT 0
);

ALTER TABLE trackers ADD FOREIGN KEY (default_status_id) REFERENCES issue_statuses(id);

CREATE TABLE project_trackers (
    project_id uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    tracker_id uuid NOT NULL REFERENCES trackers(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, tracker_id)
);

CREATE TABLE workflow_transitions (
    tracker_id    uuid NOT NULL REFERENCES trackers(id) ON DELETE CASCADE,
    role_id       uuid NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    old_status_id uuid NOT NULL REFERENCES issue_statuses(id) ON DELETE CASCADE,
    new_status_id uuid NOT NULL REFERENCES issue_statuses(id) ON DELETE CASCADE,
    PRIMARY KEY (tracker_id, role_id, old_status_id, new_status_id)
);

CREATE TABLE issues (
    id             uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id     uuid NOT NULL REFERENCES projects(id),
    tracker_id     uuid NOT NULL REFERENCES trackers(id),
    status_id      uuid NOT NULL REFERENCES issue_statuses(id),
    subject        text NOT NULL CHECK (length(subject) BETWEEN 1 AND 255),
    description    text NOT NULL DEFAULT '',
    author_id      uuid NOT NULL REFERENCES principals(id),
    assigned_to_id uuid REFERENCES principals(id),
    is_private     boolean NOT NULL DEFAULT false,
    parent_id      uuid REFERENCES issues(id),
    start_date     date,
    due_date       date,
    lock_version   integer NOT NULL DEFAULT 0,
    archived_at    timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
-- Visibility columns, matching the search index fast fields (§5.5).
CREATE INDEX issues_visibility_idx ON issues (project_id, tracker_id, is_private, author_id, assigned_to_id);
CREATE INDEX issues_parent_idx ON issues (parent_id) WHERE parent_id IS NOT NULL;

-- D9: human keys like AUTH-123. One current key per issue; old keys keep resolving after a move.
CREATE TABLE issue_keys (
    key        text PRIMARY KEY,
    issue_id   uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id uuid NOT NULL REFERENCES projects(id),
    number     integer NOT NULL,
    is_current boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX issue_keys_current_uniq ON issue_keys(issue_id) WHERE is_current;

-- Ontology aliases (SKOS pref/alt labels) used by `resolve` and query expansion.
CREATE TABLE aliases (
    entity_kind text NOT NULL CHECK (entity_kind IN ('tracker', 'status', 'principal', 'project', 'concept')),
    entity_id   uuid NOT NULL,
    label       text NOT NULL,
    label_kind  text NOT NULL DEFAULT 'alt' CHECK (label_kind IN ('pref', 'alt', 'hidden')),
    lang        text NOT NULL DEFAULT '',
    PRIMARY KEY (entity_kind, entity_id, label)
);
CREATE INDEX aliases_label_idx ON aliases (lower(label));

-- D11: transactional outbox ordered by xid8 snapshot, consumed at-least-once.
CREATE TABLE outbox (
    id         bigserial PRIMARY KEY,
    xid        xid8 NOT NULL DEFAULT pg_current_xact_id(),
    topic      text NOT NULL,
    entity_id  uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX outbox_xid_idx ON outbox (xid, id);

CREATE TABLE outbox_cursors (
    consumer  text PRIMARY KEY,
    last_xid  xid8 NOT NULL DEFAULT '0',
    last_id   bigint NOT NULL DEFAULT 0
);

-- Every issue change enqueues a reindex message in the same transaction.
CREATE FUNCTION issues_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO outbox (topic, entity_id) VALUES ('issue', COALESCE(NEW.id, OLD.id));
    RETURN NULL;
END $$;
CREATE TRIGGER issues_outbox AFTER INSERT OR UPDATE OR DELETE ON issues
    FOR EACH ROW EXECUTE FUNCTION issues_enqueue();
